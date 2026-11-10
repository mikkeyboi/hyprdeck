//! Native keyboard visualization. The plugin supplies validated key rectangles in
//! key units; the host owns drawing, hit testing, selection and accessibility.
//! Labels and details are always plain text, never markup.
use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use adw::prelude::*;
use gtk::{cairo, gdk, glib};
use serde_json::{Map, Value, json};

use crate::backend::{self, InputStream};
use crate::controller::{GREEN, ensure_css, rounded_rect, set_text, text};
use crate::keyboard_fx::{self, Press, Spot};
use crate::protocol::{
    Control, KeyAnimation, KeyAnimationKind, KeyPaint, KeyRemap, KeyboardFrame, KeyboardKey,
    KeyboardLayout, LiveStream, OptionItem, Visualization,
};
use crate::stream_retry::StreamRetry;

const ACCENT: (f64, f64, f64) = (0.47, 0.72, 1.0);
/// Pixels kept free around the drawing, matching the controller stage.
const PAD_X: f64 = 16.0;
const PAD_Y: f64 = 12.0;
/// Chassis margin around the key field, in key units.
const CHASSIS: f64 = 0.3;
/// Visual gap between neighbouring key caps, in key units.
const GAP: f64 = 0.06;
/// Pointer travel, in pixels, that turns a press into a rubber-band drag.
const DRAG_THRESHOLD: f64 = 4.0;
const SUMMARY_LABELS: usize = 8;
const MIN_HEIGHT: i32 = 140;
const MAX_HEIGHT: i32 = 560;
/// Effect previews redraw at most this often (microseconds): smooth enough, light on the CPU.
const FRAME_INTERVAL: i64 = 33_000;
/// The frame shown when the desktop asks for no animations.
const STILL_FRAME: f64 = 1.3;
const LEGEND_UNKNOWN: &str = "▨ Colour unknown";
const LEGEND_PREVIEW: &str = "◐ Effect preview";

type ControlCallback = dyn Fn(&adw::ActionRow, Control);
/// Submits `(action, args)` through the host's native action path for one row.
pub(crate) type ActionSubmit = Rc<dyn Fn(String, Map<String, Value>)>;
/// Binds a row owned by the view to action feedback and returns its submitter.
pub(crate) type ActionBinder = dyn Fn(&adw::ActionRow) -> ActionSubmit;

pub(crate) struct KeyboardView {
    pub widget: gtk::Widget,
    inner: Rc<Inner>,
}

struct PaintRow {
    row: adw::ActionRow,
    picker: gtk::ColorDialogButton,
    apply: gtk::Button,
}

struct RemapRow {
    row: adw::ActionRow,
    choice: gtk::DropDown,
    apply: gtk::Button,
    options: RefCell<Vec<OptionItem>>,
    /// The key whose assignment the dropdown currently shows.
    subject: RefCell<Option<String>>,
}

struct Inner {
    plugin_id: String,
    stream: RefCell<Option<LiveStream>>,
    stream_lease: RefCell<Option<InputStream>>,
    stream_generation: Cell<u64>,
    stream_retry: StreamRetry,
    mapped: Cell<bool>,
    paused: Cell<bool>,
    name: gtk::Label,
    connection: gtk::Label,
    status: gtk::Label,
    area: gtk::DrawingArea,
    selector: gtk::DropDown,
    additive: gtk::ToggleButton,
    select_all: gtk::Button,
    clear: gtk::Button,
    summary: gtk::Label,
    telemetry: gtk::Label,
    inspector_title: gtk::Label,
    readings: gtk::Label,
    detail: gtk::Label,
    actions: adw::PreferencesGroup,
    paint_row: PaintRow,
    remap_row: RemapRow,
    control_row: RefCell<Option<adw::ActionRow>>,
    shown_control: RefCell<Option<(String, Control)>>,
    layout: Cell<KeyboardLayout>,
    keys: RefCell<Vec<KeyboardKey>>,
    pressed: RefCell<Vec<Option<bool>>>,
    selection: RefCell<Selection>,
    paint: RefCell<Option<KeyPaint>>,
    remap: RefCell<Option<KeyRemap>>,
    hovered: Cell<Option<usize>>,
    drag_start: Cell<Option<(f64, f64)>>,
    drag_modifiers: Cell<gdk::ModifierType>,
    band: Cell<Option<(f64, f64, f64, f64)>>,
    /// Set while the view changes its own widgets, so notify handlers ignore it.
    syncing: Cell<bool>,
    on_control: Box<ControlCallback>,
    legend_color: gtk::Label,
    animation: RefCell<Option<Box<KeyAnimation>>>,
    /// Monotonic time (µs) the view was created; effect previews count from here.
    epoch: i64,
    /// Recent presses for typing effects: key centre and monotonic press time (µs).
    presses: RefCell<Vec<(f64, f64, i64)>>,
    tick: RefCell<Option<gtk::TickCallbackId>>,
    /// Set once in `connect`, so `&self` methods can hand the frame clock a weak handle.
    this: RefCell<std::rc::Weak<Inner>>,
    last_frame: Cell<i64>,
}

impl KeyboardView {
    pub(crate) fn new(
        plugin_id: &str,
        visualization: &Visualization,
        on_control: impl Fn(&adw::ActionRow, Control) + 'static,
        bind_action: impl Fn(&adw::ActionRow) -> ActionSubmit + 'static,
    ) -> Self {
        ensure_css();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.add_css_class("card");
        root.add_css_class("hd-controller");
        root.add_css_class("hd-keyboard");
        root.append(&text("KEYBOARD", "hd-controller-eyebrow"));
        let name = text("", "title-2");
        root.append(&name);
        let connection = text("", "dim-label");
        root.append(&connection);

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let legend = gtk::Box::new(gtk::Orientation::Horizontal, 14);
        legend.set_valign(gtk::Align::Center);
        let pressed = text("● Pressed", "hd-controller-legend");
        pressed.add_css_class("hd-controller-active");
        legend.append(&pressed);
        legend.append(&text("▢ Selected", "hd-controller-legend"));
        let legend_color = text(LEGEND_UNKNOWN, "hd-controller-legend");
        legend.append(&legend_color);
        toolbar.append(&legend);
        let status = text("", "hd-controller-status");
        status.set_hexpand(true);
        status.set_xalign(1.0);
        toolbar.append(&status);
        root.append(&toolbar);

        let area = gtk::DrawingArea::builder()
            .content_height(260)
            .hexpand(true)
            .focusable(true)
            .build();
        area.add_css_class("hd-controller-stage");
        area.update_property(&[gtk::accessible::Property::Label(
            "Keyboard layout. Arrow keys move between keys, Space toggles the focused key, \
             Shift+Arrow extends the selection, Ctrl+A selects all and Escape clears. \
             The key selector below lists every key.",
        )]);
        root.append(&area);
        let telemetry = text("", "hd-controller-telemetry");
        telemetry.set_selectable(true);
        root.append(&telemetry);

        let bar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let selector_label = text("_Key:", "dim-label");
        selector_label.set_use_underline(true);
        selector_label.set_valign(gtk::Align::Center);
        let selector = searchable_dropdown(&[]);
        selector.add_css_class("hd-controller-selector");
        selector.set_hexpand(true);
        selector.update_property(&[gtk::accessible::Property::Label("Select keyboard key")]);
        selector_label.set_mnemonic_widget(Some(&selector));
        let additive = gtk::ToggleButton::with_label("Add to selection");
        additive.set_tooltip_text(Some(
            "Keys picked in the key selector are added to or removed from the selection, \
             like Ctrl+click",
        ));
        let select_all = gtk::Button::with_label("Select all");
        let clear = gtk::Button::with_label("Clear selection");
        for widget in [
            selector_label.upcast_ref::<gtk::Widget>(),
            selector.upcast_ref(),
            additive.upcast_ref(),
            select_all.upcast_ref(),
            clear.upcast_ref(),
        ] {
            widget.set_valign(gtk::Align::Center);
            bar.append(widget);
        }
        root.append(&bar);
        let summary = text("", "hd-controller-hint");
        summary.set_selectable(true);
        root.append(&summary);

        let inspector = gtk::Box::new(gtk::Orientation::Vertical, 6);
        inspector.add_css_class("hd-controller-inspector");
        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let inspector_title = text("", "heading");
        inspector_title.set_hexpand(true);
        let readings = text("", "hd-controller-readings");
        readings.set_selectable(true);
        title_row.append(&inspector_title);
        title_row.append(&readings);
        let detail = text("", "dim-label");
        detail.set_selectable(true);
        inspector.append(&title_row);
        inspector.append(&detail);
        let actions = adw::PreferencesGroup::new();
        actions.set_visible(false);
        inspector.append(&actions);
        root.append(&inspector);

        // Both rows exist for the view's lifetime so that their action feedback
        // and the picker's pending colour survive every refresh.
        let paint_row = PaintRow::new(&actions);
        let remap_row = RemapRow::new(&actions);
        let paint_submit = bind_action(&paint_row.row);
        let remap_submit = bind_action(&remap_row.row);

        let inner = Rc::new(Inner {
            plugin_id: plugin_id.to_owned(),
            stream: RefCell::new(None),
            stream_lease: RefCell::new(None),
            stream_generation: Cell::new(0),
            stream_retry: StreamRetry::default(),
            mapped: Cell::new(false),
            paused: Cell::new(false),
            name,
            connection,
            status,
            area,
            selector,
            additive,
            select_all,
            clear,
            summary,
            telemetry,
            inspector_title,
            readings,
            detail,
            actions,
            paint_row,
            remap_row,
            control_row: RefCell::new(None),
            shown_control: RefCell::new(None),
            layout: Cell::new(KeyboardLayout {
                width: 1.0,
                height: 1.0,
            }),
            keys: RefCell::new(Vec::new()),
            pressed: RefCell::new(Vec::new()),
            selection: RefCell::new(Selection::default()),
            paint: RefCell::new(None),
            remap: RefCell::new(None),
            hovered: Cell::new(None),
            drag_start: Cell::new(None),
            drag_modifiers: Cell::new(gdk::ModifierType::empty()),
            band: Cell::new(None),
            syncing: Cell::new(false),
            on_control: Box::new(on_control),
            legend_color,
            animation: RefCell::new(None),
            epoch: glib::monotonic_time(),
            presses: RefCell::new(Vec::new()),
            tick: RefCell::new(None),
            this: RefCell::new(std::rc::Weak::new()),
            last_frame: Cell::new(0),
        });
        inner.connect(&root, paint_submit, remap_submit);
        let view = Self {
            widget: root.upcast(),
            inner,
        };
        view.update(visualization);
        view.inner.selection_changed();
        view
    }

