//! Builds GE-Proton from source with Hyprdeck's optional compiler flags and installs it.
use crate::{
    proton::{self, TargetKind},
    proton_cpu,
};
use adw::prelude::*;
use gtk::glib;
use hd_protonbuild::{
    builder::{self, Requirement},
    recipe::{FlagProfile, Recipe},
};
use hyprdeck_core::{
    rt,
    ui::{self, Ctx},
};
use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Instant,
};

const DEFAULT_BASE: &str = "GE-Proton11-1";
const FLAGS: [FlagProfile; 3] = [FlagProfile::Stock, FlagProfile::O3, FlagProfile::V3];
const MAX_LOG_LINES: i32 = 4000;

/// Sources, build trees, ccache and cargo state: `~/.local/share/hyprdeck/proton-build`.
fn work_dir() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or_else(|| hyprdeck_core::store::home().join(".local/share"))
        .join("hyprdeck/proton-build")
}

fn jobs() -> usize {
    std::thread::available_parallelism().map_or(4, |n| n.get())
}

enum Message {
    Line(String),
    Done(Result<PathBuf, String>),
}

struct Page {
    ctx: Ctx,
    requirements: adw::PreferencesGroup,
    requirement_rows: RefCell<Vec<gtk::Widget>>,
    install_tools: gtk::Button,
    missing: RefCell<Vec<&'static str>>,
    base: adw::EntryRow,
    flags: adw::ComboRow,
    estimate: adw::ActionRow,
    build: gtk::Button,
    cancel: gtk::Button,
    progress: gtk::ProgressBar,
    phase: gtk::Label,
    log: gtk::TextBuffer,
    log_view: gtk::TextView,
    busy: Cell<bool>,
    ready: Cell<bool>,
    cancel_flag: RefCell<Option<Arc<AtomicBool>>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = ui::page_scaffold();
    let intro = adw::PreferencesGroup::builder()
        .title("Proton builder")
        .description(format!(
            "Build GE-Proton from source with optional optimizations and install it as a Steam compatibility tool. The build runs inside the Steam Runtime SDK container (about 9.4 GB, downloaded once) and needs about {} GB free. HDR, DLSS and latency settings don't need a build; set them under Game settings.",
            builder::REQUIRED_FREE_BYTES / 1_000_000_000
        ))
        .build();
    content.append(&intro);

    let requirements = adw::PreferencesGroup::builder()
        .title("Requirements")
        .build();
    let install_tools = gtk::Button::with_label("Install missing tools");
    install_tools.add_css_class("suggested-action");
    install_tools.set_valign(gtk::Align::Center);
    install_tools.set_visible(false);
    let recheck = gtk::Button::with_label("Check again");
    recheck.set_valign(gtk::Align::Center);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.append(&install_tools);
    actions.append(&recheck);
    requirements.set_header_suffix(Some(&actions));
    content.append(&requirements);

    let recipe = adw::PreferencesGroup::builder().title("Build").build();
    let base = adw::EntryRow::builder().use_markup(false).build();
    base.set_title("GE-Proton release tag");
    base.set_text(DEFAULT_BASE);
    recipe.add(&base);
    let level = proton_cpu::cpu_profile().x86_64_level.unwrap_or(1);
    let labels: Vec<&str> = FLAGS.iter().map(|f| f.label()).collect();
    let flags = adw::ComboRow::builder()
        .use_markup(false)
        .model(&gtk::StringList::new(&labels))
        .build();
    flags.set_title("Compiler flags");
    flags.set_subtitle(&format!(
        "-O3 builds about 14% longer. x86-64-v3 follows Proton-CachyOS: v3 scheduling and BMI instructions without AVX. This CPU supports up to x86-64-v{level}."
    ));
    flags.set_selected(if level >= 3 { 2 } else { 0 });
    recipe.add(&flags);
    let estimate = adw::ActionRow::builder().use_markup(false).build();
    estimate.set_title("Estimated time");
    recipe.add(&estimate);
    content.append(&recipe);

