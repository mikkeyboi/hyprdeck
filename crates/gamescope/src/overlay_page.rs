//! Native customization with a real MangoApp preview and opt-in live saves.
use crate::{
    overlay::{self, Settings},
    overlay_controls::Controls,
    overlay_preview::{self, Message, Preview},
};
use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::{
    rt,
    ui::{self, Ctx},
};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    time::Duration,
};

struct Page {
    editor: gtk::TextBuffer,
    enabled: gtk::Switch,
    live: gtk::Switch,
    controls: Controls,
    picture: gtk::Picture,
    status: gtk::Label,
    applied: gtk::Label,
    save: gtk::Button,
    start: gtk::Button,
    stop: gtk::Button,
    refresh: gtk::SpinButton,
    quality: gtk::DropDown,
    ready: Cell<bool>,
    saving: Cell<bool>,
    revision: Cell<u64>,
    generation: Cell<u64>,
    preview: RefCell<Option<Preview>>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let body = gtk::Box::new(gtk::Orientation::Vertical, 12);
    body.set_margin_top(12);
    body.set_margin_bottom(12);
    body.set_margin_start(12);
    body.set_margin_end(12);
    let root = adw::BreakpointBin::builder()
        .child(&body)
        .width_request(300)
        .height_request(480)
        .build();
    let heading = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let title = gtk::Label::builder()
        .label("Gaming overlay")
        .xalign(0.)
        .hexpand(true)
        .ellipsize(gtk::pango::EllipsizeMode::End)
        .build();
    title.add_css_class("title-2");
    heading.append(&title);
    let save = gtk::Button::with_label("Save overlay");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    heading.append(&save);
    body.append(&heading);

    // These controls belong to the workspace, never either settings scroller.
    let switches = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let enabled = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    let live = gtk::Switch::builder()
        .valign(gtk::Align::Center)
        .sensitive(false)
        .build();
    for (label, switch, explanation) in [
        (
            "_Enabled",
            &enabled,
            "Enable performance overlay. Save overlay commits this switch for the next Gamescope session; changing it never restarts a running game.",
        ),
        (
            "_Live saves",
            &live,
            "Apply appearance edits live: automatically save appearance after a short delay. Already-enabled Hyprdeck MangoApp sessions reload it without restarting Steam or the game. The enable switch still requires Save overlay.",
        ),
    ] {
        let toggle = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let label = gtk::Label::new(Some(label));
        label.set_use_underline(true);
        label.set_mnemonic_widget(Some(switch));
        toggle.set_tooltip_text(Some(explanation));
        toggle.append(&label);
        toggle.append(switch);
        switches.append(&toggle);
    }
    body.append(&switches);
    let applied = gtk::Label::builder()
        .label("Draft edits preview immediately. Save overlay commits settings; live saves are opt-in.")
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    applied.add_css_class("dim-label");
    body.append(&applied);

    let workspace = gtk::Box::new(gtk::Orientation::Horizontal, 24);
    workspace.set_vexpand(true);
    body.append(&workspace);
    let preview_column = gtk::Box::new(gtk::Orientation::Vertical, 8);
    preview_column.set_width_request(320);
    preview_column.set_hexpand(false);
    preview_column.set_valign(gtk::Align::Start);
    workspace.append(&preview_column);
    let preview_heading = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    let preview_title = gtk::Label::builder()
        .label("Live preview")
        .xalign(0.)
        .hexpand(true)
        .build();
    preview_title.add_css_class("heading");
    preview_heading.append(&preview_title);
    preview_title.set_tooltip_text(Some("Real MangoApp over a 3D scene, scaled to fit. MangoApp reports Gamescope frame timing and hardware sensors independently of Steam’s FPS counter."));
    let start = gtk::Button::with_label("Start 3D preview");
    let stop = gtk::Button::with_label("Stop");
    for button in [&start, &stop] {
        button.set_sensitive(false);
        button.set_valign(gtk::Align::Center);
        preview_heading.append(button);
    }
    preview_column.append(&preview_heading);
    let preview_body = gtk::Box::new(gtk::Orientation::Vertical, 8);
    preview_column.append(&preview_body);
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .build();
    let aspect = gtk::AspectFrame::builder()
        .ratio(16. / 9.)
        .obey_child(false)
        .height_request(180)
        .hexpand(true)
        .child(&picture)
        .build();
    preview_body.append(
        &gtk::Frame::builder()
            .child(&aspect)
            .hexpand(true)
            .css_classes(["card"])
            .build(),
    );
    let preview_options = gtk::Box::new(gtk::Orientation::Vertical, 6);
    preview_options.set_valign(gtk::Align::Center);
    let refresh_label = gtk::Label::builder()
        .label("Virtual refresh (Hz)")
        .xalign(0.)
        .build();
    preview_options.append(&refresh_label);
    let refresh = gtk::SpinButton::with_range(30., 360., 1.);
    refresh.set_value(120.);
    refresh.set_tooltip_text(Some(
        "Applies when starting the preview; this is not a game FPS limit or benchmark",
    ));
    refresh_label.set_mnemonic_widget(Some(&refresh));
    preview_options.append(&refresh);
    let quality_label = gtk::Label::builder().label("Image size").xalign(0.).build();
    preview_options.append(&quality_label);
    let quality = gtk::DropDown::from_strings(&["480 × 270", "640 × 360", "960 × 540"]);
    quality.set_tooltip_text(Some("Smooth: 480 × 270; balanced: 640 × 360; detailed: 960 × 540. Applies when starting; smaller captures reduce PNG overhead. Capture targets up to 60 FPS independently of virtual refresh; speed depends on hardware."));
    quality_label.set_mnemonic_widget(Some(&quality));
    preview_options.append(&quality);
    preview_body.append(&preview_options);
    let status = gtk::Label::builder()
        .label("Preview is stopped. It never launches or restarts Steam.")
        .wrap(true)
        .xalign(0.)
        .max_width_chars(42)
        .selectable(true)
        .build();
    preview_column.append(&status);