    /// Merge a new snapshot by key id: selection, focus, the pending paint colour
    /// and an in-progress remap choice survive; changed metadata updates in place.
    pub(crate) fn update(&self, visualization: &Visualization) {
        let Visualization::Keyboard {
            name,
            connection,
            status,
            layout,
            keys,
            paint,
            remap,
            stream,
            animation,
        } = visualization
        else {
            return;
        };
        let inner = &self.inner;
        if *inner.animation.borrow() != *animation {
            inner.animation.borrow_mut().clone_from(animation);
            inner.presses.borrow_mut().clear();
            set_text(
                &inner.legend_color,
                if inner.previewing() {
                    LEGEND_PREVIEW
                } else {
                    LEGEND_UNKNOWN
                },
            );
            inner.area.queue_draw();
        }
        set_text(&inner.name, name);
        set_text(&inner.connection, connection);
        if *inner.stream.borrow() != *stream {
            inner.stop_stream();
            inner.stream.borrow_mut().clone_from(stream);
            inner.pressed.borrow_mut().fill(None);
            inner.stream_retry.reset();
            inner.area.queue_draw();
            inner.refresh_telemetry();
        }
        if inner.paused.replace(false) {
            inner.stream_retry.reset();
        }
        let live = stream.is_some();
        if !live {
            set_text(&inner.status, status);
        }
        let mut changed = inner.set_keys(*layout, keys, !live);
        if *inner.paint.borrow() != *paint {
            inner.paint.borrow_mut().clone_from(paint);
            changed = true;
        }
        if *inner.remap.borrow() != *remap {
            inner.remap.borrow_mut().clone_from(remap);
            changed = true;
        }
        if changed {
            inner.selection_changed();
        }
        inner.start_stream();
        inner.sync_tick();
    }

    pub(crate) fn show_header(&self, visible: bool) {
        if self.inner.name.is_visible() == visible {
            return;
        }
        self.inner.name.set_visible(visible);
        self.inner.connection.set_visible(visible);
        let mut child = self.widget.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            if widget.has_css_class("hd-controller-eyebrow") {
                widget.set_visible(visible);
            }
        }
    }

    pub(crate) fn disconnect(&self) {
        self.inner.paused.set(true);
        self.inner.stop_stream();
        self.inner
            .status
            .set_text("Live key presses paused · Backend unavailable");
    }
}

impl PaintRow {
    fn new(actions: &adw::PreferencesGroup) -> Self {
        let row = adw::ActionRow::builder().use_markup(false).build();
        let picker = crate::page::color_picker("#FFFFFF");
        picker.update_property(&[gtk::accessible::Property::Label("Colour for selected keys")]);
        let apply = gtk::Button::builder()
            .label("Apply")
            .valign(gtk::Align::Center)
            .build();
        apply.add_css_class("suggested-action");
        row.add_suffix(&picker);
        row.add_suffix(&apply);
        row.set_visible(false);
        actions.add(&row);
        Self { row, picker, apply }
    }
}

impl RemapRow {
    fn new(actions: &adw::PreferencesGroup) -> Self {
        let row = adw::ActionRow::builder().use_markup(false).build();
        let choice = searchable_dropdown(&[]);
        choice.set_valign(gtk::Align::Center);
        let apply = gtk::Button::builder()
            .label("Apply")
            .valign(gtk::Align::Center)
            .build();
        row.add_suffix(&choice);
        row.add_suffix(&apply);
        row.set_visible(false);
        actions.add(&row);
        Self {
            row,
            choice,
            apply,
            options: RefCell::new(Vec::new()),
            subject: RefCell::new(None),
        }
    }
}