    let run = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let build = gtk::Button::with_label("Build and install");
    build.add_css_class("suggested-action");
    let cancel = gtk::Button::with_label("Cancel");
    cancel.set_visible(false);
    buttons.append(&build);
    buttons.append(&cancel);
    run.append(&buttons);
    let phase = gtk::Label::builder()
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    run.append(&phase);
    let progress = gtk::ProgressBar::new();
    progress.set_visible(false);
    run.append(&progress);
    let log_view = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    let log = log_view.buffer();
    run.append(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_height(260)
            .child(&log_view)
            .build(),
    );
    content.append(&run);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        requirements,
        requirement_rows: RefCell::default(),
        install_tools,
        missing: RefCell::default(),
        base,
        flags,
        estimate,
        build,
        cancel,
        progress,
        phase,
        log,
        log_view,
        busy: Cell::new(false),
        ready: Cell::new(false),
        cancel_flag: RefCell::default(),
    });
    page.update_estimate();
    page.flags.connect_selected_notify({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.update_estimate();
            }
        }
    });
    recheck.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.check();
            }
        }
    });
    page.install_tools.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.install_missing();
            }
        }
    });
    page.build.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.start();
            }
        }
    });
    page.cancel.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade()
                && let Some(flag) = page.cancel_flag.borrow().as_ref()
            {
                flag.store(true, Ordering::Relaxed);
                page.phase.set_label("Cancelling…");
            }
        }
    });
    ui::on_shown(&scroller, {
        let page = Rc::downgrade(&page);
        move || {
            if let Some(page) = page.upgrade()
                && !page.busy.get()
            {
                page.check();
            }
        }
    });
    scroller.connect_destroy({
        let owner = RefCell::new(Some(page));
        move |_| {
            if let Some(page) = owner.borrow_mut().take()
                && let Some(flag) = page.cancel_flag.borrow().as_ref()
            {
                flag.store(true, Ordering::Relaxed);
            }
        }
    });
    scroller.upcast()
}

impl Page {
    fn selected_flags(&self) -> FlagProfile {
        FLAGS
            .get(self.flags.selected() as usize)
            .copied()
            .unwrap_or_default()
    }

    fn update_estimate(&self) {
        let flags = self.selected_flags();
        let minutes = builder::estimate(flags, jobs()).as_secs().div_ceil(60);
        self.estimate.set_subtitle(&format!(
            "About {minutes} minutes with {} build jobs, plus the first download (about 7 GB of source and 9.4 GB of SDK). Rebuilds take about 2 minutes.",
            jobs()
        ));
        let level = proton_cpu::cpu_profile().x86_64_level.unwrap_or(1);
        self.build
            .set_sensitive(!self.busy.get() && level >= flags.required_x86_64_level());
    }

