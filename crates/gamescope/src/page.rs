//! Native Gamescope / Steam Big Picture launcher. Detection and persistence
//! stay off the GTK thread; refreshing never replaces an edited draft.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::hypr::ctl::Monitor;
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::config::{Filter, Settings, Toggle, WindowMode};
use crate::engine::{self, LaunchOutcome, Plan, SessionStatus, Snapshot, SteamState};
use crate::startup;

/// A visibility epoch prevents an observation started before hiding the page
/// from being applied after it is shown again. At most one poll is in flight.
#[derive(Default)]
struct SessionPolling {
    mapped: bool,
    in_flight: bool,
    epoch: u64,
}

impl SessionPolling {
    fn set_mapped(&mut self, mapped: bool) {
        self.mapped = mapped;
        self.epoch = self.epoch.wrapping_add(1);
    }

    fn begin(&mut self) -> Option<u64> {
        if !self.mapped || self.in_flight {
            return None;
        }
        self.in_flight = true;
        Some(self.epoch)
    }

    fn complete(&mut self, epoch: u64) -> bool {
        self.in_flight = false;
        self.mapped && self.epoch == epoch
    }
}
struct Page {
    window: glib::WeakRef<adw::ApplicationWindow>,
    toasts: glib::WeakRef<adw::ToastOverlay>,
    ready: Cell<bool>,
    startup_known: Cell<bool>,
    syncing: Cell<bool>,
    dirty: Cell<bool>,
    busy: Cell<bool>,
    refresh_pending: Cell<bool>,
    detecting: Cell<bool>,
    polling: RefCell<SessionPolling>,
    poll_timer: RefCell<Option<glib::SourceId>>,
    session: RefCell<Option<SessionStatus>>,
    session_error: RefCell<Option<String>>,
    preview_busy: Cell<bool>,
    revision: Cell<u64>,
    save_valid: Cell<bool>,
    snapshot: RefCell<Option<Snapshot>>,
    plan: RefCell<Option<Plan>>,
    command: RefCell<String>,
    outputs: RefCell<Vec<Option<String>>>,
    config_error: RefCell<Option<String>>,
    detection_error: RefCell<Option<String>>,
    hero_state: gtk::Label,
    launch: gtk::Button,
    refresh: gtk::Button,
    save: gtk::Button,
    copy: gtk::Button,
    spinner: adw::Spinner,
    startup: adw::SwitchRow,
    status_group: adw::PreferencesGroup,
    config_message: gtk::Label,
    detection_message: gtk::Label,
    operation_message: gtk::Label,
    defaults: gtk::Button,
    saved_state: gtk::Label,
    controls: gtk::Box,
    display: adw::ActionRow,
    output: adw::ComboRow,
    auto_display: adw::SwitchRow,
    width: adw::EntryRow,
    height: adw::EntryRow,
    refresh_rate: adw::EntryRow,
    custom_render: adw::SwitchRow,
    render_width: adw::EntryRow,
    render_height: adw::EntryRow,
    hdr: adw::ComboRow,
    adaptive_sync: adw::ComboRow,
    window_mode: adw::ComboRow,
    filter: adw::ComboRow,
    framerate_limit: adw::EntryRow,
    grab_keyboard: adw::SwitchRow,
    force_grab_cursor: adw::SwitchRow,
    hide_cursor_delay: adw::EntryRow,
    mouse_sensitivity: adw::EntryRow,
    resolved: adw::ActionRow,
    preview: gtk::TextView,
    plan_message: gtk::Label,
    warnings: gtk::Label,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (root, content) = ui::page_scaffold();

    let hero = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    hero.set_margin_top(20);
    hero.set_margin_bottom(20);
    hero.set_margin_start(20);
    hero.set_margin_end(20);
    let icon = gtk::Image::from_icon_name("input-gaming-symbolic");
    icon.set_pixel_size(56);
    icon.add_css_class("accent");
    hero.append(&icon);
    let heading = gtk::Box::new(gtk::Orientation::Vertical, 6);
    heading.set_hexpand(true);
    let title = text_label("Steam Big Picture", "title-1");
    title.set_selectable(false);
    heading.append(&title);
    heading.append(&text_label(
        "A nested Gamescope session, right on your Hyprland desktop.",
        "dim-label",
    ));
    let hero_state = text_label(
        "Open this page to detect Gamescope, Steam and your displays.",
        "caption",
    );
    heading.append(&hero_state);
    hero.append(&heading);
    let launch = gtk::Button::builder()
        .label("Launch Big Picture")
        .css_classes(["suggested-action", "pill"])
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    hero.append(&launch);
    let hero_frame = gtk::Frame::builder()
        .child(&hero)
        .css_classes(["card"])
        .build();
    content.append(&hero_frame);

    let overlay_group = adw::PreferencesGroup::new();
    let overlay_row = action_row(
        "Performance overlay",
        "MangoHUD settings and a live 3D preview",
    );
    overlay_row.add_prefix(&gtk::Image::from_icon_name(
        "utilities-system-monitor-symbolic",
    ));
    overlay_row.add_suffix(&gtk::Image::from_icon_name("go-next-symbolic"));
    overlay_row.set_activatable(true);
    overlay_group.add(&overlay_row);
    content.append(&overlay_group);

    let startup = switch_row(
        "Launch Steam Big Picture at login",
        "Off by default. Uses your saved launch profile at the next Hyprland login; toggling does not start or stop Steam.",
    );
    startup.set_sensitive(false);
    let startup_group = adw::PreferencesGroup::builder().title("Startup").build();
    startup_group.add(&startup);
    content.append(&startup_group);

