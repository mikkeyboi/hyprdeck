//! The update dialog: review → (password prompt) → progress → summary.

use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::rc::Rc;

use adw::prelude::*;
use gtk::{gio, glib};
use hyprdeck_core::{cmd, rt, ui, ui::Ctx};

use crate::apply::{self, Outcome, RunState, StepState, Summary};
use crate::aur;
use crate::news::NewsItem;
use crate::parse::Update;
use crate::progress::Op;
use crate::review::{self, AurReview, Review};
use crate::scan::{Finding, Severity};
use crate::state;

thread_local! {
    /// The open dialog; owns its state until the dialog closes.
    static OPEN: RefCell<Option<Rc<Dialog>>> = const { RefCell::new(None) };
}

/// Open the update dialog (or bring the open one back). Shows the running or
/// finished update when there is one, otherwise prepares a fresh review.
pub fn open(ctx: &Ctx) {
    if let Some(d) = OPEN.with(|o| o.borrow().clone()) {
        d.dialog.present(Some(&ctx.window));
        return;
    }
    let d = Dialog::new(ctx);
    OPEN.with(|o| *o.borrow_mut() = Some(d.clone()));
    d.dialog.present(Some(&ctx.window));
    let st = apply::subscribe().borrow().clone();
    if st.active || st.summary.is_some() {
        d.show_progress();
    } else {
        d.load_review();
    }
}

struct Dialog {
    ctx: Ctx,
    dialog: adw::Dialog,
    stack: gtk::Stack,
    review_page: RefCell<adw::PreferencesPage>,
    bottom: gtk::Box,
    update_btn: gtk::Button,
    review: RefCell<Option<Rc<Review>>>,
    approved: RefCell<BTreeSet<String>>,
    news_ack: Cell<bool>,
    progress: ProgressView,
    /// Number of log lines already in the log view.
    log_shown: Cell<usize>,
    /// Step rows currently shown.
    step_rows: RefCell<Vec<(adw::ActionRow, gtk::Stack)>>,
    bottom_mode: Cell<Option<BottomMode>>,
}

/// What the bottom bar of the progress view currently offers.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BottomMode {
    Cancellable,
    Running,
    Finished,
}

struct ProgressView {
    root: gtk::ScrolledWindow,
    headline: gtk::Label,
    phase: gtk::Label,
    bar: gtk::ProgressBar,
    steps: gtk::ListBox,
    summary: gtk::Box,
    log: gtk::TextView,
    log_scroller: gtk::ScrolledWindow,
}

impl Dialog {
    fn new(ctx: &Ctx) -> Rc<Self> {
        crate::page::load_css();
        let dialog = adw::Dialog::builder()
            .title("System update")
            .content_width(760)
            .content_height(860)
            .build();
        let stack = gtk::Stack::builder()
            .transition_type(gtk::StackTransitionType::Crossfade)
            .vexpand(true)
            .build();
        let loading = adw::StatusPage::builder()
            .title("Preparing the review")
            .description(
                "Checking repositories, Arch news and AUR packages. Nothing is changed yet.",
            )
            .child(
                &adw::Spinner::builder()
                    .width_request(48)
                    .height_request(48)
                    .build(),
            )
            .build();
        stack.add_named(&loading, Some("loading"));
        let review_page = adw::PreferencesPage::new();
        stack.add_named(&review_page, Some("review"));
        let progress = ProgressView::new();
        stack.add_named(&progress.root, Some("progress"));

        let bottom = gtk::Box::builder()
            .orientation(gtk::Orientation::Horizontal)
            .spacing(12)
            .margin_top(12)
            .margin_bottom(12)
            .margin_start(18)
            .margin_end(18)
            .build();
        let update_btn = gtk::Button::builder()
            .label("Update")
            .css_classes(["suggested-action", "pill"])
            .build();
        let view = adw::ToolbarView::new();
        view.add_top_bar(&adw::HeaderBar::new());
        view.set_content(Some(&stack));
        view.add_bottom_bar(&bottom);
        dialog.set_child(Some(&view));

        let d = Rc::new(Dialog {
            ctx: ctx.clone(),
            dialog,
            stack,
            review_page: RefCell::new(review_page),
            bottom,
            update_btn,
            review: RefCell::new(None),
            approved: RefCell::new(BTreeSet::new()),
            news_ack: Cell::new(false),
            progress,
            log_shown: Cell::new(0),
            step_rows: RefCell::new(Vec::new()),
            bottom_mode: Cell::new(None),
        });
        let weak = Rc::downgrade(&d);
        d.update_btn.connect_clicked(move |_| {
            if let Some(d) = weak.upgrade() {
                d.start_update();
            }
        });
        d.dialog.connect_closed(|_| {
            // A finished run is forgotten once its summary was seen.
            apply::reset();
            OPEN.with(|o| o.borrow_mut().take());
        });

        let weak = Rc::downgrade(&d);
        let mut rx = apply::subscribe();
        glib::spawn_future_local(async move {
            while rx.changed().await.is_ok() {
                let Some(d) = weak.upgrade() else { break };
                if d.stack.visible_child_name().as_deref() == Some("progress") {
                    let st = rx.borrow_and_update().clone();
                    d.render_progress(&st);
                }
            }
        });
        d
    }