    fn check(self: &Rc<Self>) {
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let list = rt::blocking(|| builder::requirements(&work_dir())).await;
                if let Some(page) = weak.upgrade() {
                    page.render_requirements(&list);
                }
            }
        });
    }

    fn render_requirements(&self, list: &[Requirement]) {
        for row in self.requirement_rows.borrow_mut().drain(..) {
            self.requirements.remove(&row);
        }
        let mut rows = Vec::new();
        for requirement in list {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(requirement.name);
            row.set_subtitle(&requirement.detail);
            let icon = gtk::Image::from_icon_name(if requirement.present {
                "object-select-symbolic"
            } else {
                "dialog-warning-symbolic"
            });
            row.add_prefix(&icon);
            self.requirements.add(&row);
            rows.push(row.upcast::<gtk::Widget>());
        }
        *self.requirement_rows.borrow_mut() = rows;
        let missing = builder::missing_packages(list);
        self.install_tools.set_visible(!missing.is_empty());
        *self.missing.borrow_mut() = missing;
        self.ready.set(list.iter().all(|r| r.present));
        if !self.ready.get() && !self.busy.get() {
            self.phase
                .set_label("Some requirements are missing. Install the missing tools or free up disk space first.");
        } else if !self.busy.get() {
            self.phase.set_label("");
        }
    }

    fn install_missing(self: &Rc<Self>) {
        let names: Vec<String> = self
            .missing
            .borrow()
            .iter()
            .map(|s| s.to_string())
            .collect();
        if names.is_empty() || self.busy.get() {
            return;
        }
        self.set_busy(true);
        self.phase
            .set_label(&format!("Installing {}…", names.join(", ")));
        let (sender, receiver) = async_channel::unbounded::<Message>();
        rt::spawn(async move {
            let lines = sender.clone();
            let result = tokio::task::spawn_blocking(move || {
                let cancel = AtomicBool::new(false);
                hd_updates::install_tools(&names, &cancel, &mut |line| {
                    let _ = lines.send_blocking(Message::Line(line.to_owned()));
                })
            })
            .await
            .unwrap_or_else(|e| Err(e.to_string()));
            let _ = sender
                .send(Message::Done(result.map(|()| PathBuf::new())))
                .await;
        });
        let page = self.clone();
        glib::spawn_future_local(async move {
            while let Ok(message) = receiver.recv().await {
                match message {
                    Message::Line(line) => page.append_log(&line),
                    Message::Done(result) => {
                        match result {
                            Ok(_) => page.ctx.toast("Build tools installed"),
                            Err(e) => page.phase.set_label(&e),
                        }
                        page.set_busy(false);
                        page.check();
                    }
                }
            }
        });
    }

    fn start(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        if !self.ready.get() {
            self.phase
                .set_label("Some requirements are missing; see above.");
            return;
        }
        let base = self.base.text().trim().to_owned();
        let valid = base
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
        if base.is_empty() || !valid || base.starts_with(['-', '.']) {
            self.phase
                .set_label("Enter a GE-Proton release tag such as GE-Proton11-1.");
            return;
        }
        let recipe = Recipe::new(&base, self.selected_flags());
        let plan = builder::plan(&recipe, &work_dir(), jobs());
        let total = plan.steps.len().max(1);
        let cancel = Arc::new(AtomicBool::new(false));
        *self.cancel_flag.borrow_mut() = Some(cancel.clone());
        self.log.set_text("");
        self.set_busy(true);
        self.progress.set_fraction(0.);
        let started = Instant::now();
        let (sender, receiver) = async_channel::unbounded::<Message>();
        rt::spawn(async move {
            let lines = sender.clone();
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<PathBuf> {
                std::fs::create_dir_all(work_dir())?;
                builder::run(&plan, &cancel, &mut |line| {
                    let _ = lines.send_blocking(Message::Line(line.to_owned()));
                })?;
                let target = proton::targets()?
                    .into_iter()
                    .find(|t| t.kind == TargetKind::Steam)
                    .ok_or_else(|| anyhow::anyhow!("No Steam installation to install into"))?;
                builder::install(&plan, &target.root)
            })
            .await
            .map_err(anyhow::Error::from)
            .and_then(|r| r)
            .map_err(|e| format!("{e:#}"));
            let _ = sender.send(Message::Done(result)).await;
        });
        let page = self.clone();
        let mut step = 0usize;
        glib::spawn_future_local(async move {
            while let Ok(message) = receiver.recv().await {
                match message {
                    Message::Line(line) => {
                        if let Some(title) = line.strip_prefix("==> ") {
                            step += 1;
                            page.progress.set_fraction(step as f64 / total as f64);
                            page.phase
                                .set_label(&format!("Step {step} of {total}: {title}"));
                        }
                        page.append_log(&line);
                    }
                    Message::Done(result) => {
                        let elapsed = started.elapsed().as_secs();
                        let time = format!("{}m{:02}s", elapsed / 60, elapsed % 60);
                        match result {
                            Ok(path) => {
                                page.progress.set_fraction(1.);
                                let name = path
                                    .file_name()
                                    .unwrap_or_default()
                                    .to_string_lossy()
                                    .into_owned();
                                page.phase.set_label(&format!("Built and installed {name} in {time}. Restart Steam, then pick it for a game or create a Hyprdeck variant of it under Game settings."));
                                page.ctx.toast(format!("Installed {name}"));
                            }
                            Err(e) => page
                                .phase
                                .set_label(&format!("Build stopped after {time}: {e}")),
                        }
                        page.cancel_flag.borrow_mut().take();
                        page.set_busy(false);
                    }
                }
            }
        });
    }

    fn append_log(&self, line: &str) {
        let mut end = self.log.end_iter();
        self.log.insert(&mut end, line);
        self.log.insert(&mut end, "\n");
        if self.log.line_count() > MAX_LOG_LINES {
            let mut start = self.log.start_iter();
            let mut cut = self
                .log
                .iter_at_line(self.log.line_count() - MAX_LOG_LINES)
                .unwrap_or_else(|| self.log.start_iter());
            self.log.delete(&mut start, &mut cut);
        }
        let mark = self.log.create_mark(None, &self.log.end_iter(), false);
        self.log_view.scroll_mark_onscreen(&mark);
        self.log.delete_mark(&mark);
    }

    fn set_busy(&self, busy: bool) {
        self.busy.set(busy);
        self.cancel
            .set_visible(busy && self.cancel_flag.borrow().is_some());
        self.progress
            .set_visible(busy || self.progress.fraction() > 0.);
        self.base.set_sensitive(!busy);
        self.flags.set_sensitive(!busy);
        self.install_tools.set_sensitive(!busy);
        self.update_estimate();
    }
}