    let status_group = adw::PreferencesGroup::builder()
        .title("Launcher status")
        .visible(false)
        .build();
    let config_message = text_label("", "error");
    let detection_message = text_label("", "error");
    let operation_message = text_label("", "caption");
    let defaults = gtk::Button::builder()
        .label("Use defaults…")
        .halign(gtk::Align::Start)
        .visible(false)
        .build();
    let status_box = gtk::Box::new(gtk::Orientation::Vertical, 8);
    status_box.append(&config_message);
    status_box.append(&defaults);
    status_box.append(&detection_message);
    status_box.append(&operation_message);
    status_group.add(&status_box);
    content.append(&status_group);

    let controls = gtk::Box::new(gtk::Orientation::Vertical, 24);
    controls.set_sensitive(false);
    let refresh = gtk::Button::builder()
        .icon_name("view-refresh-symbolic")
        .tooltip_text("Refresh display and Steam detection; keep your settings")
        .css_classes(["flat"])
        .build();
    let spinner = adw::Spinner::builder()
        .visible(false)
        .valign(gtk::Align::Center)
        .build();
    let detection_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    detection_actions.append(&spinner);
    detection_actions.append(&refresh);
    // Detection remains available even when a configuration cannot be read.
    let display_group = adw::PreferencesGroup::builder()
        .title("Detected display")
        .description("Automatic sizing uses physical pixels, not scale-divided desktop coordinates. The focused display is resolved again at launch.")
        .header_suffix(&detection_actions)
        .build();
    let display = action_row("Waiting for display detection", "");
    display.add_prefix(&gtk::Image::from_icon_name("video-display-symbolic"));
    display_group.add(&display);
    content.append(&display_group);

    let output_group = adw::PreferencesGroup::builder()
        .title("Output and resolution")
        .description("Recommended: follow the selected display’s current physical resolution and refresh rate. This does not change your desktop display mode.")
        .build();
    let output = combo_row(
        "Target display",
        "Focused display follows your focus when launching",
        &["Focused display (at launch)"],
    );
    let auto_display = switch_row(
        "Use detected display settings",
        "Turn off to use the custom output values below; automatic values are shown in the launch plan",
    );
    let width = entry_row("Custom output width (physical pixels)");
    let height = entry_row("Custom output height (physical pixels)");
    let refresh_rate = entry_row("Custom output refresh rate (Hz)");
    for row in [&width, &height, &refresh_rate] {
        row.set_tooltip_text(Some(
            "An output override for the nested Gamescope session, not a desktop mode change",
        ));
    }
    output_group.add(&output);
    output_group.add(&auto_display);
    output_group.add(&width);
    output_group.add(&height);
    output_group.add(&refresh_rate);
    controls.append(&output_group);

    let advanced_group = adw::PreferencesGroup::builder()
        .title("Rendering and presentation")
        .build();
    let advanced = adw::ExpanderRow::builder().use_markup(false).build();
    advanced.set_title("Advanced Gamescope settings");
    advanced.set_subtitle("Render resolution, HDR, adaptive sync and frame rates");
    advanced.add_prefix(&gtk::Image::from_icon_name("preferences-system-symbolic"));
    let custom_render = switch_row(
        "Custom render resolution",
        "Off: render at the Gamescope output resolution",
    );
    let render_width = entry_row("Render width (pixels)");
    let render_height = entry_row("Render height (pixels)");
    render_width.set_tooltip_text(Some(
        "A lower render resolution can be scaled to the output resolution",
    ));
    render_height.set_tooltip_text(Some("Both render dimensions must be set together"));
    let hdr = combo_row(
        "HDR",
        "Auto follows the target display’s current Hyprland HDR color preset",
        &["Auto", "On", "Off"],
    );
    let adaptive_sync = combo_row(
        "Adaptive sync",
        "Auto follows Hyprland’s runtime VRR policy, not momentary VRR activity",
        &["Auto", "On", "Off"],
    );
    let window_mode = combo_row(
        "Window mode",
        "Fullscreen is recommended for a controller-first session",
        &["Fullscreen", "Borderless", "Windowed"],
    );
    let filter = combo_row(
        "Scaling filter",
        "Used when render and output resolutions differ",
        &["Linear", "Nearest", "AMD FSR", "NVIDIA NIS"],
    );
    let framerate_limit = entry_row("Frame-rate limit (FPS; 0 = unlimited)");
    let grab_keyboard = switch_row(
        "Grab keyboard",
        "Requests Gamescope’s keyboard grab; host Hyprland shortcuts remain active on native Wayland",
    );
    let force_grab_cursor = switch_row(
        "Force relative mouse capture",
        "Off: Gamescope switches capture automatically. On: always relative, even in Steam menus; may make pointing awkward.",
    );
    let hide_cursor_delay = entry_row("Hide idle cursor after (milliseconds)");
    hide_cursor_delay.set_tooltip_text(Some(
        "1000 ms helps the cursor get out of the way after switching to a controller; 0 hides it immediately when idle",
    ));
    let mouse_sensitivity = entry_row("Mouse movement multiplier");
    mouse_sensitivity.set_tooltip_text(Some("Positive decimal; 1.0 leaves movement unchanged"));
    advanced.add_row(&custom_render);
    advanced.add_row(&render_width);
    advanced.add_row(&render_height);
    advanced.add_row(&hdr);
    advanced.add_row(&adaptive_sync);
    advanced.add_row(&window_mode);
    advanced.add_row(&filter);
    advanced.add_row(&framerate_limit);
    advanced_group.add(&advanced);
    controls.append(&advanced_group);
    let input_group = adw::PreferencesGroup::new();
    let input = adw::ExpanderRow::builder().use_markup(false).build();
    input.set_title("Controller and mouse switching");
    input.set_subtitle("Automatic capture recommended · keyboard and cursor options");
    input.add_prefix(&gtk::Image::from_icon_name("input-mouse-symbolic"));
    input.add_row(&force_grab_cursor);
    input.add_row(&hide_cursor_delay);
    input.add_row(&mouse_sensitivity);
    input.add_row(&grab_keyboard);
    input.add_row(&action_row(
        "Host shortcuts stay active",
        "Gamescope 3.16.25’s native Wayland backend does not inhibit Hyprland shortcuts, even with keyboard grab. Focus and input switching still depend on Steam, the game and upstream Gamescope; these options are mitigations, not a complete fix. Changes apply to the next new session only.",
    ));
    input_group.add(&input);
    controls.append(&input_group);
    content.append(&controls);

