//! The "Updates" page.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Once;

use adw::prelude::*;
use gtk::{gio, glib};
use hyprdeck_core::{cmd, rt, ui, ui::Ctx};

use crate::check::{self, PROJECTS, StatusKind, Tooling, Tracked};
use crate::github::Release;
use crate::parse::{CrateUpdate, Update};
use crate::selfstate::{self, Checked, Job, Origin, SelfState};
use crate::selfupdate::{self, Blocker, Channel, Mode, Policy, SelfCheck, SourceCheck};
use crate::state::{self, State};
use crate::{apply, dialog};

/// The page re-checks on show when the last check is older than this.
const STALE_ON_SHOW: i64 = 15 * 60;
/// Same for hyprdeck's own update check (a source check runs `git fetch`).
const SELF_STALE_ON_SHOW: i64 = 5 * 60;

const CSS: &str = "
.hd-badge { font-size: smaller; font-weight: bold; padding: 1px 8px; border-radius: 999px;
            background: alpha(currentColor, 0.1); }
.hd-badge.important { background: alpha(var(--accent-bg-color), 0.25); color: var(--accent-color); }
.hd-badge.aur { background: alpha(var(--warning-bg-color), 0.2); color: var(--warning-color); }
.hd-badge.warning { background: alpha(var(--warning-bg-color), 0.25); color: var(--warning-color); }
.hd-badge.critical { background: alpha(var(--error-bg-color), 0.25); color: var(--error-color); }
.hd-badge.ok { background: alpha(var(--success-bg-color), 0.2); color: var(--success-color); }
";

pub(crate) fn load_css() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Some(display) = gtk::gdk::Display::default() else {
            return;
        };
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    });
}

struct Page {
    ctx: Ctx,
    system: gtk::Box,
    tracked: gtk::Box,
    hyprdeck: gtk::Box,
    /// "Restart to finish" after an installed update.
    banner: adw::Banner,
    /// Install mode, resolved when the page is first shown.
    mode: RefCell<Option<Mode>>,
    /// Source mode: latest crate-update lookup (`None` while running).
    crates: RefCell<Option<Result<Vec<CrateUpdate>, String>>>,
    /// Keeps the build log expander open across re-renders.
    log_expanded: Cell<bool>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    load_css();
    let (scroller, content) = ui::page_scaffold();
    let section = || gtk::Box::new(gtk::Orientation::Vertical, 24);
    let banner = adw::Banner::builder()
        .use_markup(false)
        .button_label("Restart now")
        .build();
    banner.connect_button_clicked(|_| selfstate::restart_now());
    let page = Rc::new(Page {
        ctx: ctx.clone(),
        system: section(),
        tracked: section(),
        hyprdeck: section(),
        banner,
        mode: RefCell::new(None),
        crates: RefCell::new(None),
        log_expanded: Cell::new(false),
    });
    content.append(&page.banner);
    content.append(&page.system);
    content.append(&page.tracked);
    content.append(&page.hyprdeck);
    content.append(&settings_group(ctx));

    page.render(&state::current());

    // Re-render whenever a check starts or finishes (from any trigger).
    let weak = Rc::downgrade(&page);
    let mut rx = state::subscribe();
    glib::spawn_future_local(async move {
        while rx.changed().await.is_ok() {
            let Some(page) = weak.upgrade() else { break };
            let st = rx.borrow_and_update().clone();
            page.render(&st);
        }
    });
    let weak = Rc::downgrade(&page);
    let mut rx = selfstate::subscribe();
    glib::spawn_future_local(async move {
        while rx.changed().await.is_ok() {
            let Some(page) = weak.upgrade() else { break };
            let st = rx.borrow_and_update().clone();
            page.render_hyprdeck(&st);
        }
    });
    // Re-render when an in-app update starts or ends ("Update everything" ↔ "Show progress").
    let weak = Rc::downgrade(&page);
    let mut rx = apply::subscribe();
    glib::spawn_future_local(async move {
        let mut active = rx.borrow().active;
        while rx.changed().await.is_ok() {
            let Some(page) = weak.upgrade() else { break };
            let now_active = rx.borrow_and_update().active;
            if now_active != active {
                active = now_active;
                page.render(&state::current());
            }
        }
    });
    // Tray entry / notification action: open the update dialog.
    let ctx2 = ctx.clone();
    let mut rx = state::review_requests();
    glib::spawn_future_local(async move {
        loop {
            // Let the window present itself before the dialog attaches to it.
            glib::timeout_future(std::time::Duration::from_millis(100)).await;
            if state::take_review_request() {
                dialog::open(&ctx2);
            }
            if rx.changed().await.is_err() {
                break;
            }
        }
    });

    // The page widget owns the page state through this handler; everything else holds weak refs.
    ui::on_shown(&scroller, move || {
        let st = state::current();
        let stale = st
            .report
            .as_ref()
            .is_none_or(|r| check::now() - r.checked_at > STALE_ON_SHOW);
        if stale && !st.checking {
            state::trigger(false);
        }
        page.render(&st);
        page.load_hyprdeck();
    });
    scroller.upcast()
}