impl Inner {
    fn connect(self: &Rc<Self>, root: &gtk::Box, paint: ActionSubmit, remap: ActionSubmit) {
        *self.this.borrow_mut() = Rc::downgrade(self);
        let weak = Rc::downgrade(self);
        self.area.set_draw_func(move |_, cr, width, height| {
            if let Some(inner) = weak.upgrade() {
                inner.draw(cr, f64::from(width), f64::from(height));
            }
        });
        let weak = Rc::downgrade(self);
        self.area.connect_resize(move |_, width, _| {
            if let Some(inner) = weak.upgrade() {
                inner.fit_height(width);
            }
        });
        let weak = Rc::downgrade(self);
        self.selector.connect_selected_notify(move |selector| {
            if let Some(inner) = weak.upgrade()
                && !inner.syncing.get()
            {
                inner.pick(selector.selected() as usize);
            }
        });
        let weak = Rc::downgrade(self);
        self.select_all.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.selection.borrow_mut().all();
                inner.selection_changed();
            }
        });
        let weak = Rc::downgrade(self);
        self.clear.connect_clicked(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.selection.borrow_mut().clear();
                inner.selection_changed();
            }
        });
        let weak = Rc::downgrade(self);
        self.paint_row.apply.connect_clicked(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            let Some(spec) = inner.paint.borrow().clone() else {
                return;
            };
            let ids = inner.selection.borrow().ids(&inner.keys.borrow());
            if ids.is_empty() {
                return;
            }
            let color = crate::page::color_hex(&inner.paint_row.picker.rgba());
            paint(spec.action, with_value(spec.args, paint_value(ids, &color)));
        });
        let weak = Rc::downgrade(self);
        self.remap_row.choice.connect_selected_notify(move |_| {
            if let Some(inner) = weak.upgrade()
                && !inner.syncing.get()
            {
                inner.remap_row.row.add_css_class("hd-pending-edit");
            }
        });
        let weak = Rc::downgrade(self);
        self.remap_row.apply.connect_clicked(move |_| {
            let Some(inner) = weak.upgrade() else { return };
            let Some(spec) = inner.remap.borrow().clone() else {
                return;
            };
            let Some(index) = inner.selection.borrow().single() else {
                return;
            };
            let keys = inner.keys.borrow();
            let Some(key) = keys.get(index).filter(|key| key.can_remap()) else {
                return;
            };
            let key = key.id.clone();
            drop(keys);
            let options = inner.remap_row.options.borrow();
            let Some(option) = options.get(inner.remap_row.choice.selected() as usize) else {
                return;
            };
            let value = json!({"key": key, "target": option.value});
            drop(options);
            remap(spec.action, with_value(spec.args, value));
        });

        let drag = gtk::GestureDrag::new();
        drag.set_button(1);
        let weak = Rc::downgrade(self);
        drag.connect_drag_begin(move |gesture, x, y| {
            if let Some(inner) = weak.upgrade() {
                inner.area.grab_focus();
                inner.drag_start.set(Some((x, y)));
                inner.drag_modifiers.set(gesture.current_event_state());
                inner.band.set(None);
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_update(move |_, dx, dy| {
            if let Some(inner) = weak.upgrade()
                && let Some((x, y)) = inner.drag_start.get()
                && (inner.band.get().is_some() || dx.hypot(dy) >= DRAG_THRESHOLD)
            {
                inner.band.set(Some((x, y, x + dx, y + dy)));
                inner.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_drag_end(move |_, _, _| {
            if let Some(inner) = weak.upgrade() {
                inner.finish_drag();
            }
        });
        let weak = Rc::downgrade(self);
        drag.connect_cancel(move |_, _| {
            if let Some(inner) = weak.upgrade() {
                inner.drag_start.set(None);
                if inner.band.take().is_some() {
                    inner.area.queue_draw();
                }
            }
        });
        self.area.add_controller(drag);

        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(self);
        motion.connect_motion(move |_, x, y| {
            if let Some(inner) = weak.upgrade() {
                let hit = inner.hit(x, y);
                if inner.hovered.replace(hit) != hit {
                    inner.area.set_cursor_from_name(hit.map(|_| "pointer"));
                    let keys = inner.keys.borrow();
                    let tooltip = hit.and_then(|index| keys.get(index)).map(|key| {
                        if key.detail.is_empty() {
                            key.label.clone()
                        } else {
                            format!("{} · {}", key.label, key.detail)
                        }
                    });
                    inner.area.set_tooltip_text(tooltip.as_deref());
                }
            }
        });
        let weak = Rc::downgrade(self);
        motion.connect_leave(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.hovered.set(None);
                inner.area.set_cursor_from_name(None);
                inner.area.set_tooltip_text(None);
            }
        });
        self.area.add_controller(motion);

        let keys = gtk::EventControllerKey::new();
        let weak = Rc::downgrade(self);
        keys.connect_key_pressed(move |_, key, _, modifiers| {
            weak.upgrade().map_or(glib::Propagation::Proceed, |inner| {
                inner.key(key, modifiers)
            })
        });
        self.area.add_controller(keys);
        let focus = gtk::EventControllerFocus::new();
        let weak = Rc::downgrade(self);
        focus.connect_enter(move |_| {
            if let Some(inner) = weak.upgrade() {
                let has_keys = !inner.keys.borrow().is_empty();
                let mut selection = inner.selection.borrow_mut();
                if selection.focus.is_none() && has_keys {
                    selection.focus = Some(selection.first().unwrap_or(0));
                }
                drop(selection);
                inner.area.queue_draw();
            }
        });
        let weak = Rc::downgrade(self);
        focus.connect_leave(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.area.queue_draw();
            }
        });
        self.area.add_controller(focus);

        let weak = Rc::downgrade(self);
        root.connect_map(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.mapped.set(true);
                inner.stream_retry.reset();
                inner.start_stream();
                inner.sync_tick();
            }
        });
        let weak = Rc::downgrade(self);
        root.connect_unmap(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.mapped.set(false);
                inner.stop_stream();
                inner.sync_tick();
            }
        });
        let weak = Rc::downgrade(self);
        root.connect_destroy(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.mapped.set(false);
                inner.stop_stream();
                inner.sync_tick();
            }
        });
    }

    /// An effect preview is configured and this host knows how to draw it.
    fn previewing(&self) -> bool {
        self.animation
            .borrow()
            .as_ref()
            .is_some_and(|animation| animation.kind != KeyAnimationKind::Unknown)
    }

    fn animations_enabled(&self) -> bool {
        self.area.settings().is_gtk_enable_animations()
    }

    /// Forget presses too old to light anything; returns how many remain.
    fn prune_presses(&self, now: i64) -> usize {
        let mut presses = self.presses.borrow_mut();
        presses.retain(|(_, _, at)| (now - at) as f64 / 1e6 < keyboard_fx::PRESS_LIFE);
        presses.len()
    }

    /// Redraw on the frame clock only while a mapped preview is actually moving.
    fn should_tick(&self) -> bool {
        let presses = self.prune_presses(glib::monotonic_time());
        self.mapped.get()
            && self.animations_enabled()
            && self
                .animation
                .borrow()
                .as_ref()
                .is_some_and(|animation| keyboard_fx::moving(animation, presses))
    }

    fn sync_tick(&self) {
        let wanted = self.should_tick();
        if !wanted {
            if let Some(tick) = self.tick.borrow_mut().take() {
                tick.remove();
                self.area.queue_draw();
            }
            return;
        }
        if self.tick.borrow().is_some() {
            return;
        }
        let weak = self.this.borrow().clone();
        let tick = self.area.add_tick_callback(move |area, clock| {
            let Some(inner) = weak.upgrade() else {
                return glib::ControlFlow::Break;
            };
            if !inner.should_tick() {
                inner.tick.borrow_mut().take();
                area.queue_draw();
                return glib::ControlFlow::Break;
            }
            let now = clock.frame_time();
            if now - inner.last_frame.get() >= FRAME_INTERVAL {
                inner.last_frame.set(now);
                area.queue_draw();
            }
            glib::ControlFlow::Continue
        });
        *self.tick.borrow_mut() = Some(tick);
    }

    fn stop_stream(&self) {
        self.stream_retry.cancel();
        self.stream_generation
            .set(self.stream_generation.get().wrapping_add(1));
        self.stream_lease.borrow_mut().take();
        self.set_pressed(None);
    }

    fn start_stream(self: &Rc<Self>) {
        if !self.mapped.get() || self.paused.get() || self.stream_lease.borrow().is_some() {
            return;
        }
        let Some(stream) = self.stream.borrow().clone() else {
            return;
        };
        let weak = Rc::downgrade(self);
        if !self.stream_retry.ready(move || {
            if let Some(inner) = weak.upgrade() {
                inner.start_stream();
            }
        }) {
            return;
        }
        let generation = self.stream_generation.get();
        let (lease, mut receiver) =
            backend::input_stream::<KeyboardFrame>(self.plugin_id.clone(), stream);
        *self.stream_lease.borrow_mut() = Some(lease);
        self.status.set_text("Starting live key presses…");
        self.status.remove_css_class("error");
        let weak = Rc::downgrade(self);
        // Frames wake the GTK future directly; there is no polling and no
        // device or file I/O on the main thread.
        glib::spawn_future_local(async move {
            while let Some(event) = receiver.recv().await {
                let Some(inner) = weak.upgrade() else { break };
                if inner.stream_generation.get() != generation {
                    break;
                }
                match event {
                    Ok(frame) => {
                        inner.stream_retry.reset();
                        set_text(&inner.status, &frame.status);
                        inner.set_pressed(frame.pressed.as_deref());
                    }
                    Err(error) => {
                        inner.stream_disconnected(&error);
                        return;
                    }
                }
            }
            if let Some(inner) = weak.upgrade()
                && inner.stream_generation.get() == generation
            {
                inner.stream_disconnected("Live input disconnected: stream closed");
            }
        });
    }

    fn stream_disconnected(&self, error: &str) {
        self.status.set_text(error);
        self.status.add_css_class("error");
        self.stream_retry.failed();
        self.set_pressed(None);
        self.stream_lease.borrow_mut().take();
    }

    /// Returns whether keys changed in a way that affects selection-dependent UI.
    fn set_keys(&self, layout: KeyboardLayout, keys: &[KeyboardKey], samples: bool) -> bool {
        let mut redraw = false;
        let mut relabel = false;
        {
            let mut current = self.keys.borrow_mut();
            if *current != keys {
                let same_ids = current.len() == keys.len()
                    && current.iter().zip(keys).all(|(a, b)| a.id == b.id);
                if same_ids {
                    relabel = current.iter().zip(keys).any(|(a, b)| a.label != b.label);
                } else {
                    self.selection.borrow_mut().remap(&current, keys);
                    let mut pressed = self.pressed.borrow_mut();
                    let old: HashMap<&str, Option<bool>> = current
                        .iter()
                        .map(|key| key.id.as_str())
                        .zip(pressed.iter().copied())
                        .collect();
                    *pressed = keys
                        .iter()
                        .map(|key| old.get(key.id.as_str()).copied().flatten())
                        .collect();
                    self.hovered.set(None);
                    relabel = true;
                }
                *current = keys.to_vec();
                redraw = true;
            }
        }
        if samples {
            let mut pressed = self.pressed.borrow_mut();
            if pressed.len() != keys.len()
                || pressed
                    .iter()
                    .zip(keys)
                    .any(|(old, key)| *old != key.pressed)
            {
                *pressed = keys.iter().map(|key| key.pressed).collect();
                redraw = true;
            }
        }
        if self.layout.replace(layout) != layout {
            self.fit_height(self.area.width());
            redraw = true;
        }
        if relabel {
            self.rebuild_selector();
        }
        if redraw {
            self.area.queue_draw();
            self.refresh_telemetry();
        }
        redraw
    }

    fn set_pressed(&self, pressed: Option<&[String]>) {
        let next: Vec<Option<bool>> = {
            let keys = self.keys.borrow();
            match pressed {
                Some(ids) => {
                    // Unknown ids are ignored; unlisted keys are released.
                    let ids: HashSet<&str> = ids.iter().map(String::as_str).collect();
                    keys.iter()
                        .map(|key| Some(ids.contains(key.id.as_str())))
                        .collect()
                }
                None => vec![None; keys.len()],
            }
        };
        if *self.pressed.borrow() == next {
            return;
        }
        if self
            .animation
            .borrow()
            .as_ref()
            .is_some_and(|animation| keyboard_fx::reactive(animation.kind))
        {
            let now = glib::monotonic_time();
            let keys = self.keys.borrow();
            let old = self.pressed.borrow();
            let mut presses = self.presses.borrow_mut();
            for (index, key) in keys.iter().enumerate() {
                let was = old.get(index).copied().flatten() == Some(true);
                if next.get(index).copied().flatten() == Some(true) && !was {
                    let (x, y) = center(key);
                    presses.push((x, y, now));
                }
            }
            // Bounded even if a stream reports presses faster than they fade.
            let excess = presses.len().saturating_sub(64);
            presses.drain(..excess);
        }
        *self.pressed.borrow_mut() = next;
        self.area.queue_draw();
        self.sync_tick();
        self.refresh_telemetry();
        if self.selection.borrow().single().is_some() {
            self.refresh_inspector();
        }
    }

    fn rebuild_selector(&self) {
        let names = display_names(&self.keys.borrow());
        let names: Vec<&str> = names.iter().map(String::as_str).collect();
        self.syncing.set(true);
        self.selector.set_model(Some(&gtk::StringList::new(&names)));
        self.syncing.set(false);
    }

    fn fit_height(&self, width: i32) {
        if width <= 0 {
            return;
        }
        let want = desired_height(f64::from(width), self.layout.get());
        if (self.area.content_height() - want).abs() > 3 {
            // Changing the size request from inside an allocation is deferred.
            let area = self.area.downgrade();
            glib::idle_add_local_once(move || {
                if let Some(area) = area.upgrade() {
                    area.set_content_height(want);
                }
            });
        }
    }

    fn hit(&self, x: f64, y: f64) -> Option<usize> {
        let (x, y) = self.to_layout(x, y)?;
        hit(&self.keys.borrow(), x, y)
    }

    fn to_layout(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (scale, ox, oy) = fit(
            f64::from(self.area.width()),
            f64::from(self.area.height()),
            self.layout.get(),
        );
        (scale > 0.0).then(|| ((x - ox) / scale, (y - oy) / scale))
    }

    fn finish_drag(&self) {
        let Some((x, y)) = self.drag_start.take() else {
            return;
        };
        let modifiers = self.drag_modifiers.get();
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
        if let Some((x0, y0, x1, y1)) = self.band.take() {
            if let (Some((ax, ay)), Some((bx, by))) =
                (self.to_layout(x0, y0), self.to_layout(x1, y1))
            {
                let indices = intersecting(&self.keys.borrow(), (ax, ay, bx, by));
                self.selection.borrow_mut().replace(&indices, ctrl || shift);
            }
            self.selection_changed();
            return;
        }
        let hit = self.hit(x, y);
        {
            let mut selection = self.selection.borrow_mut();
            match hit {
                Some(index) if shift => selection.extend(&self.keys.borrow(), index, ctrl),
                Some(index) if ctrl => selection.toggle(index),
                Some(index) => selection.only(index),
                None if !ctrl && !shift => selection.clear(),
                None => return,
            }
        }
        self.selection_changed();
    }

    fn pick(&self, index: usize) {
        if index >= self.keys.borrow().len() {
            return;
        }
        if self.additive.is_active() {
            self.selection.borrow_mut().toggle(index);
        } else {
            self.selection.borrow_mut().only(index);
        }
        self.selection_changed();
    }

    fn key(&self, key: gdk::Key, modifiers: gdk::ModifierType) -> glib::Propagation {
        let ctrl = modifiers.contains(gdk::ModifierType::CONTROL_MASK);
        let shift = modifiers.contains(gdk::ModifierType::SHIFT_MASK);
        let len = self.keys.borrow().len();
        if len == 0 {
            return glib::Propagation::Proceed;
        }
        let direction = match key {
            gdk::Key::Left | gdk::Key::KP_Left => Some((-1.0, 0.0)),
            gdk::Key::Right | gdk::Key::KP_Right => Some((1.0, 0.0)),
            gdk::Key::Up | gdk::Key::KP_Up => Some((0.0, -1.0)),
            gdk::Key::Down | gdk::Key::KP_Down => Some((0.0, 1.0)),
            _ => None,
        };
        let mut selection = self.selection.borrow_mut();
        let focus = selection.focus.or_else(|| selection.first()).unwrap_or(0);
        if let Some(direction) = direction {
            let target = neighbor(&self.keys.borrow(), focus, direction).unwrap_or(focus);
            if shift {
                if selection.anchor.is_none() {
                    selection.anchor = Some(focus);
                }
                selection.extend(&self.keys.borrow(), target, ctrl);
            } else {
                selection.focus = Some(target);
            }
        } else {
            match key {
                gdk::Key::space | gdk::Key::KP_Space => selection.toggle(focus),
                gdk::Key::Return | gdk::Key::KP_Enter | gdk::Key::ISO_Enter => {
                    selection.only(focus);
                    drop(selection);
                    self.selection_changed();
                    self.actions.child_focus(gtk::DirectionType::TabForward);
                    return glib::Propagation::Stop;
                }
                gdk::Key::a | gdk::Key::A if ctrl => selection.all(),
                // An empty selection lets Escape reach the page (back to products).
                gdk::Key::Escape if selection.count() > 0 => selection.clear(),
                _ => return glib::Propagation::Proceed,
            }
        }
        drop(selection);
        self.selection_changed();
        glib::Propagation::Stop
    }

    /// Single place that reflects selection and focus into every dependent widget.
    fn selection_changed(&self) {
        let (count, focus, text) = {
            let keys = self.keys.borrow();
            let selection = self.selection.borrow();
            (
                selection.count(),
                selection.focus,
                summary(&keys, &selection),
            )
        };
        let len = self.keys.borrow().len();
        set_text(&self.summary, &text);
        self.area
            .update_property(&[gtk::accessible::Property::Description(&text)]);
        self.clear.set_sensitive(count > 0);
        self.select_all.set_sensitive(count < len);
        if let Some(focus) = focus
            && self.selector.selected() != focus as u32
        {
            self.syncing.set(true);
            self.selector.set_selected(focus as u32);
            self.syncing.set(false);
        }
        self.refresh_inspector();
        self.area.queue_draw();
    }

    fn refresh_telemetry(&self) {
        let keys = self.keys.borrow();
        let pressed = self.pressed.borrow();
        let line = if pressed.iter().all(Option::is_none) {
            if self.stream.borrow().is_some() {
                "Live key presses unavailable".to_owned()
            } else {
                "Key presses are not reported by this backend".to_owned()
            }
        } else {
            let down: Vec<&str> = keys
                .iter()
                .zip(pressed.iter())
                .filter(|(_, pressed)| **pressed == Some(true))
                .map(|(key, _)| key.label.as_str())
                .collect();
            if down.is_empty() {
                "Pressed: none".to_owned()
            } else {
                format!("Pressed: {}", down.join(" + "))
            }
        };
        set_text(&self.telemetry, &line);
    }

    fn refresh_inspector(&self) {
        let keys = self.keys.borrow();
        let selection = self.selection.borrow();
        let count = selection.count();
        let single = selection.single();
        let remap = self.remap.borrow();
        let paint = self.paint.borrow();
        match single.and_then(|index| keys.get(index).map(|key| (index, key))) {
            Some((index, key)) => {
                set_text(&self.inspector_title, &key.label);
                set_text(
                    &self.readings,
                    &reading(
                        key,
                        self.pressed.borrow().get(index).copied().flatten(),
                        remap.as_ref(),
                    ),
                );
                set_text(
                    &self.detail,
                    if key.detail.is_empty() {
                        "Click another key, Ctrl+click to add keys, or drag to select a group."
                    } else {
                        &key.detail
                    },
                );
            }
            None if count > 1 => {
                set_text(&self.inspector_title, &format!("{count} keys selected"));
                set_text(&self.readings, "");
                set_text(
                    &self.detail,
                    "Paint applies to every selected key. Select a single key to inspect or remap it.",
                );
            }
            None => {
                set_text(&self.inspector_title, "No key selected");
                set_text(&self.readings, "");
                set_text(
                    &self.detail,
                    "Click a key, drag across keys, or use the key selector to choose keys.",
                );
            }
        }

        let paint_row = &self.paint_row;
        paint_row.row.set_visible(paint.is_some());
        if let Some(paint) = paint.as_ref() {
            set_title(&paint_row.row, &paint.label);
            set_subtitle(
                &paint_row.row,
                &match count {
                    0 => "Select keys to paint".to_owned(),
                    1 => "1 key selected".to_owned(),
                    count => format!("{count} keys selected"),
                },
            );
            paint_row.apply.set_sensitive(count > 0);
        }

        let row = &self.remap_row;
        row.row.set_visible(remap.is_some());
        if let Some(remap) = remap.as_ref() {
            set_title(&row.row, &remap.label);
            if *row.options.borrow() != remap.options {
                let labels: Vec<&str> = remap
                    .options
                    .iter()
                    .map(|option| option.label.as_str())
                    .collect();
                self.syncing.set(true);
                row.choice.set_model(Some(&gtk::StringList::new(&labels)));
                row.choice.set_enable_search(labels.len() > 16);
                self.syncing.set(false);
                *row.options.borrow_mut() = remap.options.clone();
                row.row.remove_css_class("hd-pending-edit");
            }
            let subject = single.map(|index| keys[index].id.clone());
            if *row.subject.borrow() != subject {
                // A pending choice belongs to the key it was made for.
                row.row.remove_css_class("hd-pending-edit");
                *row.subject.borrow_mut() = subject;
            }
            match single.map(|index| &keys[index]) {
                Some(key) if key.can_remap() => {
                    let current = key
                        .remap
                        .as_ref()
                        .and_then(|value| remap.options.iter().position(|o| &o.value == value));
                    set_subtitle(
                        &row.row,
                        &format!(
                            "{} · {}",
                            key.label,
                            current.map_or("current assignment unknown".to_owned(), |index| {
                                format!("currently {}", remap.options[index].label)
                            })
                        ),
                    );
                    if !row.row.has_css_class("hd-pending-edit")
                        && let Some(current) = current
                        && row.choice.selected() != current as u32
                    {
                        self.syncing.set(true);
                        row.choice.set_selected(current as u32);
                        self.syncing.set(false);
                    }
                    row.choice.set_sensitive(true);
                    row.apply.set_sensitive(true);
                }
                Some(key) => {
                    row.row.remove_css_class("hd-pending-edit");
                    set_subtitle(
                        &row.row,
                        &format!("{} · Shared remapping unavailable", key.label),
                    );
                    row.choice.set_sensitive(false);
                    row.apply.set_sensitive(false);
                }
                None => {
                    set_subtitle(&row.row, "Select a single key to remap");
                    row.choice.set_sensitive(false);
                    row.apply.set_sensitive(false);
                }
            }
        }

        let control = single
            .and_then(|index| keys.get(index))
            .and_then(|key| key.control.as_ref().map(|control| (key, control)));
        let shows_actions = paint.is_some() || remap.is_some() || control.is_some();
        let same_control = match (&*self.shown_control.borrow(), control) {
            (None, None) => true,
            (Some((id, old)), Some((key, new))) => *id == key.id && old == new,
            _ => false,
        };
        if same_control {
            if let (Some(row), Some((key, _))) = (self.control_row.borrow().as_ref(), control) {
                set_title(row, &key.label);
            }
            self.actions.set_visible(shows_actions);
            return;
        }
        // Refreshed capability values must not discard an in-progress edit.
        if let Some((key, new)) = control
            && self
                .shown_control
                .borrow()
                .as_ref()
                .is_some_and(|(id, old)| {
                    *id == key.id && crate::products::compatible_control(old, new)
                })
            && self.control_row.borrow().as_ref().is_some_and(editing)
        {
            return;
        }
        let next =
            control.map(|(key, control)| (key.id.clone(), key.label.clone(), control.clone()));
        // Release state borrows before invoking the host-provided renderer.
        drop((keys, selection, remap, paint));
        if let Some(row) = self.control_row.borrow_mut().take() {
            self.actions.remove(&row);
        }
        *self.shown_control.borrow_mut() = next
            .as_ref()
            .map(|(id, _, control)| (id.clone(), control.clone()));
        if let Some((_, title, control)) = next {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(&title);
            row.set_subtitle("Provided by the plugin for this key.");
            (self.on_control)(&row, control);
            self.actions.add(&row);
            *self.control_row.borrow_mut() = Some(row);
        }
        self.actions.set_visible(shows_actions);
    }

    fn draw(&self, cr: &cairo::Context, width: f64, height: f64) {
        let layout = self.layout.get();
        let (scale, ox, oy) = fit(width, height, layout);
        if scale <= 0.0 || cr.save().is_err() {
            return;
        }
        cr.translate(ox, oy);
        cr.scale(scale, scale);
        let px = 1.0 / scale;
        cr.set_line_join(cairo::LineJoin::Round);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        let margin = CHASSIS * 0.65;
        rounded_rect(
            cr,
            -margin,
            -margin,
            layout.width + margin * 2.0,
            layout.height + margin * 2.0,
            0.3,
        );
        cr.set_source_rgb(0.10, 0.14, 0.16);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.64, 0.75, 0.76, 0.28);
        cr.set_line_width(1.5 * px);
        let _ = cr.stroke();
        let keys = self.keys.borrow();
        let pressed = self.pressed.borrow();
        let selection = self.selection.borrow();
        let focus = selection.focus.filter(|_| self.area.has_focus());
        let animation = self.animation.borrow();
        let preview = animation
            .as_ref()
            .filter(|animation| animation.kind != KeyAnimationKind::Unknown);
        let now = glib::monotonic_time();
        let time = if self.animations_enabled() {
            (now - self.epoch) as f64 / 1e6
        } else {
            STILL_FRAME
        };
        let presses: Vec<Press> = self
            .presses
            .borrow()
            .iter()
            .map(|(x, y, at)| Press {
                x: *x,
                y: *y,
                age: (now - at) as f64 / 1e6,
            })
            .collect();
        let fills: Vec<Option<(f64, f64, f64)>> = keys
            .iter()
            .map(|key| {
                key.color.as_deref().and_then(parse_color).or_else(|| {
                    let (x, y) = center(key);
                    preview.map(|animation| {
                        keyboard_fx::color(
                            animation,
                            time,
                            Spot {
                                x,
                                y,
                                width: layout.width,
                                height: layout.height,
                            },
                            &presses,
                        )
                    })
                })
            })
            .collect();
        // Light spilling onto the chassis, under every key cap.
        for (key, fill) in keys.iter().zip(&fills) {
            if let Some(fill) = fill {
                draw_glow(cr, key, *fill);
            }
        }
        for (index, key) in keys.iter().enumerate() {
            draw_key(
                cr,
                key,
                fills[index],
                KeyState {
                    pressed: pressed.get(index).copied().flatten() == Some(true),
                    selected: selection.selected.get(index).copied().unwrap_or(false),
                    focused: focus == Some(index),
                },
                px,
            );
        }
        let _ = cr.restore();
        if let Some((x0, y0, x1, y1)) = self.band.get() {
            cr.rectangle(x0.min(x1), y0.min(y1), (x1 - x0).abs(), (y1 - y0).abs());
            cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 0.14);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(ACCENT.0, ACCENT.1, ACCENT.2, 0.85);
            cr.set_line_width(1.0);
            let _ = cr.stroke();
        }
    }
}