    let copy = gtk::Button::builder()
        .icon_name("edit-copy-symbolic")
        .tooltip_text("Copy the command preview; does not execute it")
        .sensitive(false)
        .build();
    let save = gtk::Button::builder()
        .label("Save settings")
        .sensitive(false)
        .build();
    let command_actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    command_actions.append(&copy);
    command_actions.append(&save);
    let command_group = adw::PreferencesGroup::builder()
        .title("Launch plan")
        .description("Preview only: copying or saving never starts Steam or Gamescope. Launch checks the current display and Steam state again.")
        .header_suffix(&command_actions)
        .build();
    let resolved = action_row("Waiting for detection", "");
    resolved.add_prefix(&gtk::Image::from_icon_name("system-run-symbolic"));
    command_group.add(&resolved);
    let preview = gtk::TextView::builder()
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .wrap_mode(gtk::WrapMode::WordChar)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let preview_scroll = gtk::ScrolledWindow::builder()
        .min_content_height(100)
        .max_content_height(220)
        .propagate_natural_height(true)
        .hscrollbar_policy(gtk::PolicyType::Never)
        .child(&preview)
        .build();
    command_group.add(&gtk::Frame::builder().child(&preview_scroll).build());
    let plan_message = text_label("", "error");
    let warnings = text_label("", "warning");
    let saved_state = text_label("Settings have not been loaded yet.", "dim-label");
    let command_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    command_body.set_margin_top(8);
    command_body.append(&plan_message);
    command_body.append(&warnings);
    command_body.append(&saved_state);
    command_group.add(&command_body);
    content.append(&command_group);

    let integration = adw::PreferencesGroup::builder()
        .title("Steam and your desktop")
        .build();
    integration.add(&action_row(
        "Steam Input and overlay stay with Steam",
        "Steam itself runs inside Gamescope, then launches your games. Controller navigation is provided by Steam Big Picture; these settings and restart confirmations remain desktop controls.",
    ));
    integration.add(&action_row(
        "A nested session, not a desktop replacement",
        "Your Hyprland session stays running. Closing Hyprdeck does not close gaming. Controller activity does not reset desktop idle. Use Hyprland’s gamescope idle_inhibit fullscreen rule to prevent automatic locking during fullscreen play; windowed play is outside that rule’s scope. This launcher does not disable desktop idle globally.",
    ));
    content.append(&integration);

    let page = Rc::new(Page {
        window: ctx.window.downgrade(),
        toasts: ctx.toasts.downgrade(),
        ready: Cell::new(false),
        startup_known: Cell::new(false),
        syncing: Cell::new(false),
        dirty: Cell::new(false),
        busy: Cell::new(false),
        refresh_pending: Cell::new(false),
        detecting: Cell::new(false),
        polling: RefCell::new(SessionPolling::default()),
        poll_timer: RefCell::new(None),
        session: RefCell::new(None),
        session_error: RefCell::new(None),
        preview_busy: Cell::new(false),
        revision: Cell::new(0),
        save_valid: Cell::new(false),
        snapshot: RefCell::new(None),
        plan: RefCell::new(None),
        command: RefCell::new(String::new()),
        outputs: RefCell::new(vec![None]),
        config_error: RefCell::new(None),
        detection_error: RefCell::new(None),
        hero_state,
        launch,
        refresh,
        save,
        copy,
        spinner,
        startup,
        status_group,
        config_message,
        detection_message,
        operation_message,
        defaults,
        saved_state,
        controls,
        display,
        output,
        auto_display,
        width,
        height,
        refresh_rate,
        custom_render,
        render_width,
        render_height,
        hdr,
        adaptive_sync,
        window_mode,
        filter,
        framerate_limit,
        grab_keyboard,
        force_grab_cursor,
        hide_cursor_delay,
        mouse_sensitivity,
        resolved,
        preview,
        plan_message,
        warnings,
    });
    // Only the root owns the state. Child callbacks and background work are weak;
    // the state also holds the window weakly, avoiding a window -> page -> window cycle.
    root.connect_destroy({
        let owner = RefCell::new(Some(page.clone()));
        move |_| {
            if let Some(page) = owner.borrow().as_ref() {
                page.stop_polling();
            }
            owner.borrow_mut().take();
        }
    });
    page.set_settings(&Settings::default());
    page.connect_controls();
    overlay_row.connect_activated({
        let weak = Rc::downgrade(&page);
        move |_| {
            let Some(page) = weak.upgrade() else {
                return;
            };
            let Some(ctx) = page.context() else {
                return;
            };
            let dialog = crate::overlay_page::present(&ctx);
            dialog.connect_closed({
                let weak = weak.clone();
                move |_| {
                    if let Some(page) = weak.upgrade() {
                        page.request_plan();
                    }
                }
            });
        }
    });
    page.refresh.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.refresh_detection();
            }
        }
    });
    page.save.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.save_settings();
            }
        }
    });
    page.launch.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.launch_session();
            }
        }
    });
    page.defaults.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.use_defaults();
            }
        }
    });
    page.copy.connect_clicked({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                let command = page.command.borrow();
                if !command.is_empty() {
                    page.preview.display().clipboard().set_text(&command);
                    if let Some(ctx) = page.context() {
                        ctx.toast("Command copied");
                    }
                }
            }
        }
    });
    ui::on_shown(&root, {
        let page = Rc::downgrade(&page);
        move || {
            if let Some(page) = page.upgrade() {
                page.refresh_detection();
                page.start_polling();
            }
        }
    });
    root.connect_unmap({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.stop_polling();
            }
        }
    });
    root.upcast()
}