impl Page {
    fn render(self: &Rc<Self>, st: &State) {
        ui::clear(&self.system);
        self.system.append(&self.system_group(st));
        ui::clear(&self.tracked);
        if let Some(report) = &st.report {
            let now = check::now();
            for t in &report.tracked {
                self.tracked
                    .append(&self.tracked_group(t, &report.tooling, now));
            }
            if report.tracked.is_empty() {
                self.tracked.append(&no_components_group());
            }
        }
    }

    fn system_group(self: &Rc<Self>, st: &State) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title("System packages")
            .build();
        group.set_description(Some(&match &st.report {
            Some(r) => {
                format!(
                    "Last checked {} · {}",
                    local_time(r.checked_at),
                    crate::ago(check::now() - r.checked_at)
                )
            }
            None => "Not checked yet".into(),
        }));

        let check_btn = gtk::Button::builder()
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .build();
        if st.checking {
            let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
            b.append(&adw::Spinner::new());
            b.append(&gtk::Label::new(Some("Checking…")));
            check_btn.set_child(Some(&b));
            check_btn.set_sensitive(false);
        } else {
            check_btn.set_child(Some(
                &adw::ButtonContent::builder()
                    .icon_name("view-refresh-symbolic")
                    .label("Check now")
                    .build(),
            ));
            check_btn.set_tooltip_text(Some(
                "Refresh package lists and upstream releases (no root needed)",
            ));
            check_btn.connect_clicked(|_| state::trigger(false));
        }
        group.set_header_suffix(Some(&check_btn));

        let Some(report) = &st.report else {
            let row = adw::ActionRow::builder()
                .title(if st.checking { "Checking for updates…" } else { "No check has run yet" })
                .subtitle("Repository and AUR updates are checked without root, using a temporary package database")
                .build();
            group.add(&row);
            return group;
        };
        let tooling = &report.tooling;

        if !tooling.pacman {
            let status = adw::StatusPage::builder()
                .icon_name("package-x-generic-symbolic")
                .title("System package updates need an Arch-based distro (pacman)")
                .description(
                    "pacman was not found, so installed packages and their updates can't be listed here. \
                     Upstream releases of desktop components found on your PATH are still shown below.",
                )
                .css_classes(["compact"])
                .build();
            group.add(&status);
            return group;
        }

        if let Some(err) = &report.repo_error {
            let title = if tooling.checkupdates {
                "Repository check failed"
            } else {
                "Repository updates unavailable"
            };
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(title);
            row.set_subtitle(err);
            row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            group.add(&row);
        }
        if let Some(err) = &report.aur_error {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title("AUR check failed");
            row.set_subtitle(err);
            row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            group.add(&row);
        }

        let total = report.total();
        let important = report.important().count();
        let summary = adw::ActionRow::builder()
            .title(match total {
                0 if report.failed() => "No updates found".to_owned(),
                0 => "System is up to date".to_owned(),
                1 => "1 update available".to_owned(),
                n => format!("{n} updates available"),
            })
            .subtitle(format!(
                "{} from repositories · {} from the AUR · {important} important",
                report.repo.len(),
                report.aur.len()
            ))
            .build();
        summary.add_prefix(&if total == 0 {
            icon("object-select-symbolic", "success")
        } else {
            icon("software-update-available-symbolic", "accent")
        });
        if apply::active() {
            let btn = gtk::Button::builder()
                .label("Show progress")
                .valign(gtk::Align::Center)
                .css_classes(["pill"])
                .tooltip_text("A system update is running")
                .build();
            let ctx = self.ctx.clone();
            btn.connect_clicked(move |_| dialog::open(&ctx));
            summary.add_suffix(&btn);
        } else if total > 0 {
            let btn = gtk::Button::builder()
                .label("Update everything")
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action", "pill"])
                .tooltip_text(
                    "Review the updates (news, AUR changes and security checks), then update with one password prompt",
                )
                .build();
            let ctx = self.ctx.clone();
            btn.connect_clicked(move |_| dialog::open(&ctx));
            summary.add_suffix(&btn);
        }
        group.add(&summary);