    let settings = gtk::Box::new(gtk::Orientation::Vertical, 8);
    settings.set_hexpand(true);
    settings.set_vexpand(true);
    workspace.append(&settings);
    let tabs = gtk::Stack::builder()
        .hexpand(true)
        .vexpand(true)
        .hhomogeneous(false)
        .vhomogeneous(false)
        .build();
    let switcher = gtk::StackSwitcher::builder().stack(&tabs).build();
    settings.append(&switcher);
    settings.append(&tabs);

    let native = gtk::Box::new(gtk::Orientation::Vertical, 8);
    native.set_sensitive(false);
    let navigation = gtk::Box::new(gtk::Orientation::Vertical, 6);
    native.append(&navigation);
    let (native_scroll, graphical) = ui::page_scaffold();
    graphical.set_margin_top(8);
    native.append(&native_scroll);
    let editor = gtk::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let controls = Controls::build(&graphical, &navigation, &native_scroll, &editor.buffer());
    tabs.add_titled(&native, Some("controls"), "Controls");

    let configuration = gtk::Box::new(gtk::Orientation::Vertical, 12);
    configuration.set_sensitive(false);
    let extras_popover = gtk::Popover::new();
    let editor_extras = gtk::MenuButton::builder()
        .label("Presets & reference")
        .halign(gtk::Align::Start)
        .popover(&extras_popover)
        .build();
    let extras_content = gtk::Box::new(gtk::Orientation::Vertical, 8);
    extras_popover.set_child(Some(
        &gtk::ScrolledWindow::builder()
            .hscrollbar_policy(gtk::PolicyType::Never)
            .min_content_width(260)
            .max_content_width(360)
            .propagate_natural_width(true)
            .min_content_height(160)
            .max_content_height(360)
            .propagate_natural_height(true)
            .child(&extras_content)
            .build(),
    ));
    configuration.append(&editor_extras);
    let presets = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .column_spacing(8)
        .row_spacing(8)
        .max_children_per_line(3)
        .min_children_per_line(1)
        .build();
    let fps = gtk::Button::with_label("FPS only");
    let sensors = gtk::Button::with_label("FPS + temperatures");
    let detailed = gtk::Button::with_label("Detailed");
    for button in [&fps, &sensors, &detailed] {
        presets.insert(button, -1);
    }
    let preset_group = adw::PreferencesGroup::builder()
        .title("Replace with preset")
        .description("Replaces the entire draft, including custom options. Live saves, when enabled, save the replacement too.")
        .build();
    preset_group.add(&presets);
    extras_content.append(&preset_group);
    let reference = gtk::LinkButton::with_label(
        "https://github.com/flightlessmango/MangoHud/blob/master/data/MangoHud.conf",
        "Native configuration reference",
    );
    reference.set_halign(gtk::Align::Start);
    extras_content.append(&reference);
    let editor_note = gtk::Label::builder()
        .label("Lossless native MangoHUD syntax. Graphical controls edit this same draft and preserve unrelated lines, ordering and repeated custom text. Version-specific options depend on the installed MangoHUD. Warning: exec executes commands as your user.")
        .wrap(true)
        .xalign(0.)
        .build();
    editor_note.add_css_class("dim-label");
    extras_content.append(&editor_note);
    editor.set_tooltip_text(Some("Lossless native MangoHUD draft. Graphical controls edit the same text. Warning: exec executes commands as your user."));
    // Only the editor scrolls here; there is no nested long preferences scroller.
    configuration.append(
        &gtk::ScrolledWindow::builder()
            .vexpand(true)
            .child(&editor)
            .css_classes(["card"])
            .build(),
    );
    tabs.add_titled(&configuration, Some("configuration"), "Editor");
    let (updates_scroll, updates_content) = ui::page_scaffold();
    updates_content.append(&hd_updates::overlay_packages(ctx));
    tabs.add_titled(&updates_scroll, Some("updates"), "Updates");