impl Page {
    fn start_polling(self: &Rc<Self>) {
        if self.poll_timer.borrow().is_some() {
            return;
        }
        self.polling.borrow_mut().set_mapped(true);
        self.poll_session();
        let weak = Rc::downgrade(self);
        *self.poll_timer.borrow_mut() = Some(glib::timeout_add_seconds_local(2, move || {
            let Some(page) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            page.poll_session();
            glib::ControlFlow::Continue
        }));
    }

    fn stop_polling(&self) {
        self.polling.borrow_mut().set_mapped(false);
        if let Some(timer) = self.poll_timer.borrow_mut().take() {
            timer.remove();
        }
    }

    fn poll_session(self: &Rc<Self>) {
        if self.detecting.get() {
            return;
        }
        let Some(epoch) = self.polling.borrow_mut().begin() else {
            return;
        };
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result =
                rt::blocking(|| engine::session_status().map_err(|error| format!("{error:#}")))
                    .await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            let accepted = page.polling.borrow_mut().complete(epoch);
            if accepted {
                let update = match result {
                    Ok(session) => {
                        let state_changed = page
                            .session
                            .borrow()
                            .as_ref()
                            .is_some_and(|old| old.steam_state() != session.steam_state());
                        let session_changed = page.session.borrow().as_ref() != Some(&session);
                        let recovered = page.session_error.borrow_mut().take().is_some();
                        if session_changed {
                            if let Some(snapshot) = page.snapshot.borrow_mut().as_mut() {
                                snapshot.session = session.clone();
                            }
                            *page.session.borrow_mut() = Some(session);
                        }
                        if state_changed {
                            page.request_plan();
                        }
                        session_changed || recovered
                    }
                    Err(error) => {
                        let changed = page.session_error.borrow().as_ref() != Some(&error);
                        *page.session_error.borrow_mut() = Some(error);
                        changed
                    }
                };
                if update {
                    page.update_session_banner();
                    page.update_controls();
                }
            } else if page.polling.borrow().mapped {
                page.poll_session();
            }
            if !page.busy.get() && page.refresh_pending.get() {
                page.refresh_detection();
            }
        });
    }

    fn update_session_banner(&self) {
        if let Some(error) = self.session_error.borrow().as_ref() {
            self.hero_state.set_label(&format!("Live Steam detection unavailable: {error}\nRetrying while this page is open; no processes were changed."));
            return;
        }
        let session = self.session.borrow();
        let Some(session) = session.as_ref() else {
            self.hero_state
                .set_label("Waiting for Steam session detection…");
            return;
        };
        let status = if session.managed_transition() {
            "Managed Gamescope is starting or exiting; waiting for associated Steam."
        } else {
            match session.steam_state() {
                SteamState::Stopped => "Steam is stopped. Launch a new Big Picture session.",
                SteamState::Desktop => {
                    "Desktop Steam is running. Launch asks before a normal restart."
                }
                SteamState::Gamescope => {
                    "Steam is inside Gamescope. Open it without restarting or applying your draft."
                }
            }
        };
        self.hero_state.set_label(&format!(
            "{status}\nSteam PID(s): {} · Gamescope PID(s): {} · Steam host PID(s): {}",
            pid_list(&session.steam_pids),
            pid_list(&session.gamescope_pids),
            pid_list(&session.steam_scopes),
        ));
    }

    fn context(&self) -> Option<Ctx> {
        Some(Ctx {
            window: self.window.upgrade()?,
            toasts: self.toasts.upgrade()?,
        })
    }

    fn set_startup_enabled(self: &Rc<Self>, on: bool) {
        if self.syncing.get() || self.busy.get() {
            return;
        }
        self.busy.set(true);
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = rt::blocking(move || startup::set_enabled(on)).await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            page.busy.set(false);
            match result {
                Ok(()) => page.set_operation(if on {
                    "Steam Big Picture will launch at the next login using your saved profile."
                } else {
                    "Steam Big Picture will no longer launch at login. The current session is unchanged."
                }, false),
                Err(error) => {
                    // Persistence may have succeeded before daemon-reload failed.
                    // Disable until the saved entry is read again, not guessed.
                    page.startup_known.set(false);
                    page.refresh_pending.set(true);
                    page.set_operation(&format!("Changing Gamescope login launch failed: {error:#}"), true);
                }
            }
            page.finish_operation();
        });
    }

    fn connect_controls(self: &Rc<Self>) {
        self.startup.connect_active_notify({
            let page = Rc::downgrade(self);
            move |row| {
                if let Some(page) = page.upgrade() {
                    page.set_startup_enabled(row.is_active());
                }
            }
        });
        for row in [
            &self.width,
            &self.height,
            &self.refresh_rate,
            &self.render_width,
            &self.render_height,
            &self.framerate_limit,
            &self.hide_cursor_delay,
            &self.mouse_sensitivity,
        ] {
            let page = Rc::downgrade(self);
            row.connect_changed(move |_| {
                if let Some(page) = page.upgrade() {
                    page.changed();
                }
            });
        }
        for row in [
            &self.output,
            &self.hdr,
            &self.adaptive_sync,
            &self.window_mode,
            &self.filter,
        ] {
            let page = Rc::downgrade(self);
            row.connect_selected_notify(move |_| {
                if let Some(page) = page.upgrade() {
                    page.changed();
                }
            });
        }
        for row in [
            &self.auto_display,
            &self.grab_keyboard,
            &self.force_grab_cursor,
        ] {
            let page = Rc::downgrade(self);
            row.connect_active_notify(move |_| {
                if let Some(page) = page.upgrade() {
                    page.changed();
                }
            });
        }
        self.custom_render.connect_active_notify({
            let page = Rc::downgrade(self);
            move |row| {
                if let Some(page) = page.upgrade() {
                    if page.syncing.get() {
                        return;
                    }
                    if row.is_active()
                        && page.render_width.text() == "0"
                        && page.render_height.text() == "0"
                    {
                        let dimensions = page
                            .plan
                            .borrow()
                            .as_ref()
                            .map(|p| (p.width, p.height))
                            .unwrap_or((1920, 1080));
                        page.syncing.set(true);
                        page.render_width.set_text(&dimensions.0.to_string());
                        page.render_height.set_text(&dimensions.1.to_string());
                        page.syncing.set(false);
                    }
                    page.changed();
                }
            }
        });
    }

    fn set_settings(&self, settings: &Settings) {
        self.syncing.set(true);
        self.populate_outputs(settings.output.as_deref());
        self.auto_display.set_active(settings.auto_display);
        self.width.set_text(&settings.width.to_string());
        self.height.set_text(&settings.height.to_string());
        self.refresh_rate
            .set_text(&settings.refresh_rate.to_string());
        self.custom_render
            .set_active(settings.render_width != 0 || settings.render_height != 0);
        self.render_width
            .set_text(&settings.render_width.to_string());
        self.render_height
            .set_text(&settings.render_height.to_string());
        self.hdr.set_selected(toggle_index(settings.hdr));
        self.adaptive_sync
            .set_selected(toggle_index(settings.adaptive_sync));
        self.window_mode.set_selected(match settings.window_mode {
            WindowMode::Fullscreen => 0,
            WindowMode::Borderless => 1,
            WindowMode::Windowed => 2,
        });
        self.filter.set_selected(match settings.filter {
            Filter::Linear => 0,
            Filter::Nearest => 1,
            Filter::Fsr => 2,
            Filter::Nis => 3,
        });
        self.framerate_limit
            .set_text(&settings.framerate_limit.to_string());
        self.grab_keyboard.set_active(settings.grab_keyboard);
        self.force_grab_cursor
            .set_active(settings.force_grab_cursor);
        self.hide_cursor_delay
            .set_text(&settings.hide_cursor_delay.to_string());
        self.mouse_sensitivity
            .set_text(&settings.mouse_sensitivity.to_string());
        self.syncing.set(false);
        self.update_controls();
    }

    fn selected_output(&self) -> Option<String> {
        self.outputs
            .borrow()
            .get(self.output.selected() as usize)
            .cloned()
            .flatten()
    }

    fn populate_outputs(&self, selected: Option<&str>) {
        let mut labels = vec!["Focused display (at launch)".to_owned()];
        let mut values = vec![None];
        if let Some(snapshot) = self.snapshot.borrow().as_ref() {
            for monitor in &snapshot.displays {
                labels.push(format!(
                    "{}{}{}",
                    monitor.name,
                    if monitor.focused { " · focused" } else { "" },
                    if monitor.disabled { " · disabled" } else { "" }
                ));
                values.push(Some(monitor.name.clone()));
            }
        }
        if let Some(selected) = selected
            && !values
                .iter()
                .any(|value| value.as_deref() == Some(selected))
        {
            labels.push(format!("{selected} · unavailable"));
            values.push(Some(selected.to_owned()));
        }
        let index = values
            .iter()
            .position(|value| value.as_deref() == selected)
            .unwrap_or(0);
        *self.outputs.borrow_mut() = values;
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        self.output.set_model(Some(&gtk::StringList::new(&refs)));
        self.output.set_selected(index as u32);
    }

    fn read_settings(&self) -> Result<Settings, String> {
        if !self.ready.get() {
            return Err("Saved settings have not been loaded. Resolve the configuration error before starting a new session.".into());
        }
        let integer = |row: &adw::EntryRow, name: &str| {
            row.text()
                .trim()
                .parse::<u32>()
                .map_err(|_| format!("{name} must be a whole number from 0 to {}.", u32::MAX))
        };
        let refresh_rate = self
            .refresh_rate
            .text()
            .trim()
            .parse::<f64>()
            .map_err(|_| "Refresh rate must be a number in Hz (for example 119.88).".to_owned())?;
        let (render_width, render_height) = if self.custom_render.is_active() {
            (
                integer(&self.render_width, "Render width")?,
                integer(&self.render_height, "Render height")?,
            )
        } else {
            (0, 0)
        };
        Ok(Settings {
            output: self.selected_output(),
            auto_display: self.auto_display.is_active(),
            width: integer(&self.width, "Output width")?,
            height: integer(&self.height, "Output height")?,
            refresh_rate,
            render_width,
            render_height,
            hdr: selected_toggle(&self.hdr),
            adaptive_sync: selected_toggle(&self.adaptive_sync),
            window_mode: match self.window_mode.selected() {
                1 => WindowMode::Borderless,
                2 => WindowMode::Windowed,
                _ => WindowMode::Fullscreen,
            },
            filter: match self.filter.selected() {
                1 => Filter::Nearest,
                2 => Filter::Fsr,
                3 => Filter::Nis,
                _ => Filter::Linear,
            },
            framerate_limit: integer(&self.framerate_limit, "Frame-rate limit")?,
            grab_keyboard: self.grab_keyboard.is_active(),
            force_grab_cursor: self.force_grab_cursor.is_active(),
            hide_cursor_delay: integer(&self.hide_cursor_delay, "Cursor hide delay")?,
            mouse_sensitivity: self
                .mouse_sensitivity
                .text()
                .trim()
                .parse::<f64>()
                .map_err(|_| {
                    "Mouse sensitivity must be a positive decimal (for example 1.0).".to_owned()
                })?,
        })
    }

    fn changed(self: &Rc<Self>) {
        if self.syncing.get() {
            return;
        }
        self.dirty.set(true);
        self.set_operation("", false);
        self.update_display();
        self.request_plan();
    }

    fn update_controls(&self) {
        self.controls
            .set_sensitive(self.ready.get() && !self.busy.get());
        self.startup
            .set_sensitive(self.startup_known.get() && !self.busy.get());
        for row in [&self.width, &self.height, &self.refresh_rate] {
            row.set_sensitive(!self.auto_display.is_active());
        }
        self.render_width
            .set_sensitive(self.custom_render.is_active());
        self.render_height
            .set_sensitive(self.custom_render.is_active());
        self.refresh.set_sensitive(!self.busy.get());
        self.defaults.set_sensitive(!self.busy.get());
        self.save
            .set_sensitive(!self.busy.get() && self.save_valid.get());
        self.copy.set_sensitive(!self.command.borrow().is_empty());
        let existing = self
            .session
            .borrow()
            .as_ref()
            .is_some_and(|s| s.steam_state() == SteamState::Gamescope);
        self.launch.set_label(if existing {
            "Open existing session"
        } else {
            "Launch Big Picture"
        });
        let managed_transition = self
            .session
            .borrow()
            .as_ref()
            .is_some_and(SessionStatus::managed_transition);
        self.launch.set_sensitive(
            !self.busy.get()
                && self.session_error.borrow().is_none()
                && (existing || (!managed_transition && self.plan.borrow().is_some())),
        );
        self.spinner
            .set_visible(self.busy.get() || self.preview_busy.get());
        self.saved_state.set_label(if !self.ready.get() {
            "Saved settings are unavailable; the original file has not been changed."
        } else if self.dirty.get() {
            "Unsaved changes · Save settings or launch to persist valid choices."
        } else {
            "Settings ready · Automatic display choices are resolved again at launch."
        });
    }

    fn update_status(&self) {
        let config_error = self.config_error.borrow();
        self.config_message.set_visible(config_error.is_some());
        if let Some(error) = config_error.as_ref() {
            self.config_message.set_label(&format!("Could not read gamescope.toml: {error}\n{}", if self.ready.get() {
                "You explicitly chose an unsaved defaults draft. Save or a new launch will replace the unreadable file."
            } else { "The file has been preserved. Repair it and refresh, or explicitly choose defaults." }));
        }
        self.defaults
            .set_visible(config_error.is_some() && !self.ready.get());
        let detection_error = self.detection_error.borrow();
        self.detection_message
            .set_visible(detection_error.is_some());
        if let Some(error) = detection_error.as_ref() {
            self.detection_message.set_label(&format!("Detection failed: {error}\nRefresh to retry. No new session can start until detection succeeds."));
        }
        self.status_group.set_visible(
            config_error.is_some()
                || detection_error.is_some()
                || !self.operation_message.text().is_empty(),
        );
    }

    fn set_operation(&self, message: &str, error: bool) {
        self.operation_message.set_label(message);
        self.operation_message.set_visible(!message.is_empty());
        if error {
            self.operation_message.add_css_class("error");
        } else {
            self.operation_message.remove_css_class("error");
        }
        self.update_status();
    }

    fn update_display(&self) {
        self.update_session_banner();
        let snapshot = self.snapshot.borrow();
        let Some(snapshot) = snapshot.as_ref() else {
            self.display.set_title("Display detection unavailable");
            self.display
                .set_subtitle("Refresh detection to read the current Hyprland outputs.");
            return;
        };
        let selected = self.selected_output();
        let monitor = snapshot.displays.iter().find(|m| match selected.as_ref() {
            Some(name) => &m.name == name,
            None => m.focused && !m.disabled,
        });
        if let Some(monitor) = monitor {
            let (width, height) = physical_size(monitor);
            self.display.set_title(&format!(
                "{}{}",
                monitor.name,
                if monitor.focused { " · focused" } else { "" }
            ));
            let vrr_policy = match snapshot.vrr_policy {
                0 => "off",
                1 => "on",
                2 => "fullscreen only",
                3 => "fullscreen video and games",
                _ => "unknown",
            };
            self.display.set_subtitle(&format!(
                "{}\n{} × {} physical pixels · {:.3} Hz detected · {:.2}× desktop scale\n{} · HDR preset: {} · VRR policy: {} (currently {})",
                if monitor.description.is_empty() { &monitor.name } else { &monitor.description },
                width, height, monitor.refresh_rate, monitor.scale,
                if monitor.disabled { "Display disabled" } else if !monitor.dpms_status { "Display asleep" } else { "Display active" },
                if monitor.color_management_preset.is_empty() { "SDR/default" } else { &monitor.color_management_preset },
                vrr_policy, if monitor.vrr { "active" } else { "inactive" },
            ));
        } else {
            self.display
                .set_title(selected.as_deref().unwrap_or("No focused display"));
            self.display.set_subtitle("This output is unavailable. Choose a connected display or refresh detection; saved choices have not been reset.");
        }
    }

    fn refresh_detection(self: &Rc<Self>) {
        if self.busy.get() || self.polling.borrow().in_flight {
            self.refresh_pending.set(true);
            return;
        }
        self.busy.set(true);
        self.refresh_pending.set(false);
        self.detecting.set(true);
        let load_config = !self.ready.get();
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (snapshot, settings, login_enabled) = rt::blocking(move || {
                (
                    engine::snapshot().map_err(|e| format!("{e:#}")),
                    load_config.then(|| Settings::load().map_err(|e| format!("{e:#}"))),
                    startup::enabled().map_err(|e| format!("{e:#}")),
                )
            })
            .await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            match login_enabled {
                Ok(on) => {
                    page.startup_known.set(true);
                    page.syncing.set(true);
                    page.startup.set_active(on);
                    page.syncing.set(false);
                    page.startup.set_subtitle("Off by default. Uses your saved launch profile at the next Hyprland login; toggling does not start or stop Steam.");
                }
                Err(error) => {
                    page.startup_known.set(false);
                    page.startup
                        .set_subtitle(&format!("Cannot read login launch settings: {error}"));
                }
            }
            let selected = page.selected_output();
            match snapshot {
                Ok(snapshot) => {
                    *page.session.borrow_mut() = Some(snapshot.session.clone());
                    *page.session_error.borrow_mut() = None;
                    *page.snapshot.borrow_mut() = Some(snapshot);
                    *page.detection_error.borrow_mut() = None;
                }
                Err(error) => {
                    *page.snapshot.borrow_mut() = None;
                    *page.detection_error.borrow_mut() = Some(error);
                }
            }
            page.syncing.set(true);
            page.populate_outputs(selected.as_deref());
            page.syncing.set(false);
            if let Some(settings) = settings {
                match settings {
                    Ok(settings) => {
                        page.ready.set(true);
                        page.dirty.set(false);
                        *page.config_error.borrow_mut() = None;
                        page.set_settings(&settings);
                    }
                    Err(error) => {
                        *page.config_error.borrow_mut() = Some(error);
                    }
                }
            }
            page.busy.set(false);
            page.detecting.set(false);
            page.update_status();
            page.update_display();
            page.request_plan();
            page.finish_operation();
        });
    }

    fn request_plan(self: &Rc<Self>) {
        self.revision.set(self.revision.get().wrapping_add(1));
        self.plan.borrow_mut().take();
        self.command.borrow_mut().clear();
        self.save_valid.set(false);
        self.preview.buffer().set_text("");
        self.warnings.set_visible(false);
        self.resolved.set_title("Updating launch plan…");
        self.resolved.set_subtitle("");
        self.update_controls();
        if !self.preview_busy.get() {
            self.compute_plan();
        }
    }

    fn compute_plan(self: &Rc<Self>) {
        let settings = match self.read_settings() {
            Ok(settings) => settings,
            Err(error) => {
                self.show_plan_error(&error);
                return;
            }
        };
        let Some(snapshot) = self.snapshot.borrow().clone() else {
            self.show_plan_error(
                "Current display and Steam detection is unavailable. Refresh to retry.",
            );
            return;
        };
        self.preview_busy.set(true);
        self.plan_message.set_visible(false);
        self.update_controls();
        let revision = self.revision.get();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let (plan, save_valid) = rt::blocking(move || {
                let plan = engine::plan(&settings, &snapshot);
                // Saving is useful before dependencies are installed. Still use the
                // real planner to validate the display and every setting; ignore only
                // dependency availability for persistence, never for launching.
                let save_valid = if plan.is_ok() {
                    true
                } else if !snapshot.gamescope_available || !snapshot.steam_available {
                    let mut snapshot = snapshot;
                    snapshot.gamescope_available = true;
                    snapshot.steam_available = true;
                    engine::plan(&settings, &snapshot).is_ok()
                } else {
                    false
                };
                (
                    plan.and_then(Plan::with_overlay)
                        .map(|plan| {
                            let command = plan.command_line();
                            (plan, command)
                        })
                        .map_err(|error| format!("{error:#}")),
                    save_valid,
                )
            })
            .await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            page.preview_busy.set(false);
            if page.revision.get() != revision {
                page.compute_plan();
                return;
            }
            page.save_valid.set(save_valid);
            match plan {
                Ok((plan, command)) => {
                    page.resolved.set_title(&format!(
                        "{} · {} × {} @ {} Hz",
                        plan.output, plan.width, plan.height, plan.refresh_rate
                    ));
                    page.resolved.set_subtitle(&format!(
                        "HDR {} · adaptive sync {} · Steam launched inside Gamescope",
                        if plan.hdr { "on" } else { "off" },
                        if plan.adaptive_sync { "on" } else { "off" }
                    ));
                    page.preview.buffer().set_text(&command);
                    *page.command.borrow_mut() = command;
                    page.warnings.set_label(&plan.warnings.join("\n"));
                    page.warnings.set_visible(!plan.warnings.is_empty());
                    page.plan_message.set_visible(false);
                    *page.plan.borrow_mut() = Some(plan);
                }
                Err(error) => {
                    page.show_plan_error(&error);
                }
            }
            page.update_controls();
        });
    }

    fn show_plan_error(&self, error: &str) {
        self.resolved.set_title("New launch is not ready");
        self.resolved
            .set_subtitle("An existing Steam Gamescope session can still be opened.");
        self.plan_message.set_label(error);
        self.plan_message.set_visible(true);
        self.update_controls();
    }

    fn save_settings(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let settings = match self.read_settings() {
            Ok(settings) => settings,
            Err(error) => {
                self.set_operation(&error, true);
                return;
            }
        };
        let Some(mut snapshot) = self.snapshot.borrow().clone() else {
            return;
        };
        snapshot.gamescope_available = true;
        snapshot.steam_available = true;
        self.busy.set(true);
        self.set_operation("Saving settings…", false);
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = rt::blocking(move || {
                engine::plan(&settings, &snapshot)?;
                settings.save()
            })
            .await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            page.busy.set(false);
            match result {
                Ok(()) => {
                    page.dirty.set(false);
                    *page.config_error.borrow_mut() = None;
                    page.set_operation("Settings saved. No gaming processes were started.", false);
                }
                Err(error) => {
                    page.set_operation(&format!("Saving settings failed: {error:#}"), true)
                }
            }
            page.finish_operation();
        });
    }

    fn launch_session(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let state = match self.session.borrow().as_ref() {
            Some(session) if self.session_error.borrow().is_none() => session.steam_state(),
            _ => return,
        };
        let settings = match self.read_settings() {
            Ok(settings) => settings,
            // Focus is deliberately independent of settings or missing dependencies.
            Err(_) if state == SteamState::Gamescope => Settings::default(),
            Err(error) => {
                self.set_operation(&error, true);
                return;
            }
        };
        if state != SteamState::Gamescope && self.plan.borrow().is_none() {
            return;
        }
        let Some(ctx) = self.context() else {
            return;
        };
        self.busy.set(true);
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let restart_steam = if state == SteamState::Desktop {
                ctx.confirm(
                    "Restart Steam inside Gamescope?",
                    "Steam is currently running on your desktop. Steam will receive a normal shutdown request before restarting in Big Picture inside Gamescope. Running games will close and downloads may be interrupted. Nothing will be force-killed. Your Hyprland session stays open.",
                    "Restart Steam", true,
                ).await
            } else {
                false
            };
            drop(ctx);
            if state == SteamState::Desktop && !restart_steam {
                if let Some(page) = weak.upgrade() {
                    page.busy.set(false);
                    page.set_operation("Launch cancelled. Desktop Steam was left running.", false);
                    page.finish_operation();
                }
                return;
            }
            // Do not start gaming after the owning page/window has disappeared.
            let Some(page) = weak.upgrade() else {
                return;
            };
            page.set_operation(
                if state == SteamState::Gamescope {
                    "Opening the existing session…"
                } else {
                    "Starting Steam inside Gamescope…"
                },
                false,
            );
            drop(page);
            let result = rt::blocking(move || {
                if state == SteamState::Gamescope {
                    engine::open_existing()
                } else {
                    engine::launch(&settings, restart_steam)
                }
            })
            .await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            page.busy.set(false);
            match result {
                Ok(LaunchOutcome::Launched) => {
                    page.dirty.set(false);
                    *page.config_error.borrow_mut() = None;
                    page.set_operation("Started Steam Big Picture inside Gamescope.", false);
                }
                Ok(LaunchOutcome::Focused) => {
                    page.set_operation("Opened the existing Steam Gamescope session. Your draft settings were not changed or saved.", false);
                }
                Err(error) => page.set_operation(&format!("Launch failed: {error:#}"), true),
            }
            page.refresh_pending.set(true);
            page.finish_operation();
        });
    }

    fn use_defaults(self: &Rc<Self>) {
        if self.busy.get() {
            return;
        }
        let Some(ctx) = self.context() else {
            return;
        };
        self.busy.set(true);
        self.update_controls();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let consent = ctx.confirm(
                "Use a new defaults draft?",
                "Your saved configuration could not be read. It will not be overwritten now. Choosing defaults creates an editable draft; Save settings or a new launch will replace the unreadable configuration with these choices.",
                "Use defaults", false,
            ).await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            page.busy.set(false);
            if consent {
                page.ready.set(true);
                page.dirty.set(true);
                page.set_settings(&Settings::default());
                page.set_operation(
                    "Defaults are an unsaved draft. The unreadable file is still preserved.",
                    false,
                );
                page.update_display();
                page.request_plan();
            }
            page.finish_operation();
        });
    }

    fn finish_operation(self: &Rc<Self>) {
        self.update_status();
        self.update_controls();
        if self.refresh_pending.replace(false) {
            self.refresh_detection();
        }
    }
}