        for u in report.repo.iter().filter(|u| u.important) {
            group.add(&update_row(u, false));
        }
        for u in report.aur.iter().filter(|u| u.important) {
            group.add(&update_row(u, true));
        }
        for (title, list, aur) in [
            ("All repository updates", &report.repo, false),
            ("All AUR updates", &report.aur, true),
        ] {
            if list.is_empty() {
                continue;
            }
            let exp = adw::ExpanderRow::builder()
                .title(format!("{title} ({})", list.len()))
                .build();
            for u in list {
                exp.add_row(&update_row(u, aur));
            }
            group.add(&exp);
        }
        if !report.not_in_aur.is_empty() {
            let exp = adw::ExpanderRow::builder()
                .title(format!("Not from the AUR ({})", report.not_in_aur.len()))
                .subtitle("Foreign packages the AUR doesn't know; never updated here")
                .build();
            for f in &report.not_in_aur {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&f.name);
                row.set_subtitle(&f.version);
                exp.add_row(&row);
            }
            group.add(&exp);
        }
        group
    }

    fn tracked_group(
        self: &Rc<Self>,
        t: &Tracked,
        tooling: &Tooling,
        now: i64,
    ) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title(glib::markup_escape_text(&t.title))
            .description(glib::markup_escape_text(&format!(
                "Tracking github.com/{}",
                t.github
            )))
            .build();
        if let Some(rel) = &t.upstream {
            group.set_header_suffix(Some(&self.link_button("Release page", &rel.url)));
        }

        let status = t.status(now);
        let status_row = adw::ActionRow::builder().use_markup(false).build();
        status_row.set_title(&status.text);
        status_row.set_title_lines(0);
        status_row.add_prefix(&match status.kind {
            StatusKind::UpToDate => icon("object-select-symbolic", "success"),
            StatusKind::RepoUpdate => icon("software-update-available-symbolic", "accent"),
            StatusKind::BehindUpstream => icon("dialog-information-symbolic", "warning"),
            StatusKind::Git => icon("emblem-system-symbolic", "accent"),
            StatusKind::Unmanaged => icon("dialog-information-symbolic", "dim-label"),
        });
        group.add(&status_row);

        group.add(&match (&t.installed, &t.binary) {
            (Some(i), _) => value_row(
                "Installed",
                &match &i.repo {
                    Some(repo) => format!("Package {} from {repo}", i.package),
                    None => format!("Package {} from the AUR (foreign package)", i.package),
                },
                &i.version,
            ),
            (None, Some(path)) => {
                value_row("Installed", &format!("{path} (not managed by pacman)"), "—")
            }
            (None, None) => value_row("Installed", "Version unknown", "—"),
        });

        if tooling.pacman {
            group.add(&match &t.repo {
                Some(r) => value_row(
                    "Newest in repositories",
                    &format!("{} in {} (what pacman -Syu installs)", r.name, r.repo),
                    &r.version,
                ),
                None => value_row(
                    "Newest in repositories",
                    &format!("{} is not in your configured repositories", t.repo_pkg),
                    "—",
                ),
            });
        }

        match &t.upstream {
            Some(rel) => {
                let mut sub = published(rel, now);
                if let Some(at) = t.upstream_fetched_at {
                    sub.push_str(&format!(" · fetched {}", crate::ago(now - at)));
                }
                group.add(&value_row("Latest upstream release", &sub, &rel.tag));
                group.add(&notes_row(rel));
            }
            None => group.add(&value_row(
                "Latest upstream release",
                "Could not be determined",
                "—",
            )),
        }
        if let Some(err) = &t.upstream_error {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title("GitHub lookup problem");
            row.set_subtitle(err);
            row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            group.add(&row);
        }

        if t.on_git() {
            if t.repo.is_some() {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&format!("Switch back to repo {}", t.repo_pkg));
                row.set_subtitle("Tested release builds through normal updates");
                row.add_suffix(&self.switch_button(t, tooling, false));
                group.add(&row);
            }
        } else if t.git_in_aur && t.installed.is_some() && tooling.aur_helper.is_some() {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(&format!("Switch to {} (builds main branch)", t.git_pkg));
            row.set_subtitle("Newest fixes as soon as they land; untested builds compiled locally");
            row.add_suffix(&self.switch_button(t, tooling, true));
            group.add(&row);
        }
        group
    }

    fn switch_button(self: &Rc<Self>, t: &Tracked, tooling: &Tooling, to_git: bool) -> gtk::Button {
        let btn = gtk::Button::builder()
            .label(if to_git {
                "Switch…"
            } else {
                "Switch back…"
            })
            .valign(gtk::Align::Center)
            .build();
        let (title, repo_pkg, git_pkg) = (t.title.clone(), t.repo_pkg.clone(), t.git_pkg.clone());
        let repo = t.repo.as_ref().map(|r| (r.repo.clone(), r.version.clone()));
        let helper = tooling.aur_helper.clone().unwrap_or_default();
        let command = if to_git {
            tooling.aur_install_command(&git_pkg)
        } else {
            Some(tooling.repo_install_command(&repo_pkg))
        };
        let Some(command) = command else { return btn };
        let page = Rc::downgrade(self);
        btn.connect_clicked(move |_| {
            let Some(page) = page.upgrade() else { return };
            let (heading, body) = if to_git {
                (
                    format!("Switch to {git_pkg}?"),
                    format!(
                        "{git_pkg} is built from the latest commit on {title}'s main branch, so you get fixes as soon \
                         as they land, before the next release.\n\n\
                         Tradeoffs: these are untested snapshots that can break, every update compiles from source \
                         (can take several minutes), and new commits are only picked up by “{helper} -Syu --devel”.\n\n\
                         A terminal will open running “{command}”; confirm replacing {repo_pkg} there."
                    ),
                )
            } else {
                let from = repo.as_ref().map_or_else(String::new, |(r, v)| format!(" {v} from {r}"));
                (
                    format!("Switch back to {repo_pkg}?"),
                    format!(
                        "This reinstalls {repo_pkg}{from}. You get tested release builds through normal updates \
                         without compiling, but fixes from the main branch only arrive with the next release.\n\n\
                         A terminal will open running “{command}”; confirm removing {git_pkg} there."
                    ),
                )
            };
            let (page, command) = (page.clone(), command.clone());
            let term_title = format!("Switch {title} package");
            glib::spawn_future_local(async move {
                if page.ctx.confirm(&heading, &body, "Open Terminal", false).await {
                    page.run_terminal(&term_title, &command);
                }
            });
        });
        btn
    }

    fn run_terminal(&self, title: &str, command: &str) {
        match cmd::spawn_in_terminal(title, command) {
            Ok(()) => self.ctx.toast(format!(
                "Opened a terminal running “{command}” — the list refreshes when it finishes"
            )),
            Err(e) => self.ctx.error("Could not open a terminal", &e),
        }
    }

    fn link_button(&self, label: &str, url: &str) -> gtk::Button {
        let btn = gtk::Button::builder()
            .child(
                &adw::ButtonContent::builder()
                    .icon_name("adw-external-link-symbolic")
                    .label(label)
                    .build(),
            )
            .valign(gtk::Align::Center)
            .css_classes(["flat"])
            .tooltip_text(url)
            .build();
        let (url, window) = (url.to_owned(), self.ctx.window.clone());
        btn.connect_clicked(move |_| {
            gtk::UriLauncher::new(&url).launch(Some(&window), gio::Cancellable::NONE, |_| {})
        });
        btn
    }

    /// Resolve the install mode, refresh the self-update check when stale and
    /// (source mode) look up crate updates.
    fn load_hyprdeck(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let mode = rt::blocking(selfupdate::mode).await;
            let st = selfstate::current();
            let stale = st
                .last
                .as_ref()
                .is_none_or(|c| c.mode != mode || check::now() - c.at > SELF_STALE_ON_SHOW);
            if stale && !st.job.is_running() {
                selfstate::check(false, false);
            }
            {
                let Some(page) = weak.upgrade() else { return };
                *page.mode.borrow_mut() = Some(mode.clone());
                page.render_hyprdeck(&selfstate::current());
            }
            let Mode::Source(dir) = mode else { return };
            let result =
                rt::blocking(move || selfupdate::crate_updates(&dir).map_err(|e| format!("{e:#}")))
                    .await;
            let Some(page) = weak.upgrade() else { return };
            *page.crates.borrow_mut() = Some(result);
            page.render_hyprdeck(&selfstate::current());
        });
    }

    fn render_hyprdeck(self: &Rc<Self>, st: &SelfState) {
        match &st.job {
            Job::RestartPending { label, .. } => {
                self.banner.set_title(&format!(
                    "Hyprdeck {label} is installed — restart to finish updating"
                ));
                self.banner.set_revealed(true);
            }
            _ => self.banner.set_revealed(false),
        }
        ui::clear(&self.hyprdeck);
        let Some(mode) = self.mode.borrow().clone() else {
            return;
        };
        let group = adw::PreferencesGroup::builder().title("Hyprdeck").build();
        group.set_description(Some(&glib::markup_escape_text(&mode.describe())));
        self.hyprdeck.append(&group);

        let header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        header.append(&self_check_button(st));
        if !matches!(mode, Mode::Source(_)) {
            header.append(&self.link_button("Releases", selfupdate::RELEASES_URL));
        }
        group.set_header_suffix(Some(&header));
        group.add(&value_row(
            "Version",
            "Running build",
            &hyprdeck_core::version_string(),
        ));

        let checked = st.last.as_deref().filter(|c| c.mode == mode);
        group.add(&self.self_status_row(checked, st));
        self.add_job_rows(&group, &mode, st);

        let check = checked.and_then(|c| c.result.as_ref().ok());
        match check {
            Some(SelfCheck::AppImage(c)) => {
                if let Some(w) = &c.warning {
                    let row = adw::ActionRow::builder().use_markup(false).build();
                    row.set_title("GitHub lookup problem");
                    row.set_subtitle(w);
                    row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                    group.add(&row);
                }
                if c.available
                    && let Some(rel) = &c.release
                {
                    if selfupdate::select_assets(&rel.assets).is_none() {
                        let b = Blocker::NoAsset {
                            tag: rel.tag.clone(),
                        };
                        let row = adw::ActionRow::builder().use_markup(false).build();
                        row.set_title(b.title());
                        row.set_subtitle(&b.message());
                        row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                        row.add_suffix(&self.link_button("Release page", &rel.url));
                        group.add(&row);
                    }
                    group.add(&notes_row(rel));
                }
            }
            Some(SelfCheck::Source(c)) => self.add_source_rows(&group, c),
            _ => {}
        }
        if !matches!(mode, Mode::Installed) {
            group.add(&self.policy_row());
        }
        match &mode {
            Mode::AppImage(_) => group.add(&self.channel_row()),
            Mode::Source(dir) => {
                let upstream = match check {
                    Some(SelfCheck::Source(c)) => c.git.as_ref().and_then(|g| g.upstream.clone()),
                    _ => None,
                };
                let row = value_row(
                    "Update channel",
                    &format!(
                        "Source installs follow their branch's upstream ({}); the stable and nightly channels apply to AppImage installs",
                        upstream.as_deref().unwrap_or("none set")
                    ),
                    "",
                );
                row.set_subtitle_lines(0);
                group.add(&row);
                self.add_source_extras(&group, dir, check);
            }
            Mode::Installed => {}
        }
    }

    /// The headline: up to date, what is available, or why it can't update.
    fn self_status_row(
        self: &Rc<Self>,
        checked: Option<&Checked>,
        st: &SelfState,
    ) -> adw::ActionRow {
        let row = adw::ActionRow::builder().use_markup(false).build();
        row.set_subtitle_lines(0);
        let Some(checked) = checked else {
            row.set_title(if st.checking {
                "Checking for updates…"
            } else {
                "Not checked yet"
            });
            if st.checking {
                row.add_suffix(&adw::Spinner::new());
            }
            return row;
        };
        let now = check::now();
        let checked_ago = format!("checked {}", crate::ago(now - checked.at));
        let check = match &checked.result {
            Ok(c) => c,
            Err(e) => {
                row.set_title("Could not check for updates");
                row.set_subtitle(e);
                row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                return row;
            }
        };
        if let Some(offer) = check.offer() {
            row.set_title(&offer.title);
            let mut sub = offer.detail.clone();
            if let SelfCheck::AppImage(c) = check
                && let Some(assets) = c
                    .release
                    .as_ref()
                    .and_then(|r| selfupdate::select_assets(&r.assets))
            {
                sub.push_str(&format!(
                    " · {:.1} MB{}",
                    assets.appimage.size as f64 / 1e6,
                    if assets.checksum.is_some() {
                        " · SHA-256 checksum published"
                    } else {
                        ""
                    }
                ));
            }
            row.set_subtitle(&format!("{sub} · {checked_ago}"));
            row.add_prefix(&icon("software-update-available-symbolic", "accent"));
            if offer.blocker.is_none() && !st.job.is_running() && !st.job.restart_pending() {
                let btn = gtk::Button::builder()
                    .label("Update now")
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action", "pill"])
                    .tooltip_text(match check {
                        SelfCheck::Source(_) => {
                            "Pulls the new commits and runs install.sh in the background; hyprdeck.service restarts when it finishes"
                        }
                        _ => "Downloads and installs the update; restarts once the window is closed",
                    })
                    .build();
                btn.connect_clicked(|_| selfstate::update(Origin::Page));
                row.add_suffix(&btn);
            }
            return row;
        }
        match check {
            SelfCheck::Installed => {
                row.set_title("Updates are not managed by Hyprdeck");
                row.set_subtitle(
                    "Get new versions from the releases page or the package/script you installed with",
                );
                row.add_prefix(&icon("dialog-information-symbolic", "dim-label"));
            }
            SelfCheck::AppImage(c) => match &c.release {
                None => {
                    row.set_title(match c.channel {
                        Channel::Stable => "No releases published yet",
                        Channel::Nightly => "No nightly build published yet",
                    });
                    row.set_subtitle(&format!(
                        "github.com/{} has no {} to update to · {checked_ago}",
                        selfupdate::REPO,
                        match c.channel {
                            Channel::Stable => "releases",
                            Channel::Nightly => "nightly prerelease",
                        }
                    ));
                    row.add_prefix(&icon("dialog-information-symbolic", "dim-label"));
                }
                Some(rel) => {
                    row.set_title("Up to date");
                    row.set_subtitle(&match &c.commit {
                        Some(commit) => format!(
                            "Latest nightly build is {} · {checked_ago}",
                            selfupdate::short(commit)
                        ),
                        None => format!(
                            "Latest release {} · {} · {checked_ago}",
                            rel.tag,
                            published(rel, now)
                        ),
                    });
                    row.add_prefix(&icon("object-select-symbolic", "success"));
                }
            },
            SelfCheck::Source(c) => match &c.plan.blocker {
                Some(b) => {
                    row.set_title(b.title());
                    row.set_subtitle(&b.message());
                    row.add_prefix(&icon("dialog-information-symbolic", "warning"));
                    row.add_suffix(&self.terminal_fallback(&c.dir, b));
                }
                None => {
                    let upstream = c
                        .git
                        .as_ref()
                        .and_then(|g| g.upstream.as_deref())
                        .unwrap_or("upstream");
                    row.set_title(&format!("Up to date with {upstream}"));
                    row.set_subtitle(&format!("Fetched and compared · {checked_ago}"));
                    row.add_prefix(&icon("object-select-symbolic", "success"));
                }
            },
        }
        row
    }

    fn add_job_rows(self: &Rc<Self>, group: &adw::PreferencesGroup, mode: &Mode, st: &SelfState) {
        let row = adw::ActionRow::builder().use_markup(false).build();
        row.set_subtitle_lines(0);
        match &st.job {
            Job::Idle => return,
            Job::Running(step) => {
                row.set_title(step);
                if matches!(mode, Mode::Source(_)) {
                    row.set_subtitle(&format!(
                        "Building in the background (unit hyprdeck-self-update); output goes to {}",
                        selfupdate::log_path().display()
                    ));
                }
                row.add_suffix(&adw::Spinner::new());
            }
            Job::RestartPending { label, .. } => {
                row.set_title(&format!("Hyprdeck {label} is installed"));
                row.set_subtitle(
                    "Restart to finish updating. Hyprdeck restarts by itself once the window is closed.",
                );
                row.add_prefix(&icon("object-select-symbolic", "success"));
                let btn = gtk::Button::builder()
                    .label("Restart now")
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action"])
                    .build();
                btn.connect_clicked(|_| selfstate::restart_now());
                row.add_suffix(&btn);
            }
            Job::Done(msg) => {
                row.set_title(msg);
                row.add_prefix(&icon("object-select-symbolic", "success"));
            }
            Job::Failed(msg) => {
                row.set_title("Update failed");
                row.set_subtitle(msg);
                row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            }
        }
        group.add(&row);
        if let Some(log) = st.log.as_deref().filter(|l| !l.trim().is_empty()) {
            let exp = adw::ExpanderRow::builder().use_markup(false).build();
            exp.set_title("Build log");
            exp.set_subtitle(&selfupdate::log_path().display().to_string());
            exp.add_row(
                &gtk::Label::builder()
                    .label(log.trim_end())
                    .wrap(true)
                    .wrap_mode(gtk::pango::WrapMode::WordChar)
                    .xalign(0.0)
                    .selectable(true)
                    .css_classes(["monospace"])
                    .margin_top(12)
                    .margin_bottom(12)
                    .margin_start(12)
                    .margin_end(12)
                    .build(),
            );
            exp.set_expanded(self.log_expanded.get());
            let weak = Rc::downgrade(self);
            exp.connect_expanded_notify(move |e| {
                if let Some(page) = weak.upgrade() {
                    page.log_expanded.set(e.is_expanded());
                }
            });
            group.add(&exp);
        }
    }

    /// Source check details: fetch problems, blocked updates, incoming commits.
    fn add_source_rows(self: &Rc<Self>, group: &adw::PreferencesGroup, c: &SourceCheck) {
        if let Some(e) = &c.fetch_error {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title("git fetch failed");
            row.set_subtitle(e);
            row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            group.add(&row);
        }
        // Without an update, the blocker is the status row itself.
        if let Some(b) = c.plan.blocker.as_ref().filter(|_| c.plan.available()) {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(&format!("Can't update automatically: {}", b.title()));
            row.set_subtitle(&b.message());
            row.set_subtitle_lines(0);
            row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            row.add_suffix(&self.terminal_fallback(&c.dir, b));
            group.add(&row);
        }
        if c.incoming.is_empty() {
            return;
        }
        let now = check::now();
        let exp = adw::ExpanderRow::builder().use_markup(false).build();
        exp.set_title(&format!(
            "Incoming commits ({}{})",
            c.incoming.len(),
            if c.plan.behind > c.incoming.len() {
                format!(" of {}", c.plan.behind)
            } else {
                String::new()
            }
        ));
        exp.set_subtitle("What updating pulls in, newest first");
        for commit in &c.incoming {
            let row = value_row(
                &commit.subject,
                &format!(
                    "{} · {}",
                    selfupdate::short(&commit.hash),
                    crate::ago(now - commit.time)
                ),
                "",
            );
            row.set_subtitle_selectable(true);
            exp.add_row(&row);
        }
        group.add(&exp);
    }

    /// "Open terminal" for updates hyprdeck can't do by itself.
    fn terminal_fallback(&self, dir: &std::path::Path, blocker: &Blocker) -> gtk::Button {
        let d = cmd::shell_quote(&dir.to_string_lossy());
        let command = match blocker {
            Blocker::NoCargo | Blocker::NoInstallScript => format!("cd {d} && ./install.sh"),
            _ => format!("cd {d} && git status --short --branch; exec \"${{SHELL:-bash}}\""),
        };
        let btn = gtk::Button::builder()
            .label("Open terminal")
            .valign(gtk::Align::Center)
            .tooltip_text(command.as_str())
            .build();
        let ctx = self.ctx.clone();
        btn.connect_clicked(move |_| {
            if let Err(e) = cmd::spawn_in_terminal("Update Hyprdeck", &command) {
                ctx.error("Could not open a terminal", &e);
            }
        });
        btn
    }

    /// Checkout state, crate updates and the manual rebuild (source mode).
    fn add_source_extras(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        dir: &std::path::Path,
        check: Option<&SelfCheck>,
    ) {
        let now = check::now();
        if let Some(SelfCheck::Source(c)) = check
            && let Some(git) = &c.git
        {
            if let Some(h) = &c.head {
                let row = value_row(
                    &h.subject,
                    &format!(
                        "{} · {} · committed {}",
                        git.branch.as_deref().unwrap_or("detached"),
                        selfupdate::short(&h.hash),
                        crate::ago(now - h.time)
                    ),
                    "",
                );
                row.set_title_lines(1);
                row.set_subtitle_selectable(true);
                group.add(&row);
            }
            group.add(&match git.dirty() {
                0 => value_row("Working tree", "Clean — matches the last commit", ""),
                1 => value_row("Working tree", "1 uncommitted change", ""),
                n => value_row("Working tree", &format!("{n} uncommitted changes"), ""),
            });
        }

        match &*self.crates.borrow() {
            None => {
                let row = adw::ActionRow::builder()
                    .title("Crate dependencies")
                    .subtitle("Checking crates.io for compatible updates…")
                    .build();
                row.add_suffix(&adw::Spinner::new());
                group.add(&row);
            }
            Some(Err(e)) => group.add(&value_row("Crate dependencies", e, "")),
            Some(Ok(list)) if list.is_empty() => group.add(&value_row(
                "Crate dependencies",
                "All locked crates are at their newest compatible versions",
                "",
            )),
            Some(Ok(list)) => {
                let exp = adw::ExpanderRow::builder()
                    .title("Crate dependencies")
                    .subtitle(format!(
                        "{} compatible update{} available (cargo update)",
                        list.len(),
                        selfupdate::plural(list.len())
                    ))
                    .build();
                for c in list {
                    exp.add_row(&value_row(&c.name, &format!("{} → {}", c.old, c.new), ""));
                }
                group.add(&exp);
            }
        }

        let has_install_script = dir.join("install.sh").is_file();
        let rebuild = adw::ActionRow::builder()
            .title("Rebuild and reinstall")
            .subtitle(if has_install_script {
                "Runs ./install.sh in a terminal (release build, then installs hyprdeck)"
            } else {
                "install.sh is missing from the source checkout"
            })
            .build();
        let btn = gtk::Button::builder()
            .label("Rebuild…")
            .valign(gtk::Align::Center)
            .sensitive(has_install_script)
            .build();
        let cmdline = format!(
            "cd {} && ./install.sh",
            cmd::shell_quote(&dir.to_string_lossy())
        );
        let weak = Rc::downgrade(self);
        btn.connect_clicked(move |_| {
            if let Some(page) = weak.upgrade() {
                page.run_terminal("Rebuild hyprdeck", &cmdline);
            }
        });
        rebuild.add_suffix(&btn);
        group.add(&rebuild);
    }

    fn policy_row(&self) -> adw::ComboRow {
        let row = combo_row(
            "Hyprdeck updates",
            "What background checks do when a new Hyprdeck version is out",
            &Policy::ALL.map(Policy::label),
        );
        let current = state::settings().self_update_policy;
        row.set_selected(Policy::ALL.iter().position(|&p| p == current).unwrap_or(0) as u32);
        let ctx = self.ctx.clone();
        row.connect_selected_notify(move |r| {
            let Some(&policy) = Policy::ALL.get(r.selected() as usize) else {
                return;
            };
            save_settings(
                &ctx,
                state::Settings {
                    self_update_policy: policy,
                    ..state::settings()
                },
                false,
            );
        });
        row
    }

    fn channel_row(&self) -> adw::ComboRow {
        let row = combo_row(
            "Update channel",
            "Stable follows tagged releases; nightly follows every push to main (untested builds)",
            &Channel::ALL.map(Channel::label),
        );
        let current = state::settings().self_update_channel;
        row.set_selected(Channel::ALL.iter().position(|&c| c == current).unwrap_or(0) as u32);
        let ctx = self.ctx.clone();
        row.connect_selected_notify(move |r| {
            let Some(&channel) = Channel::ALL.get(r.selected() as usize) else {
                return;
            };
            save_settings(
                &ctx,
                state::Settings {
                    self_update_channel: channel,
                    ..state::settings()
                },
                true,
            );
        });
        row
    }
}