fn editing(row: &adw::ActionRow) -> bool {
    row.has_css_class("hd-pending-edit")
        || row
            .root()
            .and_downcast::<gtk::Window>()
            .and_then(|window| gtk::prelude::GtkWindowExt::focus(&window))
            .is_some_and(|focus| focus.is_ancestor(row))
}

fn set_title(row: &adw::ActionRow, value: &str) {
    if row.title() != value {
        row.set_title(value);
    }
}

fn set_subtitle(row: &adw::ActionRow, value: &str) {
    if row.subtitle().as_deref() != Some(value) {
        row.set_subtitle(value);
    }
}

/// A dropdown over plain strings whose search filters on the displayed text.
fn searchable_dropdown(labels: &[&str]) -> gtk::DropDown {
    let expression = gtk::PropertyExpression::new(
        gtk::StringObject::static_type(),
        None::<&gtk::Expression>,
        "string",
    );
    let dropdown = gtk::DropDown::new(Some(gtk::StringList::new(labels)), Some(expression));
    dropdown.set_enable_search(true);
    dropdown
}

fn with_value(mut args: Map<String, Value>, value: Value) -> Map<String, Value> {
    args.insert("value".into(), value);
    args
}

fn paint_value(keys: Vec<String>, color: &str) -> Value {
    json!({"keys": keys, "color": color})
}

