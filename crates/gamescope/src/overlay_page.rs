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
    enabled: adw::SwitchRow,
    live: adw::SwitchRow,
    controls: Controls,
    picture: gtk::Picture,
    status: gtk::Label,
    applied: gtk::Label,
    save: gtk::Button,
    start: gtk::Button,
    stop: gtk::Button,
    refresh: adw::SpinRow,
    quality: adw::ComboRow,
    ready: Cell<bool>,
    saving: Cell<bool>,
    revision: Cell<u64>,
    generation: Cell<u64>,
    preview: RefCell<Option<Preview>>,
}

pub fn present(ctx: &Ctx) -> adw::Dialog {
    let dialog = adw::Dialog::builder()
        .title("Performance overlay")
        .content_width(1000)
        .content_height(900)
        .build();
    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    let save = gtk::Button::with_label("Save overlay");
    save.add_css_class("suggested-action");
    save.set_sensitive(false);
    header.pack_end(&save);
    toolbar.add_top_bar(&header);
    let (root, content) = ui::page_scaffold();
    toolbar.set_content(Some(&root));
    dialog.set_child(Some(&toolbar));
    let group = adw::PreferencesGroup::builder().title("MangoHUD / MangoApp")
        .description("Native controls without launching GOverlay. MangoApp reports Gamescope frame timing and hardware sensors; it is independent of Steam’s FPS counter.").build();
    let enabled = adw::SwitchRow::builder().title("Enable performance overlay")
        .subtitle("Save overlay to change the next launch. Enabling/disabling requires a new Gamescope session; appearance changes do not.").build();
    let live = adw::SwitchRow::builder().title("Apply appearance edits live")
        .subtitle("Automatically saves appearance after a short delay. Already-enabled Hyprdeck MangoApp sessions reload the file without restarting Steam or the game.").build();
    group.add(&enabled);
    group.add(&live);
    let applied = gtk::Label::builder()
        .label("Appearance is only saved with Save overlay unless live application is enabled.")
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    group.add(&applied);
    content.append(&group);

    let start = gtk::Button::with_label("Start 3D preview");
    let stop = gtk::Button::with_label("Stop");
    stop.set_sensitive(false);
    start.set_sensitive(false);
    start.set_valign(gtk::Align::Center);
    stop.set_valign(gtk::Align::Center);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.append(&start);
    actions.append(&stop);
    let preview_group = adw::PreferencesGroup::builder().title("Live preview")
        .description("Real MangoApp over a 3D scene, scaled to fit. No 30 FPS limiter. Capture targets up to 60 FPS with bounded buffering; rendering follows the selected virtual refresh. Choose smaller images for smoother capture; actual speed depends on PNG readback and hardware.")
        .header_suffix(&actions).build();
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .build();
    let aspect = gtk::AspectFrame::builder()
        .ratio(16. / 9.)
        .obey_child(false)
        .height_request(300)
        .child(&picture)
        .build();
    preview_group.add(
        &gtk::Frame::builder()
            .child(&aspect)
            .css_classes(["card"])
            .build(),
    );
    let refresh = adw::SpinRow::with_range(30., 360., 1.);
    refresh.set_title("Preview virtual refresh (Hz)");
    refresh.set_subtitle("Applies when starting the preview; not a game FPS limit or benchmark");
    refresh.set_value(120.);
    preview_group.add(&refresh);
    let quality = adw::ComboRow::builder()
        .title("Preview image quality")
        .subtitle("Applies when starting; smaller captures reduce readback and encoding overhead")
        .model(&gtk::StringList::new(&[
            "Smooth · 480 × 270",
            "Balanced · 640 × 360",
            "Detailed · 960 × 540",
        ]))
        .build();
    preview_group.add(&quality);
    let status = gtk::Label::builder()
        .label("Preview is stopped. It never launches or restarts Steam.")
        .wrap(true)
        .xalign(0.)
        .selectable(true)
        .build();
    preview_group.add(&status);
    content.append(&preview_group);

    let presets = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let fps = gtk::Button::with_label("FPS only");
    let sensors = gtk::Button::with_label("FPS + temperatures");
    let detailed = gtk::Button::with_label("Detailed");
    for button in [&fps, &sensors, &detailed] {
        presets.append(button);
    }
    let preset_group = adw::PreferencesGroup::builder().title("Replace with preset")
        .description("Replaces the entire draft, including custom options. Live application, when enabled, saves the replacement too.").build();
    preset_group.add(&presets);
    content.append(&preset_group);

    let editor = gtk::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let graphical = gtk::Box::new(gtk::Orientation::Vertical, 24);
    graphical.set_sensitive(false);
    let controls = Controls::build(&graphical, &editor.buffer());
    content.append(&graphical);
    let full = adw::PreferencesGroup::builder().title("Full native configuration")
        .description("All native options remain available, including version-specific additions, ordering, repeated custom text and commands. Graphical edits preserve unrelated lines. Unsupported options are handled by the installed MangoHUD version. exec executes commands as your user.").build();
    full.set_header_suffix(Some(&gtk::LinkButton::with_label(
        "https://github.com/flightlessmango/MangoHud/blob/master/data/MangoHud.conf",
        "Configuration reference",
    )));
    let advanced = adw::ExpanderRow::builder()
        .title("Full configuration")
        .subtitle("Lossless native MangoHUD syntax · graphical controls edit the same draft")
        .build();
    advanced.add_row(
        &gtk::ScrolledWindow::builder()
            .min_content_height(260)
            .child(&editor)
            .build(),
    );
    full.add(&advanced);
    content.append(&full);
    content.append(&hd_updates::overlay_packages(ctx));

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
    dialog.connect_closed({
        let page = page.clone();
        move |_| page.stop_preview()
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
    ] { button.connect_clicked({ let weak = Rc::downgrade(&page); move |_| { if let Some(page) = weak.upgrade() && page.ready.get() { page.editor.set_text(&config); } } }); }
    glib::spawn_future_local({
        let weak = Rc::downgrade(&page);
        async move {
            let loaded = rt::blocking(Settings::load).await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            match loaded {
            Ok(settings) => {
                page.enabled.set_active(settings.enabled); page.editor.set_text(&settings.config);
                page.ready.set(true); graphical.set_sensitive(true); page.save.set_sensitive(true); page.start.set_sensitive(true);
            }
            Err(error) => page.applied.set_label(&format!("Could not load overlay.toml: {error:#}. The original file is preserved; repair it and reopen this view.")),
        }
        }
    });
    dialog.present(Some(&ctx.window));
    dialog
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
                        page.applied.set_label(if page.revision.get() == revision { "Saved. Already-enabled Hyprdeck MangoApp sessions reload appearance automatically; no application is restarted. The enable switch is only committed by Save overlay." } else { "Saved an earlier revision; the current draft has newer changes." });
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
                                page.status.set_label("Live · actual MangoApp renderer · draft edits hot-reload · close or hide this view to stop");
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