fn self_check_button(st: &SelfState) -> gtk::Button {
    let btn = gtk::Button::builder()
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    if st.checking {
        let b = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        b.append(&adw::Spinner::new());
        b.append(&gtk::Label::new(Some("Checking…")));
        btn.set_child(Some(&b));
        btn.set_sensitive(false);
    } else {
        btn.set_child(Some(
            &adw::ButtonContent::builder()
                .icon_name("view-refresh-symbolic")
                .label("Check now")
                .build(),
        ));
        btn.set_tooltip_text(Some(
            "Check GitHub / the upstream branch for a new Hyprdeck",
        ));
        btn.connect_clicked(|_| selfstate::check(true, false));
    }
    btn
}

fn combo_row(title: &str, subtitle: &str, labels: &[&str]) -> adw::ComboRow {
    let row = adw::ComboRow::builder()
        .use_markup(false)
        .model(&gtk::StringList::new(labels))
        .build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

/// Persist settings off the main thread; `recheck` re-runs the self-update check.
fn save_settings(ctx: &Ctx, new: state::Settings, recheck: bool) {
    if new == state::settings() {
        return;
    }
    let ctx = ctx.clone();
    glib::spawn_future_local(async move {
        match rt::blocking(move || state::save_settings(new)).await {
            Ok(()) => {
                ctx.toast("Update settings saved");
                if recheck {
                    selfstate::check(false, false);
                }
            }
            Err(e) => ctx.error("Saving update settings failed", &e),
        }
    });
}

/// Shown when none of the known components is installed.
fn no_components_group() -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Upstream releases")
        .build();
    let names: Vec<&str> = PROJECTS.iter().map(|p| p.title).collect();
    let row = adw::ActionRow::builder()
        .title("No tracked desktop components installed")
        .subtitle(format!("Release tracking covers {}", names.join(", ")))
        .build();
    row.set_subtitle_lines(0);
    row.add_prefix(&icon("dialog-information-symbolic", "dim-label"));
    group.add(&row);
    group
}