    fn set_bottom(&self, widgets: &[&gtk::Widget]) {
        ui::clear(&self.bottom);
        for w in widgets {
            self.bottom.append(*w);
        }
    }

    fn load_review(self: &Rc<Self>) {
        self.stack.set_visible_child_name("loading");
        let cancel = gtk::Button::with_label("Cancel");
        let dialog = self.dialog.clone();
        cancel.connect_clicked(move |_| {
            dialog.close();
        });
        self.set_bottom(&[spacer().upcast_ref(), cancel.upcast_ref()]);
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let review = rt::blocking(|| review::prepare(false)).await;
            let Some(d) = weak.upgrade() else { return };
            d.show_review(Rc::new(review));
        });
    }

    fn show_review(self: &Rc<Self>, review: Rc<Review>) {
        *self.approved.borrow_mut() = review.default_approved();
        self.news_ack.set(false);
        *self.review.borrow_mut() = Some(review.clone());
        let page = adw::PreferencesPage::new();
        let old = self.review_page.replace(page.clone());
        self.stack.remove(&old);
        self.stack.add_named(&page, Some("review"));

        page.add(&self.summary_group(&review));
        if let Some(g) = self.news_group(&review) {
            page.add(&g);
        }
        if !review.repo.is_empty() {
            page.add(&repo_group(&review));
        }
        if !review.aur.is_empty() || review.aur_error.is_some() {
            page.add(&self.aur_group(&review));
        }
        if !review.not_in_aur.is_empty() {
            page.add(&not_in_aur_group(&review));
        }

        let terminal = gtk::Button::builder()
            .label("Open in terminal instead")
            .tooltip_text(format!(
                "Runs “{}” in a terminal (asks for your password there)",
                review.tooling.upgrade_command()
            ))
            .build();
        let weak = Rc::downgrade(self);
        terminal.connect_clicked(move |_| {
            if let Some(d) = weak.upgrade() {
                d.open_terminal();
            }
        });
        self.set_bottom(&[
            terminal.upcast_ref(),
            spacer().upcast_ref(),
            self.update_btn.upcast_ref(),
        ]);
        self.refresh_update_button();
        self.stack.set_visible_child_name("review");
    }

    fn refresh_update_button(&self) {
        let Some(review) = self.review.borrow().clone() else {
            return;
        };
        let news_ok = review.unread_news().is_empty() || self.news_ack.get();
        let approved_aur = self.approved.borrow().len();
        let work = !review.repo.is_empty() || approved_aur > 0;
        self.update_btn.set_sensitive(news_ok && work);
        self.update_btn.set_tooltip_text(Some(if !work {
            "Nothing to update"
        } else if !news_ok {
            "Confirm that you have read the news first"
        } else {
            "Asks for your password once, then upgrades the system and builds the approved AUR packages"
        }));
    }

    fn summary_group(&self, review: &Review) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::new();
        let row = adw::ActionRow::builder().use_markup(false).build();
        let total = review.repo.len() + review.aur.len();
        row.set_title(&match total {
            0 => "Nothing to update".to_owned(),
            1 => "1 update ready".to_owned(),
            n => format!("{n} updates ready"),
        });
        let mut sub = format!(
            "{} from the repositories · {} from the AUR",
            review.repo.len(),
            review.aur.len()
        );
        if let Some(size) = review.download_size {
            sub.push_str(&format!(" · {} to download", review::human_size(size)));
        }
        let important = review
            .repo
            .iter()
            .chain(review.aur.iter().flat_map(|a| &a.updates))
            .filter(|u| u.important)
            .count();
        if important > 0 {
            sub.push_str(&format!(" · {important} important"));
        }
        row.set_subtitle(&sub);
        row.add_prefix(&icon(
            if total == 0 {
                "object-select-symbolic"
            } else {
                "software-update-available-symbolic"
            },
            if total == 0 { "success" } else { "accent" },
        ));
        group.add(&row);
        if let Some(e) = &review.repo_error {
            group.add(&warning_row("Repository check failed", e));
        }
        group
    }

    fn news_group(self: &Rc<Self>, review: &Review) -> Option<adw::PreferencesGroup> {
        let news = review.news.as_ref()?;
        if news.items.is_empty() && news.warning.is_none() {
            return None;
        }
        let group = adw::PreferencesGroup::builder()
            .title("Read before updating")
            .build();
        group.set_description(Some(&glib::markup_escape_text(&format!(
            "{} published since {}",
            news.label,
            local_date(news.since)
        ))));
        if let Some(w) = &news.warning {
            group.add(&warning_row("News unavailable", w));
        }
        for item in &news.items {
            group.add(&self.news_row(item));
        }
        if !news.items.is_empty() {
            let check = gtk::CheckButton::builder()
                .valign(gtk::Align::Center)
                .build();
            let row = adw::ActionRow::builder()
                .title("I have read the news above")
                .subtitle("Required before updating: news items announce manual interventions")
                .activatable_widget(&check)
                .build();
            row.add_prefix(&check);
            let weak = Rc::downgrade(self);
            check.connect_toggled(move |c| {
                if let Some(d) = weak.upgrade() {
                    d.news_ack.set(c.is_active());
                    d.refresh_update_button();
                }
            });
            group.add(&row);
        }
        Some(group)
    }

    fn news_row(&self, item: &NewsItem) -> adw::ExpanderRow {
        let row = adw::ExpanderRow::builder().use_markup(false).build();
        row.set_title(&item.title);
        row.set_subtitle(&local_date(item.published));
        row.add_prefix(&icon("dialog-information-symbolic", "accent"));
        row.add_row(&text_label(&item.summary));
        let link = adw::ActionRow::builder().use_markup(false).build();
        link.set_title("Read on archlinux.org");
        link.set_subtitle(&item.link);
        link.add_suffix(&self.link_button(&item.link));
        row.add_row(&link);
        row
    }

    fn aur_group(self: &Rc<Self>, review: &Review) -> adw::PreferencesGroup {
        let group = adw::PreferencesGroup::builder()
            .title(format!("AUR packages ({})", review.aur.len()))
            .description(glib::markup_escape_text(&format!(
                "Built on this computer from user-submitted scripts. Review the changes; unticked packages are skipped this run. {}.",
                aur::VCS_NOTE
            )))
            .build();
        if let Some(e) = &review.aur_error {
            group.add(&warning_row("AUR check failed", e));
        }
        for a in &review.aur {
            group.add(&self.aur_row(a));
        }
        group
    }

    fn aur_row(self: &Rc<Self>, a: &AurReview) -> adw::ExpanderRow {
        let row = adw::ExpanderRow::builder().use_markup(false).build();
        let names: Vec<&str> = a.updates.iter().map(|u| u.name.as_str()).collect();
        row.set_title(&names.join(", "));
        row.set_subtitle(
            &a.updates
                .iter()
                .map(|u| format!("{} → {}", u.old, u.new))
                .collect::<Vec<_>>()
                .join(", "),
        );
        let check = gtk::CheckButton::builder()
            .valign(gtk::Align::Center)
            .active(self.approved.borrow().contains(&a.pkgbase))
            .sensitive(a.approvable())
            .tooltip_text("Build and install this package in this run")
            .build();
        row.add_prefix(&check);
        let risk = a.risk();
        row.add_suffix(&match (&a.error, risk) {
            (Some(_), _) => badge("not reviewed", "critical"),
            (None, Some(Severity::Critical)) => badge("critical", "critical"),
            (None, Some(Severity::Warning)) => badge("warning", "warning"),
            (None, _) => badge("no warnings", "ok"),
        });
        if a.updates.iter().any(|u| u.important) {
            row.add_suffix(&badge("important", "important"));
        }
        if a.updates.iter().any(|u| aur::is_vcs(&u.name)) {
            let vcs = badge("VCS", "aur");
            vcs.set_tooltip_text(Some(aur::VCS_NOTE));
            row.add_suffix(&vcs);
        }
        let confirmed = Rc::new(Cell::new(false));
        let weak = Rc::downgrade(self);
        let pkgbase = a.pkgbase.clone();
        check.connect_toggled(move |c| {
            let Some(d) = weak.upgrade() else { return };
            if c.is_active() && risk == Some(Severity::Critical) && !confirmed.get() {
                c.set_active(false);
                let (c, d2, confirmed, pkgbase) =
                    (c.clone(), d.clone(), confirmed.clone(), pkgbase.clone());
                glib::spawn_future_local(async move {
                    let ok = d2
                        .ctx
                        .confirm(
                            &format!("Build {pkgbase} despite critical findings?"),
                            "The security scan found patterns typical of malicious packages (see the findings). Only continue if you have read the PKGBUILD and trust it: it runs on your computer and its package is installed as root.",
                            "Build anyway",
                            true,
                        )
                        .await;
                    if ok {
                        confirmed.set(true);
                        c.set_active(true);
                    }
                });
                return;
            }
            if c.is_active() {
                d.approved.borrow_mut().insert(pkgbase.clone());
            } else {
                d.approved.borrow_mut().remove(&pkgbase);
            }
            d.refresh_update_button();
        });

        let meta = adw::ActionRow::builder()
            .use_markup(false)
            .subtitle_selectable(true)
            .build();
        meta.set_title(&format!("AUR package base {}", a.pkgbase));
        meta.set_subtitle(&review::aur_meta_line(&a.info));
        meta.set_subtitle_lines(0);
        meta.add_suffix(&self.link_button(&a.info.page_url()));
        row.add_row(&meta);

        if let Some(e) = &a.error {
            row.add_row(&warning_row(
                "Could not fetch the package for review; it is skipped",
                e,
            ));
            return row;
        }
        if a.findings.is_empty() {
            let r = adw::ActionRow::builder()
                .title("Security scan: no findings")
                .subtitle(
                    "Static checks of the PKGBUILD, install scripts, sources and AUR metadata",
                )
                .build();
            r.add_prefix(&icon("object-select-symbolic", "success"));
            row.add_row(&r);
        }
        for f in &a.findings {
            row.add_row(&finding_row(f));
        }
        if let Some(c) = &a.changes {
            let header = adw::ActionRow::builder().use_markup(false).build();
            header.set_title("PKGBUILD review");
            header.set_subtitle(&review::baseline_text(c));
            header.set_subtitle_lines(0);
            row.add_row(&header);
            if !c.text.is_empty() {
                row.add_row(&code_view(&c.text, c.is_diff()));
            }
        }
        row
    }

    fn link_button(&self, url: &str) -> gtk::Button {
        let btn = gtk::Button::builder()
            .icon_name("adw-external-link-symbolic")
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

    fn open_terminal(&self) {
        let command = self
            .review
            .borrow()
            .as_ref()
            .map(|r| r.tooling.upgrade_command())
            .unwrap_or_else(|| crate::check::Tooling::detect().upgrade_command());
        self.run_terminal(&command);
    }

    fn run_terminal(&self, command: &str) {
        match cmd::spawn_in_terminal("System update", command) {
            Ok(()) => {
                self.ctx.toast(format!(
                    "Opened a terminal running “{command}” — the list refreshes when it finishes"
                ));
                self.dialog.close();
            }
            Err(e) => self.ctx.error("Could not open a terminal", &e),
        }
    }

    fn start_update(self: &Rc<Self>) {
        let Some(review) = self.review.borrow().clone() else {
            return;
        };
        let plan = review.plan(&self.approved.borrow());
        match apply::start(plan) {
            Ok(()) => self.show_progress(),
            Err(e) => self.ctx.error("Could not start the update", &e),
        }
    }

    fn show_progress(self: &Rc<Self>) {
        self.log_shown.set(0);
        self.bottom_mode.set(None);
        self.progress.log.buffer().set_text("");
        self.step_rows.borrow_mut().clear();
        ui::clear(&self.progress.steps);
        self.stack.set_visible_child_name("progress");
        let st = apply::subscribe().borrow().clone();
        self.render_progress(&st);
    }

    fn render_progress(self: &Rc<Self>, st: &RunState) {
        let p = &self.progress;
        // Steps.
        if self.step_rows.borrow().len() != st.steps.len() {
            ui::clear(&p.steps);
            let rows: Vec<_> = st
                .steps
                .iter()
                .map(|_| {
                    let row = adw::ActionRow::builder().use_markup(false).build();
                    let state_icon = gtk::Stack::new();
                    state_icon.add_named(&adw::Spinner::new(), Some("running"));
                    for (name, icon_name, class) in [
                        ("pending", "content-loading-symbolic", "dim-label"),
                        ("done", "object-select-symbolic", "success"),
                        ("failed", "dialog-error-symbolic", "error"),
                        ("skipped", "action-unavailable-symbolic", "dim-label"),
                    ] {
                        state_icon.add_named(&icon(icon_name, class), Some(name));
                    }
                    row.add_prefix(&state_icon);
                    p.steps.append(&row);
                    (row, state_icon)
                })
                .collect();
            *self.step_rows.borrow_mut() = rows;
        }
        for ((row, state_icon), step) in self.step_rows.borrow().iter().zip(&st.steps) {
            row.set_title(&step.title);
            row.set_subtitle(&step.detail);
            state_icon.set_visible_child_name(match step.state {
                StepState::Pending => "pending",
                StepState::Running => "running",
                StepState::Done => "done",
                StepState::Failed => "failed",
                StepState::Skipped => "skipped",
            });
        }
        p.bar.set_fraction(st.fraction);
        p.bar
            .set_text(Some(&format!("{:.0}%", st.fraction * 100.0)));
        p.phase.set_text(&st.phase);

        // Log: append what's new, following the end unless the user scrolled up.
        if st.log.len() > self.log_shown.get() {
            let adj = p.log_scroller.vadjustment();
            let at_end = adj.value() + adj.page_size() >= adj.upper() - 40.0;
            let buffer = p.log.buffer();
            let mut end = buffer.end_iter();
            let mut text = String::new();
            for line in &st.log[self.log_shown.get()..] {
                text.push_str(line);
                text.push('\n');
            }
            buffer.insert(&mut end, &text);
            self.log_shown.set(st.log.len());
            if at_end {
                let mark = buffer.create_mark(None, &buffer.end_iter(), false);
                p.log.scroll_mark_onscreen(&mark);
                buffer.delete_mark(&mark);
            }
        }

        // Bottom bar and summary.
        let mode = match (&st.summary, st.cancellable) {
            (Some(_), _) => BottomMode::Finished,
            (None, true) => BottomMode::Cancellable,
            (None, false) => BottomMode::Running,
        };
        if self.bottom_mode.replace(Some(mode)) == Some(mode) {
            return;
        }
        match &st.summary {
            None => {
                p.headline.set_text("Updating your system…");
                p.summary.set_visible(false);
                let hint = gtk::Label::builder()
                    .label("You can close this window; the update continues in the background.")
                    .css_classes(["dim-label"])
                    .wrap(true)
                    .xalign(0.0)
                    .hexpand(true)
                    .build();
                if mode == BottomMode::Cancellable {
                    let cancel = gtk::Button::with_label("Cancel");
                    cancel.set_tooltip_text(Some("Possible until you have entered your password"));
                    cancel.connect_clicked(|b| {
                        apply::cancel();
                        b.set_sensitive(false);
                    });
                    self.set_bottom(&[hint.upcast_ref(), cancel.upcast_ref()]);
                } else {
                    self.set_bottom(&[hint.upcast_ref()]);
                }
            }
            Some(summary) => self.render_summary(summary),
        }
    }

    fn render_summary(self: &Rc<Self>, s: &Summary) {
        let p = &self.progress;
        p.headline.set_text(&s.headline);
        p.phase.set_text("");
        ui::clear(&p.summary);
        p.summary.set_visible(true);
        let group = adw::PreferencesGroup::new();
        let status = adw::ActionRow::builder().use_markup(false).build();
        status.set_title(&s.headline);
        status.set_subtitle(&apply::summary_line(s));
        status.set_subtitle_lines(0);
        status.add_prefix(&match s.outcome {
            Outcome::Success => icon("object-select-symbolic", "success"),
            Outcome::Partial => icon("dialog-warning-symbolic", "warning"),
            Outcome::Failed => icon("dialog-error-symbolic", "error"),
            Outcome::Cancelled => icon("dialog-information-symbolic", "dim-label"),
        });
        group.add(&status);
        if !s.restart.is_empty() {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title("Restart recommended");
            row.set_subtitle(&format!(
                "Updated: {} — they are fully used only after a reboot",
                s.restart.join(", ")
            ));
            row.add_prefix(&icon("system-reboot-symbolic", "warning"));
            group.add(&row);
        }
        let changed: Vec<&(Op, String)> = s.changes.iter().collect();
        if !changed.is_empty() {
            let exp = adw::ExpanderRow::builder().use_markup(false).build();
            exp.set_title(&format!("Package changes ({})", changed.len()));
            exp.set_subtitle("What pacman installed, upgraded or removed");
            for (op, name) in changed {
                let r = adw::ActionRow::builder().use_markup(false).build();
                r.set_title(name);
                r.set_subtitle(op_label(*op));
                exp.add_row(&r);
            }
            group.add(&exp);
        }
        for (name, reason) in &s.failed {
            let r = adw::ActionRow::builder().use_markup(false).build();
            r.set_title(&format!("{name} failed"));
            r.set_subtitle(reason);
            r.set_subtitle_lines(0);
            r.add_prefix(&icon("dialog-error-symbolic", "error"));
            group.add(&r);
        }
        for (name, reason) in &s.skipped {
            let r = adw::ActionRow::builder().use_markup(false).build();
            r.set_title(&format!("{name} skipped"));
            r.set_subtitle(reason);
            r.set_subtitle_lines(0);
            r.add_prefix(&icon("action-unavailable-symbolic", "dim-label"));
            group.add(&r);
        }
        if !s.pacnew.is_empty() {
            let exp = adw::ExpanderRow::builder().use_markup(false).build();
            exp.set_title(&format!(
                "Configuration files to merge ({})",
                s.pacnew.len()
            ));
            exp.set_subtitle(
                "New default configs saved next to yours (.pacnew); merge them with pacdiff",
            );
            for path in &s.pacnew {
                let r = adw::ActionRow::builder()
                    .use_markup(false)
                    .title_selectable(true)
                    .build();
                r.set_title(path);
                if s.new_pacnew.contains(path) {
                    r.add_suffix(&badge("new", "important"));
                }
                exp.add_row(&r);
            }
            group.add(&exp);
        }
        if let Some(command) = &s.fallback {
            let r = adw::ActionRow::builder().use_markup(false).build();
            r.set_title("Continue in a terminal");
            r.set_subtitle(&format!(
                "Runs “{command}” interactively, where you can answer pacman's questions"
            ));
            r.set_subtitle_lines(0);
            let btn = gtk::Button::builder()
                .label("Open terminal")
                .valign(gtk::Align::Center)
                .build();
            let (weak, command) = (Rc::downgrade(self), command.clone());
            btn.connect_clicked(move |_| {
                if let Some(d) = weak.upgrade() {
                    d.run_terminal(&command);
                }
            });
            r.add_suffix(&btn);
            group.add(&r);
        }
        if let Some(path) = &s.log_path {
            let r = adw::ActionRow::builder()
                .use_markup(false)
                .subtitle_selectable(true)
                .build();
            r.set_title("Log saved");
            r.set_subtitle(&path.display().to_string());
            group.add(&r);
        }
        p.summary.append(&group);

        let recheck = gtk::Button::builder()
            .label("Check again")
            .tooltip_text("Look for updates again")
            .build();
        let dialog = self.dialog.clone();
        recheck.connect_clicked(move |_| {
            state::trigger(false);
            dialog.close();
        });
        let close = gtk::Button::builder()
            .label("Close")
            .css_classes(["suggested-action", "pill"])
            .build();
        let dialog = self.dialog.clone();
        close.connect_clicked(move |_| {
            dialog.close();
        });
        self.set_bottom(&[
            spacer().upcast_ref(),
            recheck.upcast_ref(),
            close.upcast_ref(),
        ]);
    }
}