#[derive(Clone, Copy)]
struct KeyState {
    pressed: bool,
    selected: bool,
    focused: bool,
}

/// A soft halo in the key's colour, scaled by how bright that colour is.
fn draw_glow(cr: &cairo::Context, key: &KeyboardKey, fill: (f64, f64, f64)) {
    let strength = fill.0.max(fill.1).max(fill.2);
    if strength < 0.2 {
        return;
    }
    let (cx, cy) = center(key);
    let reach = key.w.max(key.h) * 0.85;
    let glow = cairo::RadialGradient::new(cx, cy, 0.0, cx, cy, reach);
    glow.add_color_stop_rgba(0.0, fill.0, fill.1, fill.2, 0.34 * strength);
    glow.add_color_stop_rgba(1.0, fill.0, fill.1, fill.2, 0.0);
    if cr.set_source(&glow).is_ok() {
        cr.rectangle(cx - reach, cy - reach, reach * 2.0, reach * 2.0);
        let _ = cr.fill();
    }
}

fn draw_key(
    cr: &cairo::Context,
    key: &KeyboardKey,
    fill: Option<(f64, f64, f64)>,
    state: KeyState,
    px: f64,
) {
    let gap = GAP.min(key.w * 0.2).min(key.h * 0.2);
    let (x, y, w, h) = (
        key.x + gap,
        key.y + gap,
        key.w - gap * 2.0,
        key.h - gap * 2.0,
    );
    let radius = 0.12_f64.min(w * 0.25).min(h * 0.25);
    let base = fill.unwrap_or((0.17, 0.21, 0.23));
    rounded_rect(cr, x, y, w, h, radius);
    cr.set_source_rgb(base.0, base.1, base.2);
    let _ = cr.fill_preserve();
    if fill.is_some() {
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.38);
    } else {
        cr.set_source_rgba(0.67, 0.77, 0.79, 0.34);
    }
    cr.set_line_width(1.5 * px);
    let _ = cr.stroke();
    if fill.is_none() {
        // Hatching means "no colour value", not a black key.
        let _ = cr.save();
        rounded_rect(cr, x, y, w, h, radius);
        cr.clip();
        cr.set_source_rgba(0.61, 0.71, 0.73, 0.22);
        cr.set_line_width(px);
        let step = 0.18;
        let mut offset = -h;
        while offset < w {
            cr.move_to(x + offset, y + h);
            cr.line_to(x + offset + h, y);
            offset += step;
        }
        let _ = cr.stroke();
        let _ = cr.restore();
    }
    let mut label_fill = base;
    if state.pressed {
        rounded_rect(cr, x, y, w, h, radius);
        cr.set_source_rgba(GREEN.0, GREEN.1, GREEN.2, 0.30);
        let _ = cr.fill();
        label_fill = blend(base, GREEN, 0.30);
        let halo = 0.04;
        rounded_rect(
            cr,
            x - halo,
            y - halo,
            w + halo * 2.0,
            h + halo * 2.0,
            radius + halo,
        );
        cr.set_source_rgba(GREEN.0, GREEN.1, GREEN.2, 0.45);
        cr.set_line_width(6.0 * px);
        let _ = cr.stroke();
        let inset = (0.07_f64).min(w * 0.2).min(h * 0.2);
        rounded_rect(
            cr,
            x + inset,
            y + inset,
            w - inset * 2.0,
            h - inset * 2.0,
            (radius - inset).max(0.02),
        );
        cr.set_source_rgb(GREEN.0, GREEN.1, GREEN.2);
        cr.set_line_width(2.5 * px);
        let _ = cr.stroke();
    }
    let ink = label_color(label_fill);
    cr.set_source_rgb(ink.0, ink.1, ink.2);
    draw_label(cr, &key.label, x, y, w, h);
    if state.selected {
        let out = 0.05;
        rounded_rect(
            cr,
            x - out,
            y - out,
            w + out * 2.0,
            h + out * 2.0,
            radius + out,
        );
        cr.set_source_rgb(ACCENT.0, ACCENT.1, ACCENT.2);
        cr.set_line_width(3.0 * px);
        let _ = cr.stroke();
    }
    if state.focused {
        let out = 0.1;
        rounded_rect(
            cr,
            x - out,
            y - out,
            w + out * 2.0,
            h + out * 2.0,
            radius + out,
        );
        cr.set_source_rgba(0.95, 0.97, 0.98, 0.9);
        cr.set_line_width(1.5 * px);
        cr.set_dash(&[4.0 * px, 3.0 * px], 0.0);
        let _ = cr.stroke();
        cr.set_dash(&[], 0.0);
    }
}

/// Plain cairo text, shrunk to fit and then ellipsized; never Pango markup.
fn draw_label(cr: &cairo::Context, label: &str, x: f64, y: f64, w: f64, h: f64) {
    let room = (w - 0.16).max(w * 0.5);
    let mut size = (h * 0.34).min(0.3);
    cr.set_font_size(size);
    let Ok(mut extents) = cr.text_extents(label) else {
        return;
    };
    if extents.width() > room {
        size = (size * room / extents.width()).max(size * 0.6);
        cr.set_font_size(size);
        let Ok(fitted) = cr.text_extents(label) else {
            return;
        };
        extents = fitted;
    }
    let mut shown = std::borrow::Cow::Borrowed(label);
    if extents.width() > room {
        let mut chars: Vec<char> = label.chars().collect();
        while chars.len() > 1 {
            chars.pop();
            let candidate: String = chars.iter().chain(std::iter::once(&'…')).collect();
            let Ok(next) = cr.text_extents(&candidate) else {
                return;
            };
            extents = next;
            shown = std::borrow::Cow::Owned(candidate);
            if extents.width() <= room {
                break;
            }
        }
    }
    cr.move_to(
        x + (w - extents.width()) * 0.5 - extents.x_bearing(),
        y + (h - extents.height()) * 0.5 - extents.y_bearing(),
    );
    let _ = cr.show_text(&shown);
}

fn reading(key: &KeyboardKey, pressed: Option<bool>, remap: Option<&KeyRemap>) -> String {
    let mut parts = vec![
        match pressed {
            Some(true) => "PRESSED".to_owned(),
            Some(false) => "Released".to_owned(),
            None => "Press state unavailable".to_owned(),
        },
        key.color
            .clone()
            .unwrap_or_else(|| "Colour unknown".to_owned()),
    ];
    if let Some(remap) = remap {
        parts.push(
            key.remap
                .as_ref()
                .and_then(|value| remap.options.iter().find(|o| &o.value == value))
                .map_or("Shared remapping unavailable".to_owned(), |option| {
                    format!("→ {}", option.label)
                }),
        );
    }
    parts.join("  ·  ")
}

// ---- GTK-free geometry, colour and selection logic (unit tested) ----

/// Scale and origin mapping layout units to pixels, centred with the chassis margin.
fn fit(width: f64, height: f64, layout: KeyboardLayout) -> (f64, f64, f64) {
    let (lw, lh) = (layout.width + CHASSIS * 2.0, layout.height + CHASSIS * 2.0);
    let scale = ((width - PAD_X) / lw).min((height - PAD_Y) / lh).max(0.0);
    (
        scale,
        (width - lw * scale) * 0.5 + CHASSIS * scale,
        (height - lh * scale) * 0.5 + CHASSIS * scale,
    )
}