fn settings_group(ctx: &Ctx) -> adw::PreferencesGroup {
    let s = state::settings();
    let group = adw::PreferencesGroup::builder()
        .title("Automatic checks")
        .description("Runs in the background while hyprdeck is in the tray; the first check is 2 minutes after start")
        .build();
    let interval = adw::SpinRow::builder()
        .title("Check every (hours)")
        .subtitle(
            "Also re-checks right after any pacman transaction (including AUR helpers) finishes",
        )
        .adjustment(&gtk::Adjustment::new(
            f64::from(s.interval_hours),
            1.0,
            72.0,
            1.0,
            6.0,
            0.0,
        ))
        .build();
    let notify = adw::SwitchRow::builder()
        .title("Notify about new updates")
        .subtitle("Desktop notification when the number of pending updates grows")
        .active(s.notify)
        .build();
    let save = {
        let (ctx, interval, notify) = (ctx.clone(), interval.downgrade(), notify.downgrade());
        move || {
            let (Some(interval), Some(notify)) = (interval.upgrade(), notify.upgrade()) else {
                return;
            };
            let new = state::Settings {
                interval_hours: interval.value() as u32,
                notify: notify.is_active(),
                ..state::settings()
            };
            if new == state::settings() {
                return;
            }
            let ctx = ctx.clone();
            glib::spawn_future_local(async move {
                match rt::blocking(move || state::save_settings(new)).await {
                    Ok(()) => ctx.toast("Update check settings saved"),
                    Err(e) => ctx.error("Saving update settings failed", &e),
                }
            });
        }
    };
    let save = Rc::new(save);
    let s2 = save.clone();
    interval.connect_value_notify(move |_| s2());
    notify.connect_active_notify(move |_| save());
    group.add(&interval);
    group.add(&notify);
    group
}