impl ProgressView {
    fn new() -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
        content.set_margin_top(12);
        content.set_margin_bottom(18);
        content.set_margin_start(18);
        content.set_margin_end(18);
        let headline = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .css_classes(["title-2"])
            .build();
        let phase = gtk::Label::builder()
            .xalign(0.0)
            .wrap(true)
            .ellipsize(gtk::pango::EllipsizeMode::End)
            .css_classes(["dim-label"])
            .build();
        let bar = gtk::ProgressBar::builder().show_text(true).build();
        let steps = gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .css_classes(["boxed-list"])
            .build();
        let summary = gtk::Box::new(gtk::Orientation::Vertical, 12);
        summary.set_visible(false);

        let log = gtk::TextView::builder()
            .editable(false)
            .cursor_visible(false)
            .monospace(true)
            .wrap_mode(gtk::WrapMode::WordChar)
            .top_margin(8)
            .bottom_margin(8)
            .left_margin(8)
            .right_margin(8)
            .build();
        let log_scroller = gtk::ScrolledWindow::builder()
            .min_content_height(260)
            .max_content_height(260)
            .child(&log)
            .css_classes(["card"])
            .build();
        let copy = gtk::Button::builder()
            .icon_name("edit-copy-symbolic")
            .tooltip_text("Copy the log")
            .css_classes(["flat"])
            .build();
        let log_for_copy = log.clone();
        copy.connect_clicked(move |b| {
            let buffer = log_for_copy.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false);
            b.clipboard().set_text(&text);
        });
        let log_header = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        log_header.append(
            &gtk::Label::builder()
                .label("Log")
                .xalign(0.0)
                .hexpand(true)
                .css_classes(["heading"])
                .build(),
        );
        log_header.append(&copy);

        content.append(&headline);
        content.append(&phase);
        content.append(&bar);
        content.append(&summary);
        content.append(&steps);
        content.append(&log_header);
        content.append(&log_scroller);
        let root = gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .vexpand(true)
            .child(
                &adw::Clamp::builder()
                    .maximum_size(720)
                    .child(&content)
                    .build(),
            )
            .build();
        ProgressView {
            root,
            headline,
            phase,
            bar,
            steps,
            summary,
            log,
            log_scroller,
        }
    }
}