/// Canvas height that keeps the layout's aspect ratio at the given width.
fn desired_height(width: f64, layout: KeyboardLayout) -> i32 {
    let (lw, lh) = (layout.width + CHASSIS * 2.0, layout.height + CHASSIS * 2.0);
    let height = ((width - PAD_X).max(0.0) / lw * lh + PAD_Y).round();
    (height as i32).clamp(MIN_HEIGHT, MAX_HEIGHT)
}

fn contains(key: &KeyboardKey, x: f64, y: f64) -> bool {
    x >= key.x && x <= key.x + key.w && y >= key.y && y <= key.y + key.h
}

fn center(key: &KeyboardKey) -> (f64, f64) {
    (key.x + key.w * 0.5, key.y + key.h * 0.5)
}

/// Later keys are drawn on top, so they win where rectangles overlap.
fn hit(keys: &[KeyboardKey], x: f64, y: f64) -> Option<usize> {
    keys.iter().rposition(|key| contains(key, x, y))
}

/// Keys whose rectangles intersect the (unordered) band corners, in layout order.
fn intersecting(keys: &[KeyboardKey], (x0, y0, x1, y1): (f64, f64, f64, f64)) -> Vec<usize> {
    let (left, right) = (x0.min(x1), x0.max(x1));
    let (top, bottom) = (y0.min(y1), y0.max(y1));
    keys.iter()
        .enumerate()
        .filter(|(_, key)| {
            key.x <= right && key.x + key.w >= left && key.y <= bottom && key.y + key.h >= top
        })
        .map(|(index, _)| index)
        .collect()
}

/// Keys whose centres lie in the bounding box spanned by keys `a` and `b`.
fn range(keys: &[KeyboardKey], a: usize, b: usize) -> Vec<usize> {
    let (Some(a), Some(b)) = (keys.get(a), keys.get(b)) else {
        return Vec::new();
    };
    let left = a.x.min(b.x);
    let right = (a.x + a.w).max(b.x + b.w);
    let top = a.y.min(b.y);
    let bottom = (a.y + a.h).max(b.y + b.h);
    keys.iter()
        .enumerate()
        .filter(|(_, key)| {
            let (cx, cy) = center(key);
            (left..=right).contains(&cx) && (top..=bottom).contains(&cy)
        })
        .map(|(index, _)| index)
        .collect()
}

/// Nearest key in a direction. Keys sharing the source's row band (or column
/// band, for Up/Down) win, so Right from a short key reaches a tall Enter
/// rather than dropping a row; sideways distance breaks ties.
/// Returns `None` at the edge instead of jumping far sideways.
fn neighbor(keys: &[KeyboardKey], from: usize, (dx, dy): (f64, f64)) -> Option<usize> {
    let source = keys.get(from)?;
    let (fx, fy) = center(source);
    let band = |key: &KeyboardKey| {
        if dx != 0.0 {
            (key.y, key.y + key.h)
        } else {
            (key.x, key.x + key.w)
        }
    };
    let (start, end) = band(source);
    let candidates = || {
        keys.iter().enumerate().filter_map(move |(index, key)| {
            let (cx, cy) = center(key);
            let (vx, vy) = (cx - fx, cy - fy);
            let along = vx * dx + vy * dy;
            let across = (vx * dy - vy * dx).abs();
            let (low, high) = band(key);
            let aligned = low.max(start) < high.min(end) - 1e-6;
            // Off-band keys must lie roughly ahead, not mostly sideways.
            (index != from && along > 0.1 && (aligned || across <= along * 2.0)).then_some((
                index,
                aligned,
                along + across * 2.0,
            ))
        })
    };
    let aligned = candidates().any(|(_, aligned, _)| aligned);
    candidates()
        .filter(|(_, candidate, _)| *candidate || !aligned)
        .min_by(|a, b| a.2.total_cmp(&b.2))
        .map(|(index, _, _)| index)
}

fn parse_color(value: &str) -> Option<(f64, f64, f64)> {
    let hex = value.strip_prefix('#').filter(|hex| hex.len() == 6)?;
    let channel = |at: usize| {
        u8::from_str_radix(hex.get(at..at + 2)?, 16)
            .ok()
            .map(|value| f64::from(value) / 255.0)
    };
    Some((channel(0)?, channel(2)?, channel(4)?))
}