fn published(rel: &Release, now: i64) -> String {
    if rel.published_at > 0 {
        format!(
            "Published {} · {}",
            local_date(rel.published_at),
            crate::days_ago(now - rel.published_at)
        )
    } else {
        "Publish date unknown".to_owned()
    }
}

fn notes_row(rel: &Release) -> adw::ExpanderRow {
    let notes = adw::ExpanderRow::builder().use_markup(false).build();
    notes.set_title(&format!("Release notes: {}", rel.name));
    notes.set_subtitle("As published on GitHub (markdown)");
    let label = gtk::Label::builder()
        .label(if rel.notes.trim().is_empty() {
            "(no release notes)"
        } else {
            rel.notes.trim()
        })
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    notes.add_row(&label);
    notes
}

fn update_row(u: &Update, aur: bool) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .subtitle_selectable(true)
        .build();
    row.set_title(&u.name);
    row.set_subtitle(&format!("{} → {}", u.old, u.new));
    if u.important {
        row.add_suffix(&badge("important", "important"));
    }
    row.add_suffix(&if aur {
        badge("AUR", "aur")
    } else {
        badge("repo", "repo")
    });
    row
}

fn badge(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .valign(gtk::Align::Center)
        .css_classes(["hd-badge", class])
        .build()
}

fn value_row(title: &str, subtitle: &str, value: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    if !value.is_empty() {
        row.add_suffix(
            &gtk::Label::builder()
                .label(value)
                .selectable(true)
                .css_classes(["numeric", "dim-label"])
                .build(),
        );
    }
    row
}

fn icon(name: &str, class: &str) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(name)
        .css_classes([class])
        .build()
}

fn local_time(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix)
        .and_then(|d| d.format("%b %-d, %H:%M"))
        .map_or_else(|_| unix.to_string(), String::from)
}

fn local_date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix)
        .and_then(|d| d.format("%Y-%m-%d"))
        .map_or_else(|_| unix.to_string(), String::from)
}