fn op_label(op: Op) -> &'static str {
    match op {
        Op::Install => "installed",
        Op::Upgrade => "upgraded",
        Op::Reinstall => "reinstalled",
        Op::Downgrade => "downgraded",
        Op::Remove => "removed",
    }
}

fn repo_group(review: &Review) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title(format!("Repository packages ({})", review.repo.len()))
        .build();
    if let Some(size) = review.download_size {
        group.set_description(Some(&format!(
            "{} to download; already downloaded packages are reused",
            review::human_size(size)
        )));
    }
    for u in review.repo.iter().filter(|u| u.important) {
        group.add(&update_row(u));
    }
    let exp = adw::ExpanderRow::builder()
        .title(format!("All repository updates ({})", review.repo.len()))
        .build();
    for u in &review.repo {
        exp.add_row(&update_row(u));
    }
    group.add(&exp);
    group
}

fn not_in_aur_group(review: &Review) -> adw::PreferencesGroup {
    let group = adw::PreferencesGroup::builder()
        .title("Not from the AUR")
        .description("Foreign packages the AUR doesn't know (local builds or removed packages); they are never updated here")
        .build();
    let exp = adw::ExpanderRow::builder()
        .title(format!("{} packages", review.not_in_aur.len()))
        .build();
    for f in &review.not_in_aur {
        let r = adw::ActionRow::builder().use_markup(false).build();
        r.set_title(&f.name);
        r.set_subtitle(&f.version);
        exp.add_row(&r);
    }
    group.add(&exp);
    group
}