    // In a narrow content pane keep a compact preview pinned above the settings.
    // Neither breakpoint reparents widgets nor hides the active preview.
    let compact = adw::Breakpoint::new(
        adw::BreakpointCondition::parse("max-width: 760sp").expect("valid condition"),
    );
    compact.add_setter(
        &workspace,
        "orientation",
        Some(&gtk::Orientation::Vertical.to_value()),
    );
    compact.add_setter(&workspace, "spacing", Some(&12i32.to_value()));
    compact.add_setter(&preview_column, "width-request", Some(&(-1i32).to_value()));
    compact.add_setter(
        &preview_body,
        "orientation",
        Some(&gtk::Orientation::Horizontal.to_value()),
    );
    compact.add_setter(&aspect, "height-request", Some(&112i32.to_value()));
    compact.add_setter(&start, "label", Some(&"Start preview".to_value()));
    root.add_breakpoint(compact);

    let page = Rc::new(Page {
        editor: editor.buffer(),
        enabled,
        live,
        controls,
        picture,
        status,
        applied,
        save,
        start,
        stop,
        refresh,
        quality,
        ready: Cell::new(false),
        saving: Cell::new(false),
        revision: Cell::new(0),
        generation: Cell::new(0),
        preview: RefCell::new(None),
    });
    // Only the root retains Page. Page never retains the root; child callbacks
    // and asynchronous workers use weak references, including during startup.
    root.connect_destroy({
        let owner = RefCell::new(Some(page.clone()));
        move |_| {
            if let Some(page) = owner.borrow_mut().take() {
                page.stop_preview();
            }
        }
    });
    root.connect_unmap({
        let weak = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = weak.upgrade() {
                page.stop_preview();
            }
        }
    });
    page.connect();
    for (button, config) in [
        (fps, "fps=1\nfps_text=FPS\nhide_fps_superscript=1\ngpu_stats=0\ncpu_stats=0\nframetime=0\nframe_timing=0\nmangoapp_steam=1\nposition=top-left\nfont_size=24\n".to_owned()),
        (sensors, overlay::DEFAULT_CONFIG.to_owned()),
        (detailed, format!("{}frametime=1\nframe_timing=1\nvram\nram\nrefresh_rate\nresolution\nhdr\nfsr\n", overlay::DEFAULT_CONFIG)),
    ] {
        button.connect_clicked({
            let weak = Rc::downgrade(&page);
            let extras_popover = extras_popover.downgrade();
            move |_| {
                if let Some(page) = weak.upgrade() && page.ready.get() {
                    page.editor.set_text(&config);
                    if let Some(popover) = extras_popover.upgrade() {
                        popover.popdown();
                    }
                }
            }
        });
    }
    glib::spawn_future_local({
        let weak = Rc::downgrade(&page);
        async move {
            let loaded = rt::blocking(Settings::load).await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            match loaded {
                Ok(settings) => {
                    page.enabled.set_active(settings.enabled);
                    page.editor.set_text(&settings.config);
                    page.ready.set(true);
                    page.enabled.set_sensitive(true);
                    page.live.set_sensitive(true);
                    native.set_sensitive(true);
                    configuration.set_sensitive(true);
                    page.save.set_sensitive(true);
                    page.start.set_sensitive(true);
                }
                Err(error) => page.applied.set_label(&format!("Could not load overlay.toml: {error:#}. The original file is preserved; repair it and restart Hyprdeck.")),
            }
        }
    });
    root.upcast()
}

