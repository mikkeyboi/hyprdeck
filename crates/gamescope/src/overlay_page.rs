//! Gaming subcategory: native controls plus the complete MangoHUD configuration.
use crate::{
    overlay::{self, Settings},
    overlay_preview::{self, Message, Preview},
};
use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::{
    rt, store,
    ui::{self, Ctx},
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

struct Page {
    editor: gtk::TextBuffer,
    enabled: adw::SwitchRow,
    picture: gtk::Picture,
    status: gtk::Label,
    save: gtk::Button,
    start: gtk::Button,
    stop: gtk::Button,
    position: adw::ComboRow,
    font: adw::SpinRow,
    alpha: adw::SpinRow,
    metrics: Vec<(&'static str, bool, adw::SwitchRow)>,
    syncing: Cell<bool>,
    ready: Cell<bool>,
    revision: Cell<u64>,
    generation: Cell<u64>,
    preview: RefCell<Option<Preview>>,
}
const POSITIONS: [&str; 7] = [
    "top-left",
    "top-center",
    "top-right",
    "middle-left",
    "middle-right",
    "bottom-left",
    "bottom-right",
];

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
        .description("Independent of Steam’s FPS counter. Enable/disable takes effect on the next launch; MangoApp can reload saved appearance changes in a running session.").build();
    let enabled = adw::SwitchRow::builder()
        .title("Enable performance overlay")
        .subtitle("Use MangoApp when launching a new Big Picture session")
        .build();
    group.add(&enabled);
    content.append(&group);

    let start = gtk::Button::with_label("Start 3D preview");
    let stop = gtk::Button::with_label("Stop");
    stop.set_sensitive(false);
    start.set_valign(gtk::Align::Center);
    stop.set_valign(gtk::Align::Center);
    let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    actions.append(&start);
    actions.append(&stop);
    let preview_group = adw::PreferencesGroup::builder().title("Live preview")
        .description("Actual MangoApp over a 960 × 540 3D scene. Edits are previewed without saving. Demo: 30 FPS; capture: 5 FPS. HDR and game statistics depend on the game.")
        .header_suffix(&actions).build();
    let picture = gtk::Picture::builder()
        .can_shrink(true)
        .content_fit(gtk::ContentFit::Contain)
        .build();
    let aspect = gtk::AspectFrame::builder()
        .ratio(16.0 / 9.0)
        .obey_child(false)
        .height_request(300)
        .child(&picture)
        .build();
    let frame = gtk::Frame::builder()
        .child(&aspect)
        .css_classes(["card"])
        .build();
    preview_group.add(&frame);
    let status = gtk::Label::builder()
        .label("Preview is stopped. It never launches or restarts Steam.")
        .wrap(true)
        .xalign(0.0)
        .selectable(true)
        .build();
    preview_group.add(&status);
    content.append(&preview_group);

    let appearance = adw::PreferencesGroup::builder()
        .title("Appearance and metrics")
        .build();
    let position = adw::ComboRow::builder()
        .title("Position")
        .model(&gtk::StringList::new(&[
            "Top left",
            "Top center",
            "Top right",
            "Middle left",
            "Middle right",
            "Bottom left",
            "Bottom right",
        ]))
        .build();
    appearance.add(&position);
    let font = adw::SpinRow::with_range(8.0, 96.0, 1.0);
    font.set_title("Font size (pixels)");
    appearance.add(&font);
    let alpha = adw::SpinRow::with_range(0.0, 1.0, 0.05);
    alpha.set_title("Background opacity");
    alpha.set_digits(2);
    appearance.add(&alpha);
    let expander = adw::ExpanderRow::builder()
        .title("Displayed metrics")
        .subtitle("FPS, frame timing, hardware and layout")
        .build();
    let mut metrics = Vec::new();
    for (key, title, default) in [
        ("fps", "Frames per second", true),
        ("frametime", "Frame time", true),
        ("frame_timing", "Frame-time graph", true),
        ("gpu_stats", "GPU load", true),
        ("cpu_stats", "CPU load", true),
        ("gpu_temp", "GPU temperature", false),
        ("cpu_temp", "CPU temperature", false),
        ("vram", "Video memory", false),
        ("ram", "System memory", false),
        ("horizontal", "Horizontal layout", false),
        ("mangoapp_steam", "Show above Steam UI", false),
    ] {
        let row = adw::SwitchRow::builder().title(title).build();
        expander.add_row(&row);
        metrics.push((key, default, row));
    }
    appearance.add(&expander);
    content.append(&appearance);

    let full = adw::PreferencesGroup::builder().title("All MangoHUD settings")
        .description("Edit every native option: colors, fonts, offsets, graphs, hotkeys and logging. Comments and custom options are preserved. MangoHUD’s exec options run commands as your user.").build();
    let reference = gtk::LinkButton::with_label(
        "https://github.com/flightlessmango/MangoHud/blob/master/data/MangoHud.conf",
        "Configuration reference",
    );
    full.set_header_suffix(Some(&reference));
    let advanced = adw::ExpanderRow::builder()
        .title("Full configuration")
        .subtitle("Native MangoHUD syntax · unknown options and comments are preserved")
        .build();
    let editor = gtk::TextView::builder()
        .monospace(true)
        .wrap_mode(gtk::WrapMode::None)
        .top_margin(12)
        .bottom_margin(12)
        .left_margin(12)
        .right_margin(12)
        .build();
    let scroll = gtk::ScrolledWindow::builder()
        .min_content_height(260)
        .child(&editor)
        .build();
    advanced.add_row(&scroll);
    full.add(&advanced);
    content.append(&full);
    let presets = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let fps = gtk::Button::with_label("FPS only");
    let balanced = gtk::Button::with_label("Balanced");
    let detailed = gtk::Button::with_label("Detailed");
    for button in [&fps, &balanced, &detailed] {
        presets.append(button);
    }
    let preset_group = adw::PreferencesGroup::builder()
        .title("Replace with preset")
        .description(
            "Replaces the current draft, including custom options. Save overlay to keep it.",
        )
        .build();
    preset_group.add(&presets);
    content.append(&preset_group);

    let page = Rc::new(Page {
        editor: editor.buffer(),
        enabled,
        picture,
        status,
        save,
        start,
        stop,
        position,
        font,
        alpha,
        metrics,
        syncing: Cell::new(false),
        ready: Cell::new(false),
        revision: Cell::new(0),
        generation: Cell::new(0),
        preview: RefCell::new(None),
    });
    // The dialog owns state; callbacks and async work retain weak references.
    dialog.connect_closed({
        let page = page.clone();
        move |_| {
            page.stop_preview();
        }
    });
    root.connect_unmap({
        let page = Rc::downgrade(&page);
        move |_| {
            if let Some(page) = page.upgrade() {
                page.stop_preview();
            }
        }
    });
    page.connect();
    for (button, config) in [
        (
            fps,
            "fps=1\ngpu_stats=0\ncpu_stats=0\nframetime=0\nframe_timing=0\nmangoapp_steam=1\nposition=top-left\nfont_size=24\n".to_owned(),
        ),
        (balanced, overlay::DEFAULT_CONFIG.to_owned()),
        (
            detailed,
            format!(
                "{}gpu_temp\ncpu_temp\nvram\nram\nrefresh_rate\nresolution\nhdr\nfsr\n",
                overlay::DEFAULT_CONFIG
            ),
        ),
    ] {
        button.connect_clicked({
            let page = Rc::downgrade(&page);
            move |_| {
                if let Some(page) = page.upgrade()
                    && page.ready.get()
                {
                    page.editor.set_text(&config);
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
                    page.ready.set(true);
                    page.editor.set_text(&settings.config);
                    page.save.set_sensitive(true);
                    page.status
                        .set_label("Preview is stopped. It never launches or restarts Steam.");
                }
                Err(error) => {
                    page.status.set_label(&format!("Could not load overlay.toml: {error:#}. The file is preserved; repair it and reopen this view."));
                }
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
    fn set_option(&self, key: &str, value: &str) {
        if !self.syncing.get() && self.ready.get() {
            self.editor
                .set_text(&overlay::set_option(&self.text(), key, value));
        }
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
        self.position.connect_selected_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                if let Some(page) = weak.upgrade()
                    && let Some(value) = POSITIONS.get(row.selected() as usize)
                {
                    page.set_option("position", value);
                }
            }
        });
        self.font.connect_value_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                if let Some(page) = weak.upgrade() {
                    page.set_option("font_size", &row.value().to_string());
                }
            }
        });
        self.alpha.connect_value_notify({
            let weak = Rc::downgrade(self);
            move |row| {
                if let Some(page) = weak.upgrade() {
                    page.set_option("background_alpha", &row.value().to_string());
                }
            }
        });
        for (key, _, row) in &self.metrics {
            let key = *key;
            row.connect_active_notify({
                let weak = Rc::downgrade(self);
                move |row| {
                    if let Some(page) = weak.upgrade() {
                        page.set_option(key, if row.is_active() { "1" } else { "0" });
                    }
                }
            });
        }
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
        self.save.connect_clicked({ let weak = Rc::downgrade(self); move |_| {
            let Some(page) = weak.upgrade() else { return; };
            if !page.ready.get() { return; }
            let settings = Settings { enabled: page.enabled.is_active(), config: page.text() };
            page.save.set_sensitive(false);
            glib::spawn_future_local({ let weak = weak.clone(); async move {
                let result = rt::blocking(move || settings.save()).await;
                if let Some(page) = weak.upgrade() {
                    page.save.set_sensitive(true);
                    page.status.set_label(&match result { Ok(()) => "Overlay saved. A new gaming session uses this profile; an existing session is not restarted.".into(), Err(error) => format!("Saving overlay failed: {error:#}") });
                }
            }});
        }});
    }
    fn changed(self: &Rc<Self>) {
        if self.syncing.get() {
            return;
        }
        let text = self.text();
        self.syncing.set(true);
        if let Some(index) = POSITIONS
            .iter()
            .position(|p| Some(*p) == overlay::option(&text, "position"))
        {
            self.position.set_selected(index as u32);
        }
        self.font.set_value(
            overlay::option(&text, "font_size")
                .and_then(|v| v.parse().ok())
                .unwrap_or(24.0),
        );
        self.alpha.set_value(
            overlay::option(&text, "background_alpha")
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.5),
        );
        for (key, default, row) in &self.metrics {
            row.set_active(overlay::option(&text, key).map_or(*default, |v| v != "0"));
        }
        self.syncing.set(false);
        let revision = self.revision.get().wrapping_add(1);
        self.revision.set(revision);
        if self.preview.borrow().is_some() {
            let weak = Rc::downgrade(self);
            glib::spawn_future_local(async move {
                glib::timeout_future(Duration::from_millis(400)).await;
                let Some(page) = weak.upgrade() else {
                    return;
                };
                if page.revision.get() != revision {
                    return;
                }
                let path = page
                    .preview
                    .borrow()
                    .as_ref()
                    .map(|p| p.config_path.clone());
                let Some(path) = path else {
                    return;
                };
                let text = page.text();
                let result = rt::blocking(move || {
                    overlay::validate(&text)?;
                    store::write_atomic(&path, text.as_bytes())
                })
                .await;
                if let Err(error) = result {
                    page.status
                        .set_label(&format!("Updating preview failed: {error:#}"));
                }
            });
        }
    }
    fn stop_preview(&self) {
        self.generation.set(self.generation.get().wrapping_add(1));
        self.preview.borrow_mut().take();
        self.start.set_sensitive(true);
        self.stop.set_sensitive(false);
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
        self.status
            .set_label("Starting Gamescope, the 3D scene and MangoApp…");
        let generation = self.generation.get();
        let config = self.text();
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let result = rt::blocking(move || overlay_preview::start(config)).await;
            let Some(page) = weak.upgrade() else {
                return;
            };
            if page.generation.get() != generation {
                return;
            }
            let (preview, rx) = match result {
                Ok(value) => value,
                Err(error) => {
                    page.start.set_sensitive(true);
                    page.stop.set_sensitive(false);
                    page.status
                        .set_label(&format!("Preview unavailable: {error:#}"));
                    return;
                }
            };
            *page.preview.borrow_mut() = Some(preview);
            page.changed();
            drop(page);
            while let Ok(message) = rx.recv().await {
                let Some(page) = weak.upgrade() else {
                    break;
                };
                if page.generation.get() != generation {
                    break;
                }
                match message {
                    Message::Frame(bytes) => {
                        match gtk::gdk::Texture::from_bytes(&glib::Bytes::from_owned(bytes)) {
                            Ok(texture) => {
                                page.picture.set_paintable(Some(&texture));
                                page.status.set_label("Live · actual MangoApp renderer · edits update after a short delay · close this view to stop");
                            }
                            Err(error) => {
                                page.stop_preview();
                                page.status
                                    .set_label(&format!("Decoding preview failed: {error}"));
                                break;
                            }
                        }
                    }
                    Message::Error(error) => {
                        page.stop_preview();
                        page.status.set_label(&format!("Preview failed: {error}"));
                        break;
                    }
                }
            }
        });
    }
}
use std::time::Duration;