fn update_row(u: &Update) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(&u.name);
    row.set_subtitle(&format!("{} → {}", u.old, u.new));
    if u.important {
        row.add_suffix(&badge("important", "important"));
    }
    row
}

fn finding_row(f: &Finding) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .use_markup(false)
        .subtitle_selectable(true)
        .build();
    row.set_title(&format!("{}: {}", f.severity.label(), f.title));
    let at = match f.line {
        Some(l) => format!("{}:{l}", f.file),
        None => f.file.clone(),
    };
    row.set_subtitle(&format!("{at} · {}", f.excerpt));
    row.set_subtitle_lines(3);
    row.add_prefix(&match f.severity {
        Severity::Critical => icon("dialog-error-symbolic", "error"),
        Severity::Warning => icon("dialog-warning-symbolic", "warning"),
        Severity::Info => icon("dialog-information-symbolic", "dim-label"),
    });
    row
}

/// Monospace, read-only view of a diff (colored) or of full files.
fn code_view(text: &str, diff: bool) -> gtk::ScrolledWindow {
    let buffer = gtk::TextBuffer::new(None);
    let tag = |name: &str, color: Option<&str>, bold: bool| {
        let t = gtk::TextTag::new(Some(name));
        if let Some(c) = color {
            t.set_foreground(Some(c));
        }
        if bold {
            t.set_weight(700);
        }
        buffer.tag_table().add(&t);
    };
    tag("add", Some("#2ec27e"), false);
    tag("del", Some("#e01b24"), false);
    tag("hunk", Some("#3584e4"), false);
    tag("file", None, true);
    for line in text.lines() {
        let name = if line.starts_with("==> ") || line.starts_with("diff --git") {
            Some("file")
        } else if !diff {
            None
        } else if line.starts_with("+++") || line.starts_with("---") {
            Some("file")
        } else if line.starts_with('+') {
            Some("add")
        } else if line.starts_with('-') {
            Some("del")
        } else if line.starts_with("@@") {
            Some("hunk")
        } else {
            None
        };
        let mut end = buffer.end_iter();
        match name {
            Some(n) => buffer.insert_with_tags_by_name(&mut end, &format!("{line}\n"), &[n]),
            None => buffer.insert(&mut end, &format!("{line}\n")),
        }
    }
    let view = gtk::TextView::builder()
        .buffer(&buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .top_margin(8)
        .bottom_margin(8)
        .left_margin(8)
        .right_margin(8)
        .build();
    gtk::ScrolledWindow::builder()
        .min_content_height(320)
        .max_content_height(320)
        .child(&view)
        .build()
}

fn text_label(text: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .wrap(true)
        .wrap_mode(gtk::pango::WrapMode::WordChar)
        .xalign(0.0)
        .selectable(true)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build()
}

fn warning_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_subtitle_lines(0);
    row.add_prefix(&icon("dialog-warning-symbolic", "warning"));
    row
}

fn badge(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .valign(gtk::Align::Center)
        .css_classes(["hd-badge", class])
        .build()
}

fn icon(name: &str, class: &str) -> gtk::Image {
    gtk::Image::builder()
        .icon_name(name)
        .css_classes([class])
        .build()
}

fn spacer() -> gtk::Box {
    gtk::Box::builder().hexpand(true).build()
}

fn local_date(unix: i64) -> String {
    glib::DateTime::from_unix_local(unix)
        .and_then(|d| d.format("%Y-%m-%d"))
        .map_or_else(|_| unix.to_string(), String::from)
}