impl Page {
    fn text(&self) -> String {
        self.editor
            .text(&self.editor.start_iter(), &self.editor.end_iter(), true)
            .into()
    }
    fn connect(self: &Rc<Self>) {
        self.editor.connect_changed({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.changed();
                }
            }
        });
        self.live.connect_active_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                if let Some(page) = weak.upgrade()
                    && page.ready.get()
                    && row.is_active()
                {
                    page.changed();
                }
            }
        });
        self.start.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.start_preview();
                }
            }
        });
        self.stop.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.stop_preview();
                }
            }
        });
        self.save.connect_clicked({
            let weak = Rc::downgrade(self);
            move |_| {
                if let Some(page) = weak.upgrade() {
                    page.persist(false);
                }
            }
        });
    }
    fn persist(self: &Rc<Self>, appearance_only: bool) {
        if !self.ready.get() || self.saving.get() {
            return;
        }
        let settings = Settings {
            enabled: self.enabled.is_active(),
            config: self.text(),
        };
        let revision = self.revision.get();
        self.saving.set(true);
        self.save.set_sensitive(false);
        self.applied.set_label("Saving appearance…");
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result = rt::blocking(move || {
                    if appearance_only {
                        overlay::save_appearance(&settings.config)
                    } else {
                        settings.save()
                    }
                })
                .await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                page.saving.set(false);
                page.save.set_sensitive(true);
                match result {
                    Ok(()) => {
                        page.applied.set_label(if page.revision.get() == revision {
                            if appearance_only { "Appearance saved live. Enable changes still require Save overlay." }
                            else { "Saved. Appearance reloads live; enable changes apply to the next session." }
                        } else { "Saved an earlier revision; the current draft has newer changes." });
                        // Single-flight persistence: an older worker never overwrites a newer draft.
                        if page.live.is_active() && page.revision.get() != revision {
                            page.persist(true);
                        }
                    }
                    Err(error) => page
                        .applied
                        .set_label(&format!("Saving overlay failed: {error:#}")),
                }
            }
        });
    }
    fn changed(self: &Rc<Self>) {
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        // TextBuffer::set_text emits separate delete/insert changes. Synchronize
        // once the originating GTK control has finished emitting its signals.
        glib::idle_add_local_once({
            let weak = Rc::downgrade(self);
            move || {
                if let Some(page) = weak.upgrade()
                    && page.revision.get() == revision
                {
                    page.controls.sync(&page.text());
                }
            }
        });
        let generation = self.generation.get();
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                glib::timeout_future(Duration::from_millis(400)).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if !page.ready.get() || page.revision.get() != revision {
                    return;
                }
                if page.live.is_active() {
                    page.persist(true);
                }
                if page.generation.get() != generation {
                    return;
                }
                if let Some(preview) = page.preview.borrow().as_ref()
                    && let Err(error) = preview.update(page.text())
                {
                    page.status
                        .set_label(&format!("Updating preview failed: {error:#}"));
                }
            }
        });
    }
    fn stop_preview(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.preview.borrow_mut().take();
        self.start.set_sensitive(self.ready.get());
        self.stop.set_sensitive(false);
        self.refresh.set_sensitive(true);
        self.quality.set_sensitive(true);
        self.picture.set_paintable(gtk::gdk::Paintable::NONE);
        self.status
            .set_label("Preview stopped. No preview rendering continues in the background.");
    }
    fn start_preview(self: &Rc<Self>) {
        if !self.ready.get() {
            return;
        }
        self.stop_preview();
        self.start.set_sensitive(false);
        self.stop.set_sensitive(true);
        self.refresh.set_sensitive(false);
        self.quality.set_sensitive(false);
        self.status
            .set_label("Starting Gamescope, the 3D scene and MangoApp…");
        let generation = self.generation.get();
        let config = self.text();
        let refresh = self.refresh.value() as u32;
        let quality = self.quality.selected() as usize;
        glib::spawn_future_local({
            let weak = Rc::downgrade(self);
            async move {
                let result =
                    rt::blocking(move || overlay_preview::start(config, refresh, quality)).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if page.generation.get() != generation {
                    return;
                }
                let (preview, rx) = match result {
                    Ok(value) => value,
                    Err(error) => {
                        page.stop_preview();
                        page.status
                            .set_label(&format!("Preview unavailable: {error:#}"));
                        return;
                    }
                };
                *page.preview.borrow_mut() = Some(preview);
                page.changed();
                drop(page);
                let mut first = true;
                while let Ok(message) = rx.recv().await {
                    let Some(page) = weak.upgrade() else {
                        break;
                    };
                    if page.generation.get() != generation {
                        break;
                    }
                    match message {
                        Message::Frame {
                            width,
                            height,
                            pixels,
                        } => {
                            let texture = gtk::gdk::MemoryTexture::new(
                                width,
                                height,
                                gtk::gdk::MemoryFormat::R8g8b8a8,
                                &glib::Bytes::from_owned(pixels),
                                width as usize * 4,
                            );
                            page.picture.set_paintable(Some(&texture));
                            if first {
                                page.status.set_label("Live MangoApp · draft edits hot-reload · leaving this page stops preview");
                                first = false;
                            }
                        }
                        Message::Error(error) => {
                            page.stop_preview();
                            page.status.set_label(&format!("Preview failed: {error}"));
                            break;
                        }
                    }
                }
                if let Some(page) = weak.upgrade()
                    && page.generation.get() == generation
                {
                    page.stop_preview();
                }
            }
        });
    }
}
