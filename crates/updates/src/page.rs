//! The "Updates" page.

use std::cell::RefCell;
use std::path::PathBuf;
use std::rc::Rc;
use std::sync::Once;

use adw::prelude::*;
use gtk::{gio, glib};
use hyprdeck_core::{cmd, rt, ui, ui::Ctx};

use crate::check::{self, PROJECTS, StatusKind, Tooling, Tracked};
use crate::github::{Lookup, Release};
use crate::parse::{CrateUpdate, Update};
use crate::selfupdate::{self, Mode, SourceInfo};
use crate::state::{self, State};

/// The page re-checks on show when the last check is older than this.
const STALE_ON_SHOW: i64 = 15 * 60;

const CSS: &str = "
.hd-badge { font-size: smaller; font-weight: bold; padding: 1px 8px; border-radius: 999px;
            background: alpha(currentColor, 0.1); }
.hd-badge.important { background: alpha(var(--accent-bg-color), 0.25); color: var(--accent-color); }
.hd-badge.aur { background: alpha(var(--warning-bg-color), 0.2); color: var(--warning-color); }
";

fn load_css() {
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

/// What the hyprdeck section shows, by install mode.
enum SelfView {
    Loading,
    Source {
        info: Result<SourceInfo, String>,
        /// Latest crate-update lookup (`None` while running).
        crates: Option<Result<Vec<CrateUpdate>, String>>,
    },
    AppImage {
        path: PathBuf,
        /// Latest-release lookup (`None` while running).
        latest: Option<Result<Lookup, String>>,
    },
    Installed,
}

/// AppImage self-update progress; survives re-renders and page re-shows.
enum Download {
    Idle,
    Running(String),
    Done { tag: String, verified: bool },
    Failed(String),
}

struct Page {
    ctx: Ctx,
    system: gtk::Box,
    tracked: gtk::Box,
    hyprdeck: gtk::Box,
    me: RefCell<SelfView>,
    download: RefCell<Download>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    load_css();
    let (scroller, content) = ui::page_scaffold();
    let section = || gtk::Box::new(gtk::Orientation::Vertical, 24);
    let page = Rc::new(Page {
        ctx: ctx.clone(),
        system: section(),
        tracked: section(),
        hyprdeck: section(),
        me: RefCell::new(SelfView::Loading),
        download: RefCell::new(Download::Idle),
    });
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
        match (&tooling.aur_helper, &report.aur_error) {
            (Some(_), Some(err)) => {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title("AUR check failed");
                row.set_subtitle(err);
                row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                group.add(&row);
            }
            (None, _) => {
                let row = adw::ActionRow::builder()
                    .title("AUR packages are not checked")
                    .subtitle("Install an AUR helper (paru or yay) to include them; updates run with pacman meanwhile")
                    .build();
                row.add_prefix(&icon("dialog-information-symbolic", "dim-label"));
                group.add(&row);
            }
            (Some(_), None) => {}
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
            .subtitle(match &tooling.aur_helper {
                Some(_) => format!(
                    "{} from repositories · {} from the AUR · {important} important",
                    report.repo.len(),
                    report.aur.len()
                ),
                None => format!(
                    "{} from repositories · {important} important",
                    report.repo.len()
                ),
            })
            .build();
        summary.add_prefix(&if total == 0 {
            icon("emblem-ok-symbolic", "success")
        } else {
            icon("software-update-available-symbolic", "accent")
        });
        if total > 0 {
            let command = tooling.upgrade_command();
            let btn = gtk::Button::builder()
                .label("Update everything")
                .valign(gtk::Align::Center)
                .css_classes(["suggested-action", "pill"])
                .tooltip_text(format!(
                    "Opens a terminal running {command} (asks for your password there)"
                ))
                .build();
            let page = Rc::downgrade(self);
            btn.connect_clicked(move |_| {
                if let Some(page) = page.upgrade() {
                    page.run_terminal("System update", &command);
                }
            });
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
            StatusKind::UpToDate => icon("emblem-ok-symbolic", "success"),
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

    /// Refresh the hyprdeck section for the current install mode.
    fn load_hyprdeck(self: &Rc<Self>) {
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            match rt::blocking(selfupdate::mode).await {
                Mode::Source(dir) => {
                    let info = rt::blocking(move || {
                        selfupdate::source_info(&dir).map_err(|e| format!("{e:#}"))
                    })
                    .await;
                    let dir = info.as_ref().ok().map(|i| i.dir.clone());
                    {
                        let Some(page) = weak.upgrade() else { return };
                        *page.me.borrow_mut() = SelfView::Source { info, crates: None };
                        page.render_hyprdeck();
                    }
                    let Some(dir) = dir else { return };
                    let result = rt::blocking(move || {
                        selfupdate::crate_updates(&dir).map_err(|e| format!("{e:#}"))
                    })
                    .await;
                    let Some(page) = weak.upgrade() else { return };
                    if let SelfView::Source { crates, .. } = &mut *page.me.borrow_mut() {
                        *crates = Some(result);
                    }
                    page.render_hyprdeck();
                }
                Mode::AppImage(path) => {
                    {
                        let Some(page) = weak.upgrade() else { return };
                        *page.me.borrow_mut() = SelfView::AppImage { path, latest: None };
                        page.render_hyprdeck();
                    }
                    let lookup = rt::blocking(|| {
                        selfupdate::latest(check::now()).map_err(|e| format!("{e:#}"))
                    })
                    .await;
                    let Some(page) = weak.upgrade() else { return };
                    if let SelfView::AppImage { latest, .. } = &mut *page.me.borrow_mut() {
                        *latest = Some(lookup);
                    }
                    page.render_hyprdeck();
                }
                Mode::Installed => {
                    let Some(page) = weak.upgrade() else { return };
                    *page.me.borrow_mut() = SelfView::Installed;
                    page.render_hyprdeck();
                }
            }
        });
    }

    fn render_hyprdeck(self: &Rc<Self>) {
        ui::clear(&self.hyprdeck);
        let group = adw::PreferencesGroup::builder().title("hyprdeck").build();
        self.hyprdeck.append(&group);
        match &*self.me.borrow() {
            SelfView::Loading => {}
            SelfView::Source { info, crates } => self.render_source(&group, info, crates.as_ref()),
            SelfView::AppImage { path, latest } => {
                self.render_appimage(&group, path, latest.as_ref())
            }
            SelfView::Installed => {
                group.set_header_suffix(Some(
                    &self.link_button("Releases", selfupdate::RELEASES_URL),
                ));
                group.add(&value_row(
                    "Installed version",
                    "Installed by a package or script; new versions are published on GitHub",
                    selfupdate::VERSION,
                ));
            }
        }
    }

    fn render_source(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        info: &Result<SourceInfo, String>,
        crates: Option<&Result<Vec<CrateUpdate>, String>>,
    ) {
        let info = match info {
            Ok(info) => info,
            Err(e) => {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title("Source checkout not available");
                row.set_subtitle(e);
                row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                group.add(&row);
                return;
            }
        };
        group.set_description(Some(&glib::markup_escape_text(&format!(
            "Built from source at {}",
            info.dir.display()
        ))));
        group.add(&value_row("Version", "Running build", selfupdate::VERSION));

        let now = check::now();
        match &info.git {
            Some(git) => {
                group.add(&match &git.head {
                    Some(h) => {
                        let row = value_row(
                            &h.subject,
                            &format!(
                                "{} · {} · committed {}",
                                git.branch.as_deref().unwrap_or("detached"),
                                h.hash,
                                crate::ago(now - h.time)
                            ),
                            "",
                        );
                        row.set_title_lines(1);
                        row.set_subtitle_selectable(true);
                        row
                    }
                    None => value_row("No commits yet", "", ""),
                });
                group.add(&match git.dirty {
                    0 => value_row("Working tree", "Clean — matches the last commit", ""),
                    1 => value_row("Working tree", "1 uncommitted change", ""),
                    n => value_row("Working tree", &format!("{n} uncommitted changes"), ""),
                });
            }
            None => group.add(&value_row(
                "Git",
                "Not a git checkout (or git is not installed)",
                "",
            )),
        }

        match crates {
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
                        if list.len() == 1 { "" } else { "s" }
                    ))
                    .build();
                for c in list {
                    exp.add_row(&value_row(&c.name, &format!("{} → {}", c.old, c.new), ""));
                }
                group.add(&exp);
            }
        }

        let rebuild = adw::ActionRow::builder()
            .title("Rebuild and reinstall")
            .subtitle(if info.has_install_script {
                "Runs ./install.sh in a terminal (release build, then installs hyprdeck)"
            } else {
                "install.sh is not executable in the source checkout"
            })
            .build();
        let btn = gtk::Button::builder()
            .label("Rebuild…")
            .valign(gtk::Align::Center)
            .sensitive(info.has_install_script)
            .build();
        let cmdline = format!(
            "cd {} && ./install.sh",
            cmd::shell_quote(&info.dir.to_string_lossy())
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

    fn render_appimage(
        self: &Rc<Self>,
        group: &adw::PreferencesGroup,
        path: &std::path::Path,
        latest: Option<&Result<Lookup, String>>,
    ) {
        group.set_description(Some(&glib::markup_escape_text(&format!(
            "Running from AppImage {}",
            path.display()
        ))));
        group.set_header_suffix(Some(
            &self.link_button("Releases", selfupdate::RELEASES_URL),
        ));
        group.add(&value_row("Installed version", "", selfupdate::VERSION));
        let now = check::now();

        match &*self.download.borrow() {
            Download::Done { tag, verified } => {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&format!("Updated to {tag}"));
                row.set_subtitle(if *verified {
                    "Checksum verified; restart to use the new version"
                } else {
                    "No checksum was published; restart to use the new version"
                });
                row.add_prefix(&icon("emblem-ok-symbolic", "success"));
                let btn = gtk::Button::builder()
                    .label("Restart hyprdeck")
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action"])
                    .build();
                let (weak, path) = (Rc::downgrade(self), path.to_path_buf());
                btn.connect_clicked(move |_| {
                    let (weak, path) = (weak.clone(), path.clone());
                    glib::spawn_future_local(async move {
                        let result = rt::blocking(move || selfupdate::restart(&path)).await;
                        let Some(page) = weak.upgrade() else { return };
                        match result {
                            Ok(()) => page.ctx.toast("Restarting hyprdeck…"),
                            Err(e) => page.ctx.error("Restart failed", &e),
                        }
                    });
                });
                row.add_suffix(&btn);
                group.add(&row);
                return;
            }
            Download::Running(tag) => {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title(&format!("Downloading {tag}…"));
                row.set_subtitle(&format!("Replaces {} when complete", path.display()));
                row.add_suffix(&adw::Spinner::new());
                group.add(&row);
                return;
            }
            Download::Failed(e) => {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title("Update failed");
                row.set_subtitle(e);
                row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                group.add(&row);
            }
            Download::Idle => {}
        }

        let lookup = match latest {
            None => {
                let row = adw::ActionRow::builder()
                    .title("Latest release")
                    .subtitle("Checking GitHub…")
                    .build();
                row.add_suffix(&adw::Spinner::new());
                group.add(&row);
                return;
            }
            Some(Err(e)) => {
                let row = adw::ActionRow::builder().use_markup(false).build();
                row.set_title("Could not check for updates");
                row.set_subtitle(e);
                row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
                group.add(&row);
                return;
            }
            Some(Ok(lookup)) => lookup,
        };
        if let Some(w) = &lookup.warning {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title("GitHub lookup problem");
            row.set_subtitle(w);
            row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
            group.add(&row);
        }
        let Some(rel) = &lookup.release else {
            let row = adw::ActionRow::builder()
                .title("No releases published yet")
                .subtitle(format!(
                    "github.com/{} has no releases to update to · checked {}",
                    selfupdate::REPO,
                    crate::ago(now - lookup.fetched_at)
                ))
                .build();
            row.add_prefix(&icon("dialog-information-symbolic", "dim-label"));
            group.add(&row);
            return;
        };

        if !selfupdate::is_newer(&rel.tag) {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title("Up to date");
            row.set_subtitle(&format!(
                "Latest release {} · {}",
                rel.tag,
                published(rel, now)
            ));
            row.add_prefix(&icon("emblem-ok-symbolic", "success"));
            group.add(&row);
            return;
        }
        let row = adw::ActionRow::builder().use_markup(false).build();
        row.set_title(&format!("Update available: {}", rel.tag));
        row.add_prefix(&icon("software-update-available-symbolic", "accent"));
        match selfupdate::select_assets(&rel.assets) {
            Some(assets) => {
                row.set_subtitle(&format!(
                    "{} · {:.1} MB{}",
                    published(rel, now),
                    assets.appimage.size as f64 / 1e6,
                    if assets.checksum.is_some() {
                        " · SHA-256 checksum published"
                    } else {
                        ""
                    }
                ));
                let btn = gtk::Button::builder()
                    .label("Download update")
                    .valign(gtk::Align::Center)
                    .css_classes(["suggested-action"])
                    .build();
                let (weak, path, tag) = (Rc::downgrade(self), path.to_path_buf(), rel.tag.clone());
                btn.connect_clicked(move |_| {
                    let Some(page) = weak.upgrade() else { return };
                    *page.download.borrow_mut() = Download::Running(tag.clone());
                    page.render_hyprdeck();
                    let (weak, path, tag, assets) =
                        (weak.clone(), path.clone(), tag.clone(), assets.clone());
                    glib::spawn_future_local(async move {
                        let result =
                            rt::blocking(move || selfupdate::install_appimage(&path, &assets))
                                .await;
                        let Some(page) = weak.upgrade() else { return };
                        *page.download.borrow_mut() = match result {
                            Ok(verified) => Download::Done { tag, verified },
                            Err(e) => Download::Failed(format!("{e:#}")),
                        };
                        page.render_hyprdeck();
                    });
                });
                row.add_suffix(&btn);
            }
            None => {
                row.set_subtitle(&format!(
                    "The release has no {} file; download it from the release page",
                    selfupdate::APPIMAGE_ASSET
                ));
                row.add_suffix(&self.link_button("Release page", &rel.url));
            }
        }
        group.add(&row);
        group.add(&notes_row(rel));
    }
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