fn text_label(text: &str, class: &str) -> gtk::Label {
    gtk::Label::builder()
        .label(text)
        .use_markup(false)
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .css_classes([class])
        .build()
}

fn action_row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_subtitle_lines(0);
    row
}

fn entry_row(title: &str) -> adw::EntryRow {
    let row = adw::EntryRow::builder().use_markup(false).build();
    row.set_title(title);
    row
}

fn combo_row(title: &str, subtitle: &str, choices: &[&str]) -> adw::ComboRow {
    let row = adw::ComboRow::builder()
        .use_markup(false)
        .model(&gtk::StringList::new(choices))
        .build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

fn switch_row(title: &str, subtitle: &str) -> adw::SwitchRow {
    let row = adw::SwitchRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row
}

fn toggle_index(toggle: Toggle) -> u32 {
    match toggle {
        Toggle::Auto => 0,
        Toggle::On => 1,
        Toggle::Off => 2,
    }
}

fn selected_toggle(row: &adw::ComboRow) -> Toggle {
    match row.selected() {
        1 => Toggle::On,
        2 => Toggle::Off,
        _ => Toggle::Auto,
    }
}

fn physical_size(monitor: &Monitor) -> (i64, i64) {
    if monitor.transform.rem_euclid(2) == 1 {
        (monitor.height, monitor.width)
    } else {
        (monitor.width, monitor.height)
    }
}

fn pid_list(pids: &[u32]) -> String {
    if pids.is_empty() {
        "none".into()
    } else {
        pids.iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::SessionPolling;

    #[test]
    fn hidden_pages_never_begin_polls_and_slow_polls_do_not_overlap() {
        let mut polling = SessionPolling::default();
        assert!(polling.begin().is_none());
        polling.set_mapped(true);
        let epoch = polling.begin().unwrap();
        assert!(polling.begin().is_none());
        assert!(polling.complete(epoch));
        assert!(polling.begin().is_some());
    }

    #[test]
    fn hiding_discards_observations_even_after_a_quick_remap() {
        let mut polling = SessionPolling::default();
        polling.set_mapped(true);
        let old_epoch = polling.begin().unwrap();
        polling.set_mapped(false);
        polling.set_mapped(true);
        assert!(polling.begin().is_none());
        assert!(!polling.complete(old_epoch));
        let fresh_epoch = polling.begin().unwrap();
        polling.set_mapped(false);
        assert!(!polling.complete(fresh_epoch));
        assert!(polling.begin().is_none());
    }
}