/// WCAG relative luminance of an sRGB colour.
fn luminance((r, g, b): (f64, f64, f64)) -> f64 {
    let linear = |c: f64| {
        if c <= 0.04045 {
            c / 12.92
        } else {
            ((c + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * linear(r) + 0.7152 * linear(g) + 0.0722 * linear(b)
}

/// Dark or light ink, whichever contrasts more with the key fill.
fn label_color(fill: (f64, f64, f64)) -> (f64, f64, f64) {
    // 0.179 is where contrast against black and white is equal.
    if luminance(fill) > 0.179 {
        (0.04, 0.06, 0.07)
    } else {
        (0.94, 0.96, 0.97)
    }
}

fn blend(a: (f64, f64, f64), b: (f64, f64, f64), t: f64) -> (f64, f64, f64) {
    (
        a.0 + (b.0 - a.0) * t,
        a.1 + (b.1 - a.1) * t,
        a.2 + (b.2 - a.2) * t,
    )
}

/// Selector entries; repeated labels such as two "Shift" keys get their id.
fn display_names(keys: &[KeyboardKey]) -> Vec<String> {
    let mut counts: HashMap<&str, usize> = HashMap::new();
    for key in keys {
        *counts.entry(key.label.as_str()).or_default() += 1;
    }
    keys.iter()
        .map(|key| {
            if counts[key.label.as_str()] > 1 {
                format!("{} ({})", key.label, key.id)
            } else {
                key.label.clone()
            }
        })
        .collect()
}

fn summary(keys: &[KeyboardKey], selection: &Selection) -> String {
    let labels: Vec<&str> = keys
        .iter()
        .zip(&selection.selected)
        .filter(|(_, selected)| **selected)
        .map(|(key, _)| key.label.as_str())
        .collect();
    match labels.len() {
        0 => "No keys selected".to_owned(),
        1 => format!("Selected: {}", labels[0]),
        count => {
            let shown = labels[..count.min(SUMMARY_LABELS)].join(", ");
            let more = if count > SUMMARY_LABELS { ", …" } else { "" };
            format!("Selected: {shown}{more} ({count} keys)")
        }
    }
}

/// Selection by index into the current keys, re-keyed by id when keys change.
#[derive(Debug, Default, Clone, PartialEq)]
struct Selection {
    selected: Vec<bool>,
    anchor: Option<usize>,
    focus: Option<usize>,
}

impl Selection {
    fn count(&self) -> usize {
        self.selected.iter().filter(|selected| **selected).count()
    }

    fn first(&self) -> Option<usize> {
        self.selected.iter().position(|selected| *selected)
    }

    /// The selected key when exactly one is selected.
    fn single(&self) -> Option<usize> {
        let mut selected = self
            .selected
            .iter()
            .enumerate()
            .filter(|(_, selected)| **selected);
        match (selected.next(), selected.next()) {
            (Some((index, _)), None) => Some(index),
            _ => None,
        }
    }

    /// Selected ids in layout order, as submitted to the paint action.
    fn ids(&self, keys: &[KeyboardKey]) -> Vec<String> {
        keys.iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
            .map(|(key, _)| key.id.clone())
            .collect()
    }

    fn only(&mut self, index: usize) {
        if index < self.selected.len() {
            self.selected.fill(false);
            self.selected[index] = true;
            self.anchor = Some(index);
            self.focus = Some(index);
        }
    }

    fn toggle(&mut self, index: usize) {
        if let Some(selected) = self.selected.get_mut(index) {
            *selected = !*selected;
            self.anchor = Some(index);
            self.focus = Some(index);
        }
    }

    /// Shift selection: the rectangle from the anchor to `index`.
    fn extend(&mut self, keys: &[KeyboardKey], index: usize, additive: bool) {
        if index >= self.selected.len() {
            return;
        }
        let anchor = *self.anchor.get_or_insert(index);
        if !additive {
            self.selected.fill(false);
        }
        for key in range(keys, anchor, index) {
            self.selected[key] = true;
        }
        self.focus = Some(index);
    }

    /// Rubber-band selection; additive keeps what was already selected.
    fn replace(&mut self, indices: &[usize], additive: bool) {
        if !additive {
            self.selected.fill(false);
        }
        for &index in indices {
            if let Some(selected) = self.selected.get_mut(index) {
                *selected = true;
            }
        }
        if let Some(&last) = indices.last() {
            self.anchor = indices.first().copied();
            self.focus = Some(last);
        }
    }

    fn all(&mut self) {
        self.selected.fill(true);
    }

    fn clear(&mut self) {
        self.selected.fill(false);
        self.anchor = None;
    }

    /// Carry selection, anchor and focus over to a new key list by id.
    fn remap(&mut self, old: &[KeyboardKey], new: &[KeyboardKey]) {
        let index: HashMap<&str, usize> = new
            .iter()
            .enumerate()
            .map(|(index, key)| (key.id.as_str(), index))
            .collect();
        let moved = |position: Option<usize>| {
            position
                .and_then(|position| old.get(position))
                .and_then(|key| index.get(key.id.as_str()).copied())
        };
        let mut selected = vec![false; new.len()];
        for (key, _) in old
            .iter()
            .zip(&self.selected)
            .filter(|(_, selected)| **selected)
        {
            if let Some(&position) = index.get(key.id.as_str()) {
                selected[position] = true;
            }
        }
        self.anchor = moved(self.anchor);
        self.focus = moved(self.focus);
        self.selected = selected;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(id: &str, x: f64, y: f64, w: f64, h: f64) -> KeyboardKey {
        KeyboardKey {
            id: id.into(),
            label: id.to_uppercase(),
            detail: String::new(),
            x,
            y,
            w,
            h,
            color: None,
            pressed: None,
            remap: None,
            control: None,
        }
    }

    /// q w e / a s d with a tall enter at the right spanning both rows.
    fn board() -> Vec<KeyboardKey> {
        vec![
            key("q", 0.0, 0.0, 1.0, 1.0),
            key("w", 1.0, 0.0, 1.0, 1.0),
            key("e", 2.0, 0.0, 1.0, 1.0),
            key("a", 0.25, 1.0, 1.0, 1.0),
            key("s", 1.25, 1.0, 1.0, 1.0),
            key("d", 2.25, 1.0, 1.0, 1.0),
            key("enter", 3.5, 0.0, 1.5, 2.0),
        ]
    }

    fn selection(len: usize) -> Selection {
        Selection {
            selected: vec![false; len],
            ..Selection::default()
        }
    }

    #[test]
    fn fit_preserves_aspect_and_centres_the_layout() {
        let layout = KeyboardLayout {
            width: 15.0,
            height: 5.0,
        };
        let (scale, ox, oy) = fit(800.0, 400.0, layout);
        let lw = 15.0 + CHASSIS * 2.0;
        assert!((scale - (800.0 - PAD_X) / lw).abs() < 1e-9);
        // The layout, including its chassis margin, is centred in both axes.
        let left = ox - CHASSIS * scale;
        let right = 800.0 - (ox + 15.0 * scale + CHASSIS * scale);
        assert!((left - right).abs() < 1e-9);
        let top = oy - CHASSIS * scale;
        let bottom = 400.0 - (oy + 5.0 * scale + CHASSIS * scale);
        assert!((top - bottom).abs() < 1e-9 && top >= 0.0);
        assert_eq!(fit(10.0, 5.0, layout).0, 0.0);
        let height = desired_height(800.0, layout);
        assert!((MIN_HEIGHT..=MAX_HEIGHT).contains(&height));
        let (scale, _, _) = fit(800.0, f64::from(height), layout);
        assert!((scale * lw - (800.0 - PAD_X)).abs() < 2.0);
        assert_eq!(desired_height(0.0, layout), MIN_HEIGHT);
        assert_eq!(
            desired_height(
                4000.0,
                KeyboardLayout {
                    width: 1.0,
                    height: 1.0
                }
            ),
            MAX_HEIGHT
        );
    }

    #[test]
    fn hit_testing_uses_rectangles_and_prefers_later_keys() {
        let mut keys = board();
        assert_eq!(hit(&keys, 0.5, 0.5), Some(0));
        assert_eq!(hit(&keys, 4.0, 1.9), Some(6));
        assert_eq!(hit(&keys, 0.1, 1.5), None);
        assert_eq!(hit(&keys, -0.1, 0.5), None);
        // An overlapping decoration drawn later wins the click.
        keys.push(key("logo", 0.0, 0.0, 0.5, 0.5));
        assert_eq!(hit(&keys, 0.25, 0.25), Some(7));
    }

    #[test]
    fn rubber_band_and_shift_range_select_spatially() {
        let keys = board();
        assert_eq!(intersecting(&keys, (1.6, 0.4, 0.4, 1.6)), [0, 1, 3, 4]);
        assert_eq!(
            intersecting(&keys, (5.5, 0.0, 6.0, 2.0)),
            Vec::<usize>::new()
        );
        // From q to s covers q, w and a, s (the centres inside the span).
        assert_eq!(range(&keys, 0, 4), [0, 1, 3, 4]);
        assert_eq!(range(&keys, 4, 0), [0, 1, 3, 4]);
        assert_eq!(range(&keys, 2, 2), [2]);
        assert!(range(&keys, 0, 99).is_empty());
    }

    #[test]
    fn arrow_navigation_follows_rows_and_nearest_columns() {
        let keys = board();
        assert_eq!(neighbor(&keys, 0, (1.0, 0.0)), Some(1));
        assert_eq!(neighbor(&keys, 1, (-1.0, 0.0)), Some(0));
        assert_eq!(neighbor(&keys, 0, (-1.0, 0.0)), None);
        assert_eq!(neighbor(&keys, 1, (0.0, 1.0)), Some(4));
        assert_eq!(neighbor(&keys, 4, (0.0, -1.0)), Some(1));
        assert_eq!(neighbor(&keys, 2, (1.0, 0.0)), Some(6));
        assert_eq!(neighbor(&keys, 5, (1.0, 0.0)), Some(6));
        assert_eq!(neighbor(&keys, 6, (-1.0, 0.0)), Some(5));
        // Without a key in the same band, the nearest key in that direction wins.
        assert_eq!(neighbor(&keys, 3, (0.0, -1.0)), Some(0));
        assert_eq!(neighbor(&keys, 6, (0.0, 1.0)), None);
        assert_eq!(neighbor(&keys, 99, (1.0, 0.0)), None);
    }

    #[test]
    fn label_ink_contrasts_with_key_colour() {
        assert_eq!(parse_color("#FF2200"), Some((1.0, 34.0 / 255.0, 0.0)));
        assert_eq!(parse_color("FF2200"), None);
        assert_eq!(parse_color("#FFF"), None);
        assert_eq!(parse_color("#GG0000"), None);
        let dark = label_color((0.0, 0.0, 0.0));
        let light = label_color((1.0, 1.0, 1.0));
        assert!(dark.0 > 0.9 && light.0 < 0.1);
        // Saturated yellow is light, saturated blue is dark.
        assert!(label_color((1.0, 1.0, 0.0)).0 < 0.1);
        assert!(label_color((0.0, 0.0, 1.0)).0 > 0.9);
        assert!((luminance((1.0, 1.0, 1.0)) - 1.0).abs() < 1e-9);
        assert_eq!(
            blend((0.0, 0.0, 0.0), (1.0, 1.0, 1.0), 0.5),
            (0.5, 0.5, 0.5)
        );
    }

    #[test]
    fn selection_click_toggle_extend_and_band() {
        let keys = board();
        let mut selection = selection(keys.len());
        selection.only(1);
        assert_eq!(selection.single(), Some(1));
        selection.toggle(4);
        assert_eq!((selection.count(), selection.single()), (2, None));
        assert_eq!(selection.ids(&keys), ["w", "s"]);
        selection.toggle(4);
        assert_eq!((selection.single(), selection.anchor), (Some(1), Some(4)));
        // Shift extends from the anchor (w) and replaces the selection.
        selection.only(1);
        selection.extend(&keys, 5, false);
        assert_eq!(selection.ids(&keys), ["w", "e", "s", "d"]);
        assert_eq!((selection.anchor, selection.focus), (Some(1), Some(5)));
        // Ctrl+Shift adds the range to the existing selection.
        selection.only(0);
        selection.toggle(6);
        selection.anchor = Some(0);
        selection.extend(&keys, 3, true);
        assert_eq!(selection.ids(&keys), ["q", "a", "enter"]);
        selection.replace(&[2, 5], false);
        assert_eq!(selection.ids(&keys), ["e", "d"]);
        selection.replace(&[0], true);
        assert_eq!(selection.ids(&keys), ["q", "e", "d"]);
        selection.all();
        assert_eq!(selection.count(), keys.len());
        selection.clear();
        assert_eq!((selection.count(), selection.anchor), (0, None));
        selection.only(99);
        selection.toggle(99);
        assert_eq!(selection.count(), 0);
    }

    #[test]
    fn selection_survives_key_changes_by_id() {
        let old = board();
        let mut selection = selection(old.len());
        selection.only(4);
        selection.toggle(6);
        selection.anchor = Some(0);
        // Reordered keys with "q" removed and a new key added.
        let mut new: Vec<_> = old.iter().skip(1).rev().cloned().collect();
        new.push(key("z", 0.0, 2.0, 1.0, 1.0));
        selection.remap(&old, &new);
        assert_eq!(selection.selected.len(), new.len());
        assert_eq!(selection.ids(&new), ["enter", "s"]);
        assert_eq!(selection.anchor, None);
        assert_eq!(
            selection.focus.map(|index| new[index].id.as_str()),
            Some("enter")
        );
    }

    #[test]
    fn summaries_and_selector_names_are_plain_and_unambiguous() {
        let mut keys = board();
        let mut selection = selection(keys.len());
        assert_eq!(summary(&keys, &selection), "No keys selected");
        selection.only(0);
        assert_eq!(summary(&keys, &selection), "Selected: Q");
        selection.toggle(6);
        assert_eq!(summary(&keys, &selection), "Selected: Q, ENTER (2 keys)");
        let many: Vec<_> = (0..12)
            .map(|index| key(&format!("k{index}"), f64::from(index), 0.0, 1.0, 1.0))
            .collect();
        let mut all = self::selection(many.len());
        all.all();
        assert!(summary(&many, &all).ends_with(", K7, … (12 keys)"));
        keys[3].label = "Shift".into();
        keys[5].label = "Shift".into();
        let names = display_names(&keys);
        assert_eq!((names[0].as_str(), names[3].as_str()), ("Q", "Shift (a)"));
        assert_eq!(names[5], "Shift (d)");
    }

    #[test]
    fn shared_remap_requires_a_per_key_assignment_not_a_standalone_control() {
        let mut key = key("fn", 0.0, 0.0, 1.0, 1.0);
        assert!(!key.can_remap());
        key.control = Some(Control::Button {
            label: "Dial mode".into(),
            action: "set-dial-mode".into(),
            args: Map::new(),
            destructive: false,
        });
        assert!(!key.can_remap());
        assert!(key.control.is_some());
        for target in ["default", "disabled", "f13"] {
            key.remap = Some(target.into());
            assert!(key.can_remap());
        }
    }

    #[test]
    fn action_values_carry_layout_order_and_shared_args() {
        let keys = board();
        let mut selection = selection(keys.len());
        selection.only(6);
        selection.toggle(0);
        let mut args = Map::new();
        args.insert("device".into(), "kb".into());
        args.insert("value".into(), "stale".into());
        let args = with_value(args, paint_value(selection.ids(&keys), "#00FF00"));
        assert_eq!(
            Value::Object(args),
            json!({"device": "kb", "value": {"keys": ["q", "enter"], "color": "#00FF00"}})
        );
        let remap = KeyRemap {
            label: "Assign".into(),
            action: "remap-key".into(),
            args: Map::new(),
            options: vec![
                OptionItem {
                    value: "default".into(),
                    label: "Default".into(),
                },
                OptionItem {
                    value: "f13".into(),
                    label: "F13".into(),
                },
            ],
        };
        let mut esc = key("esc", 0.0, 0.0, 1.0, 1.0);
        esc.color = Some("#FF2200".into());
        esc.remap = Some("f13".into());
        assert_eq!(
            reading(&esc, Some(true), Some(&remap)),
            "PRESSED  ·  #FF2200  ·  → F13"
        );
        esc.remap = None;
        esc.color = None;
        assert_eq!(
            reading(&esc, None, Some(&remap)),
            "Press state unavailable  ·  Colour unknown  ·  Shared remapping unavailable"
        );
        assert_eq!(
            reading(&esc, Some(false), None),
            "Released  ·  Colour unknown"
        );
    }

    /// Run alone on an isolated Xvfb display with scratch XDG directories and
    /// HD_NATIVE_STREAM_FIXTURE_DIR set. No HID, input devices, or services.
    #[test]
    #[ignore = "requires isolated Xvfb and HD_NATIVE_STREAM_FIXTURE_DIR"]
    fn native_retry_and_remap_lifecycle() {
        use std::os::unix::fs::PermissionsExt;
        use std::time::{Duration, Instant};
        let fixture = std::path::PathBuf::from(
            std::env::var_os("HD_NATIVE_STREAM_FIXTURE_DIR").expect("isolated fixture directory"),
        );
        assert!(fixture.starts_with(std::env::temp_dir()));
        assert!(backend::root().starts_with(&fixture));
        assert!(hyprdeck_core::store::config_dir().starts_with(&fixture));
        assert!(hyprdeck_core::store::state_dir().starts_with(&fixture));
        std::fs::create_dir_all(&fixture).unwrap();
        let id = "native-retry-fixture";
        let dir = backend::root().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let manifest = crate::protocol::Manifest {
            api_version: crate::protocol::API_VERSION,
            id: id.into(),
            name: "Synthetic retry fixture".into(),
            description: String::new(),
            version: "1.0.0".into(),
            executable: "fixture".into(),
            update_repo: "test/fixture".into(),
            asset: format!("fixture-linux-{}", std::env::consts::ARCH),
        };
        std::fs::write(
            dir.join("plugin.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        std::fs::write(dir.join("fixture"), r#"#!/usr/bin/python3 -I
import json, sys, time
from pathlib import Path
mode = sys.argv[1]
root = Path('.')
count = root / (mode + '-count')
count.write_text(str(int(count.read_text()) + 1 if count.exists() else 1))
if (root / (mode + '-fail')).exists(): sys.exit(1)
if mode == 'keyboard': frame = {'status': 'Synthetic live', 'pressed': ['q']}
else: frame = {'status': 'Synthetic live', 'inputs': [{'id': 'a', 'label': 'A', 'detail': '', 'pressed': True}]}
print(json.dumps(frame), flush=True)
while True:
    if (root / (mode + '-fail')).exists(): sys.exit(1)
    if (root / (mode + '-unavailable')).exists():
        if mode == 'keyboard': frame = {'status': 'Synthetic unavailable', 'pressed': None}
        else: frame = {'status': 'Synthetic unavailable', 'inputs': []}
        print(json.dumps(frame), flush=True)
        (root / (mode + '-unavailable')).unlink()
    time.sleep(.005)
"#).unwrap();
        std::fs::set_permissions(dir.join("fixture"), std::fs::Permissions::from_mode(0o700))
            .unwrap();
        hyprdeck_core::store::save(
            "plugins",
            &backend::Settings {
                enabled: [id.to_owned()].into_iter().collect(),
            },
        )
        .unwrap();

        gtk::init().unwrap();
        fn wait(mut check: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !check() {
                assert!(Instant::now() < deadline, "native lifecycle timed out");
                glib::MainContext::default().iteration(false);
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        fn pump(duration: Duration) {
            let deadline = Instant::now() + duration;
            while Instant::now() < deadline {
                glib::MainContext::default().iteration(false);
                std::thread::sleep(Duration::from_millis(2));
            }
        }
        let starts = || {
            std::fs::read_to_string(dir.join("keyboard-count"))
                .unwrap()
                .parse::<u32>()
                .unwrap()
        };
        let mut keys = board();
        keys[0].remap = Some("default".into());
        keys[1].control = Some(Control::Button {
            label: "Standalone mode".into(),
            action: "set-mode".into(),
            args: Map::new(),
            destructive: false,
        });
        let mut visualization = Visualization::Keyboard {
            name: "Fixture".into(),
            connection: "Synthetic".into(),
            status: "Snapshot".into(),
            layout: KeyboardLayout {
                width: 5.0,
                height: 2.0,
            },
            keys,
            paint: None,
            remap: Some(KeyRemap {
                label: "Assign".into(),
                action: "remap".into(),
                args: Map::new(),
                options: vec![OptionItem {
                    value: "default".into(),
                    label: "Default".into(),
                }],
            }),
            stream: Some(LiveStream {
                args: vec!["keyboard".into()],
            }),
            animation: None,
        };
        let submitted = Rc::new(RefCell::new(Vec::new()));
        let calls = submitted.clone();
        let controls = Rc::new(Cell::new(0));
        let renders = controls.clone();
        let view = KeyboardView::new(
            id,
            &visualization,
            move |_, _| renders.set(renders.get() + 1),
            move |_| {
                let calls = calls.clone();
                Rc::new(move |action, args| calls.borrow_mut().push((action, args)))
            },
        );
        let window = gtk::Window::new();
        window.set_child(Some(&view.widget));
        window.present();
        wait(|| view.inner.pressed.borrow()[0] == Some(true));
        assert_eq!(starts(), 1);
        let generation = view.inner.stream_generation.get();
        for _ in 0..20 {
            view.update(&visualization);
        }
        pump(Duration::from_millis(1100));
        assert_eq!(starts(), 1, "idle healthy stream was restarted");
        assert_eq!(view.inner.stream_generation.get(), generation);

        view.inner.selection.borrow_mut().only(1);
        view.inner.selection_changed();
        assert!(!view.inner.remap_row.choice.is_sensitive());
        assert!(!view.inner.remap_row.apply.is_sensitive());
        assert_eq!(controls.get(), 1, "standalone control was suppressed");
        // Bypass sensitivity to prove the handler itself prevents submission.
        view.inner.remap_row.apply.emit_clicked();
        assert!(submitted.borrow().is_empty());
        view.inner.selection.borrow_mut().only(0);
        view.inner.selection_changed();
        assert!(view.inner.remap_row.apply.is_sensitive());
        view.inner.remap_row.apply.emit_clicked();
        assert_eq!(
            submitted.borrow()[0].1["value"],
            json!({"key":"q", "target":"default"})
        );
        // Eligibility can disappear while a choice is pending on the same key.
        view.inner.remap_row.row.add_css_class("hd-pending-edit");
        if let Visualization::Keyboard { keys, .. } = &mut visualization {
            keys[0].remap = None;
        }
        view.update(&visualization);
        view.inner.remap_row.apply.emit_clicked();
        assert_eq!(submitted.borrow().len(), 1);
        assert!(!view.inner.remap_row.apply.is_sensitive());
        assert!(!view.inner.remap_row.row.has_css_class("hd-pending-edit"));

        std::fs::write(dir.join("keyboard-fail"), b"").unwrap();
        wait(|| view.inner.stream_lease.borrow().is_none());
        assert!(view.inner.pressed.borrow().iter().all(Option::is_none));
        assert!(
            !view.inner.stream_retry.pending(),
            "failure retried without a successful refresh"
        );
        view.update(&visualization);
        assert!(view.inner.stream_retry.pending());
        for _ in 0..20 {
            view.update(&visualization);
        }
        pump(Duration::from_millis(200));
        assert_eq!(starts(), 1, "refresh bypassed backoff");
        // Let one authorized retry fail; there must be no autonomous loop.
        wait(|| starts() == 2 && view.inner.stream_lease.borrow().is_none());
        assert!(!view.inner.stream_retry.pending());
        pump(Duration::from_millis(200));
        assert_eq!(starts(), 2);
        std::fs::remove_file(dir.join("keyboard-fail")).unwrap();
        view.update(&visualization);
        assert!(view.inner.stream_retry.pending());
        pump(Duration::from_millis(200));
        assert_eq!(starts(), 2, "second retry ignored exponential backoff");
        wait(|| starts() == 3 && view.inner.pressed.borrow()[0] == Some(true));
        assert!(!view.inner.stream_retry.pending());
        // A valid unavailable frame is healthy too; never treat silence as failure.
        std::fs::write(dir.join("keyboard-unavailable"), b"").unwrap();
        wait(|| view.inner.pressed.borrow().iter().all(Option::is_none));
        for _ in 0..20 {
            view.update(&visualization);
        }
        assert_eq!(starts(), 3);

        std::fs::write(dir.join("keyboard-fail"), b"").unwrap();
        wait(|| view.inner.stream_lease.borrow().is_none());
        view.update(&visualization);
        assert!(view.inner.stream_retry.pending());
        window.set_visible(false);
        wait(|| !view.inner.mapped.get());
        assert!(!view.inner.stream_retry.pending());
        view.update(&visualization);
        pump(Duration::from_millis(1100));
        assert_eq!(starts(), 3, "hidden view retried");
        std::fs::remove_file(dir.join("keyboard-fail")).unwrap();
        window.present();
        wait(|| starts() == 4 && view.inner.pressed.borrow()[0] == Some(true));

        std::fs::write(dir.join("keyboard-fail"), b"").unwrap();
        wait(|| view.inner.stream_lease.borrow().is_none());
        view.update(&visualization);
        view.disconnect();
        assert!(!view.inner.stream_retry.pending());
        view.inner.start_stream();
        pump(Duration::from_millis(1100));
        assert_eq!(starts(), 4, "paused backend retried");
        std::fs::remove_file(dir.join("keyboard-fail")).unwrap();
        view.update(&visualization);
        wait(|| starts() == 5 && view.inner.pressed.borrow()[0] == Some(true));
        std::fs::write(dir.join("keyboard-fail"), b"").unwrap();
        wait(|| view.inner.stream_lease.borrow().is_none());
        view.update(&visualization);
        window.destroy();
        assert!(!view.inner.stream_retry.pending());
        assert!(!view.inner.mapped.get());
        // Destroy the view itself too, covering both signal cleanup and Drop.
        drop(view);
        pump(Duration::from_millis(1100));
        assert_eq!(starts(), 5, "destroyed view retried");
        crate::controller::native_retry_lifecycle(id, &dir);
    }
}
