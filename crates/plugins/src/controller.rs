//! Fixed, native controller artwork. Plugin strings are always plain text;
//! drawing coordinates and input identities belong to the host, not the backend.
use std::cell::{Cell, RefCell};
use std::f64::consts::PI;
use std::rc::Rc;

use adw::prelude::*;
use gtk::cairo;

use crate::protocol::{Control, ControllerInput, Visualization};

const WIDTH: f64 = 800.0;
const HEIGHT: f64 = 450.0;
const GREEN: (f64, f64, f64) = (0.48, 0.94, 0.67);
const CSS: &str = "
.hd-controller { padding: 18px; }
.hd-controller .hd-controller-eyebrow {
  font-size: 0.75em; font-weight: 700; letter-spacing: 1px; opacity: 0.65;
}
.hd-controller .hd-controller-status {
  border-radius: 10px; padding: 6px 10px;
  background: alpha(currentColor, 0.06); font-size: 0.85em;
}
.hd-controller .hd-controller-stage { background: #10171a; border-radius: 14px; }
.hd-controller .hd-controller-selector { border-radius: 9px; }
.hd-controller .hd-controller-inspector {
  padding: 14px; border-radius: 12px; background: alpha(currentColor, 0.035);
}
.hd-controller .hd-controller-readings { font-family: monospace; font-size: 0.9em; }
.hd-controller .hd-controller-legend { font-size: 0.8em; opacity: 0.75; }
.hd-controller .hd-controller-active { color: #69c991; }
.hd-controller .hd-controller-hint { font-size: 0.85em; opacity: 0.7; }
";

#[derive(Clone, Copy)]
enum Shape {
    Circle {
        x: f64,
        y: f64,
        radius: f64,
    },
    Rect {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}
#[derive(Clone, Copy)]
enum Kind {
    Button,
    Trigger,
    Stick,
    Dpad(i8, i8),
    View,
    Menu,
    Guide,
    Rear,
}
struct Slot {
    id: &'static str,
    label: &'static str,
    glyph: &'static str,
    shape: Shape,
    kind: Kind,
}
impl Slot {
    fn rear(&self) -> bool {
        matches!(self.kind, Kind::Rear)
    }
}
const fn circle(x: f64, y: f64, radius: f64) -> Shape {
    Shape::Circle { x, y, radius }
}
const fn rect(x: f64, y: f64, width: f64, height: f64) -> Shape {
    Shape::Rect {
        x,
        y,
        width,
        height,
    }
}
// One shared layout drives painting, pointer hit testing and accessible selection.
const SLOTS: [Slot; 23] = [
    Slot {
        id: "a",
        label: "A button",
        glyph: "A",
        shape: circle(568.0, 224.0, 21.0),
        kind: Kind::Button,
    },
    Slot {
        id: "b",
        label: "B button",
        glyph: "B",
        shape: circle(606.0, 184.0, 21.0),
        kind: Kind::Button,
    },
    Slot {
        id: "x",
        label: "X button",
        glyph: "X",
        shape: circle(530.0, 184.0, 21.0),
        kind: Kind::Button,
    },
    Slot {
        id: "y",
        label: "Y button",
        glyph: "Y",
        shape: circle(568.0, 144.0, 21.0),
        kind: Kind::Button,
    },
    Slot {
        id: "lb",
        label: "Left bumper · LB",
        glyph: "LB",
        shape: rect(181.0, 65.0, 150.0, 23.0),
        kind: Kind::Button,
    },
    Slot {
        id: "rb",
        label: "Right bumper · RB",
        glyph: "RB",
        shape: rect(469.0, 65.0, 150.0, 23.0),
        kind: Kind::Button,
    },
    Slot {
        id: "lt",
        label: "Left trigger · LT",
        glyph: "LT",
        shape: rect(181.0, 28.0, 150.0, 24.0),
        kind: Kind::Trigger,
    },
    Slot {
        id: "rt",
        label: "Right trigger · RT",
        glyph: "RT",
        shape: rect(469.0, 28.0, 150.0, 24.0),
        kind: Kind::Trigger,
    },
    Slot {
        id: "left_stick",
        label: "Left stick",
        glyph: "L",
        shape: circle(251.0, 191.0, 43.0),
        kind: Kind::Stick,
    },
    Slot {
        id: "right_stick",
        label: "Right stick",
        glyph: "R",
        shape: circle(479.0, 274.0, 42.0),
        kind: Kind::Stick,
    },
    Slot {
        id: "dpad_up",
        label: "D-pad up",
        glyph: "",
        shape: rect(301.0, 232.0, 28.0, 28.0),
        kind: Kind::Dpad(0, -1),
    },
    Slot {
        id: "dpad_down",
        label: "D-pad down",
        glyph: "",
        shape: rect(301.0, 280.0, 28.0, 28.0),
        kind: Kind::Dpad(0, 1),
    },
    Slot {
        id: "dpad_left",
        label: "D-pad left",
        glyph: "",
        shape: rect(277.0, 256.0, 28.0, 28.0),
        kind: Kind::Dpad(-1, 0),
    },
    Slot {
        id: "dpad_right",
        label: "D-pad right",
        glyph: "",
        shape: rect(325.0, 256.0, 28.0, 28.0),
        kind: Kind::Dpad(1, 0),
    },
    Slot {
        id: "view",
        label: "View button",
        glyph: "",
        shape: circle(365.0, 184.0, 15.0),
        kind: Kind::View,
    },
    Slot {
        id: "menu",
        label: "Menu button",
        glyph: "",
        shape: circle(435.0, 184.0, 15.0),
        kind: Kind::Menu,
    },
    Slot {
        id: "guide",
        label: "Guide button",
        glyph: "",
        shape: circle(400.0, 133.0, 19.0),
        kind: Kind::Guide,
    },
    Slot {
        id: "m1",
        label: "Rear M1",
        glyph: "M1",
        shape: rect(218.0, 126.0, 64.0, 30.0),
        kind: Kind::Rear,
    },
    Slot {
        id: "m2",
        label: "Rear M2",
        glyph: "M2",
        shape: rect(518.0, 126.0, 64.0, 30.0),
        kind: Kind::Rear,
    },
    Slot {
        id: "m3",
        label: "Rear M3",
        glyph: "M3",
        shape: rect(231.0, 207.0, 57.0, 93.0),
        kind: Kind::Rear,
    },
    Slot {
        id: "m4",
        label: "Rear M4",
        glyph: "M4",
        shape: rect(512.0, 207.0, 57.0, 93.0),
        kind: Kind::Rear,
    },
    Slot {
        id: "m5",
        label: "Rear M5",
        glyph: "M5",
        shape: rect(310.0, 217.0, 44.0, 71.0),
        kind: Kind::Rear,
    },
    Slot {
        id: "m6",
        label: "Rear M6",
        glyph: "M6",
        shape: rect(446.0, 217.0, 44.0, 71.0),
        kind: Kind::Rear,
    },
];

type ControlCallback = dyn Fn(&adw::ActionRow, Control);

pub(crate) struct ControllerView {
    pub widget: gtk::Widget,
    inner: Rc<Inner>,
}
struct Inner {
    name: gtk::Label,
    connection: gtk::Label,
    status: gtk::Label,
    area: gtk::DrawingArea,
    front: gtk::ToggleButton,
    rear_button: gtk::ToggleButton,
    selector: gtk::DropDown,
    inspector_title: gtk::Label,
    readings: gtk::Label,
    detail: gtk::Label,
    capability: gtk::Label,
    actions: adw::PreferencesGroup,
    action_row: RefCell<Option<adw::ActionRow>>,
    shown_control: RefCell<Option<(usize, Control)>>,
    inputs: RefCell<[Option<ControllerInput>; SLOTS.len()]>,
    selected: Cell<usize>,
    front_selection: Cell<usize>,
    rear_selection: Cell<usize>,
    rear: Cell<bool>,
    hovered: Cell<Option<usize>>,
    on_control: Box<ControlCallback>,
    body_fill: cairo::LinearGradient,
}

impl ControllerView {
    pub(crate) fn new(
        visualization: &Visualization,
        on_control: impl Fn(&adw::ActionRow, Control) + 'static,
    ) -> Self {
        ensure_css();
        let root = gtk::Box::new(gtk::Orientation::Vertical, 14);
        root.add_css_class("card");
        root.add_css_class("hd-controller");
        let eyebrow = text("CONTROLLER", "hd-controller-eyebrow");
        root.append(&eyebrow);
        let name = text("", "title-2");
        root.append(&name);
        let connection = text("", "dim-label");
        root.append(&connection);

        let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        let views = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        views.add_css_class("linked");
        views.set_valign(gtk::Align::Center);
        let front = gtk::ToggleButton::with_label("Front");
        let rear_button = gtk::ToggleButton::with_label("Rear");
        rear_button.set_group(Some(&front));
        front.set_active(true);
        front.set_tooltip_text(Some("Show standard buttons, sticks and triggers"));
        rear_button.set_tooltip_text(Some(
            "Inspect extra inputs; state and capabilities are supplied by the plugin",
        ));
        views.append(&front);
        views.append(&rear_button);
        toolbar.append(&views);
        let status = text("", "hd-controller-status");
        status.set_hexpand(true);
        status.set_xalign(1.0);
        toolbar.append(&status);
        root.append(&toolbar);

        let area = gtk::DrawingArea::builder()
            .content_height(360)
            .hexpand(true)
            .build();
        area.add_css_class("hd-controller-stage");
        area.update_property(&[gtk::accessible::Property::Label(
            "Controller diagram. Select an input with the input selector below, or click a control.",
        )]);
        root.append(&area);
        let legend = gtk::Box::new(gtk::Orientation::Horizontal, 18);
        let pressed = text("● Pressed", "hd-controller-legend");
        pressed.add_css_class("hd-controller-active");
        legend.append(&pressed);
        legend.append(&text("○ Released", "hd-controller-legend"));
        legend.append(&text("┄ Unavailable", "hd-controller-legend"));
        root.append(&legend);

        let selector_label = text("_Inspect input", "heading");
        selector_label.set_use_underline(true);
        let labels = SLOTS.each_ref().map(|slot| slot.label);
        let selector = gtk::DropDown::from_strings(&labels);
        selector.set_enable_search(true);
        selector.set_hexpand(true);
        selector.add_css_class("hd-controller-selector");
        selector.update_property(&[gtk::accessible::Property::Label("Inspect controller input")]);
        selector_label.set_mnemonic_widget(Some(&selector));
        root.append(&selector_label);
        root.append(&selector);

        let inspector = gtk::Box::new(gtk::Orientation::Vertical, 8);
        inspector.add_css_class("hd-controller-inspector");
        let inspector_title = text("", "heading");
        let readings = text("", "hd-controller-readings");
        readings.set_selectable(true);
        let detail = text("", "");
        detail.set_selectable(true);
        let capability = text("", "hd-controller-hint");
        inspector.append(&inspector_title);
        inspector.append(&readings);
        inspector.append(&detail);
        inspector.append(&capability);
        let actions = adw::PreferencesGroup::builder()
            .title("Mapping and actions")
            .build();
        actions.set_visible(false);
        inspector.append(&actions);
        root.append(&inspector);
        root.append(&text(
            "Sampled input, not an event history. Rear positions are a generic layout; state and actions are reported by the plugin.",
            "hd-controller-hint",
        ));

        let body_fill = cairo::LinearGradient::new(0.0, 85.0, 0.0, 410.0);
        body_fill.add_color_stop_rgb(0.0, 0.25, 0.30, 0.32);
        body_fill.add_color_stop_rgb(0.5, 0.16, 0.20, 0.22);
        body_fill.add_color_stop_rgb(1.0, 0.10, 0.14, 0.16);
        let inner = Rc::new(Inner {
            name,
            connection,
            status,
            area,
            front,
            rear_button,
            selector,
            inspector_title,
            readings,
            detail,
            capability,
            actions,
            action_row: RefCell::new(None),
            shown_control: RefCell::new(None),
            inputs: RefCell::new(std::array::from_fn(|_| None)),
            selected: Cell::new(0),
            front_selection: Cell::new(0),
            rear_selection: Cell::new(17),
            rear: Cell::new(false),
            hovered: Cell::new(None),
            on_control: Box::new(on_control),
            body_fill,
        });
        let weak = Rc::downgrade(&inner);
        inner.area.set_draw_func(move |_, cr, width, height| {
            if let Some(inner) = weak.upgrade() {
                inner.draw(cr, f64::from(width), f64::from(height));
            }
        });
        let weak = Rc::downgrade(&inner);
        inner.selector.connect_selected_notify(move |selector| {
            if let Some(inner) = weak.upgrade() {
                inner.select(selector.selected() as usize, true);
            }
        });
        let weak = Rc::downgrade(&inner);
        inner.front.connect_toggled(move |button| {
            if button.is_active()
                && let Some(inner) = weak.upgrade()
            {
                inner.set_rear(false);
            }
        });
        let weak = Rc::downgrade(&inner);
        inner.rear_button.connect_toggled(move |button| {
            if button.is_active()
                && let Some(inner) = weak.upgrade()
            {
                inner.set_rear(true);
            }
        });
        let click = gtk::GestureClick::new();
        click.set_button(1);
        let weak = Rc::downgrade(&inner);
        click.connect_released(move |_, _, x, y| {
            if let Some(inner) = weak.upgrade()
                && let Some(index) = inner.hit(x, y)
            {
                inner.select(index, false);
            }
        });
        inner.area.add_controller(click);
        let motion = gtk::EventControllerMotion::new();
        let weak = Rc::downgrade(&inner);
        motion.connect_motion(move |_, x, y| {
            if let Some(inner) = weak.upgrade() {
                let hit = inner.hit(x, y);
                if inner.hovered.replace(hit) != hit {
                    inner.area.set_cursor_from_name(hit.map(|_| "pointer"));
                    let inputs = inner.inputs.borrow();
                    inner.area.set_tooltip_text(hit.map(|i| {
                        inputs[i]
                            .as_ref()
                            .map_or(SLOTS[i].label, |input| input.label.as_str())
                    }));
                }
            }
        });
        let weak = Rc::downgrade(&inner);
        motion.connect_leave(move |_| {
            if let Some(inner) = weak.upgrade() {
                inner.hovered.set(None);
                inner.area.set_cursor_from_name(None);
                inner.area.set_tooltip_text(None);
            }
        });
        inner.area.add_controller(motion);
        let view = Self {
            widget: root.upcast(),
            inner,
        };
        view.update(visualization);
        view.inner.refresh_inspector();
        view
    }

    /// Refresh samples without replacing the drawing, selector or inspector.
    /// Only genuinely changed selected control metadata replaces its action row.
    pub(crate) fn update(&self, visualization: &Visualization) {
        let Visualization::Controller {
            name,
            connection,
            status,
            inputs,
        } = visualization;
        set_text(&self.inner.name, name);
        set_text(&self.inner.connection, connection);
        set_text(&self.inner.status, status);
        let mut incoming: [Option<&ControllerInput>; SLOTS.len()] = [None; SLOTS.len()];
        for input in inputs {
            if let Some(index) = SLOTS.iter().position(|slot| slot.id == input.id) {
                incoming[index] = Some(input);
            }
        }
        let mut redraw = false;
        let mut inspect = false;
        {
            let mut current = self.inner.inputs.borrow_mut();
            for (index, (old, new)) in current.iter_mut().zip(incoming).enumerate() {
                match (old.as_mut(), new) {
                    (Some(old), Some(new)) => {
                        if old == new {
                            continue;
                        }
                        inspect |= index == self.inner.selected.get();
                        redraw |= old.pressed != new.pressed
                            || old.value != new.value
                            || old.x != new.x
                            || old.y != new.y;
                        // Keep string storage and avoid copying unchanged metadata
                        // through frequent sample-only changes; identity is fixed.
                        if old.label != new.label {
                            old.label.clone_from(&new.label);
                        }
                        if old.detail != new.detail {
                            old.detail.clone_from(&new.detail);
                        }
                        old.pressed = new.pressed;
                        old.value = new.value;
                        old.x = new.x;
                        old.y = new.y;
                        if old.control != new.control {
                            old.control.clone_from(&new.control);
                        }
                    }
                    (None, None) => {}
                    (_, new) => {
                        *old = new.cloned();
                        redraw = true;
                        inspect |= index == self.inner.selected.get();
                    }
                }
            }
        }
        if redraw {
            self.inner.area.queue_draw();
        }
        if inspect {
            self.inner.refresh_inspector();
        }
    }
}

impl Inner {
    fn set_rear(&self, rear: bool) {
        if self.rear.replace(rear) != rear {
            self.hovered.set(None);
            self.area.set_cursor_from_name(None);
            self.area.set_tooltip_text(None);
            if SLOTS[self.selected.get()].rear() != rear {
                let index = if rear {
                    self.rear_selection.get()
                } else {
                    self.front_selection.get()
                };
                self.select(index, false);
            }
            self.area.queue_draw();
        }
    }
    fn select(&self, index: usize, reveal: bool) {
        let Some(slot) = SLOTS.get(index) else { return };
        if slot.rear() {
            self.rear_selection.set(index);
        } else {
            self.front_selection.set(index);
        }
        if self.selected.replace(index) != index {
            self.refresh_inspector();
            self.area.queue_draw();
        }
        if self.selector.selected() != index as u32 {
            self.selector.set_selected(index as u32);
        }
        if reveal {
            if slot.rear() {
                self.rear_button.set_active(true);
            } else {
                self.front.set_active(true);
            }
        }
    }
    fn refresh_inspector(&self) {
        let index = self.selected.get();
        let slot = &SLOTS[index];
        let inputs = self.inputs.borrow();
        let input = inputs[index].as_ref();
        set_text(
            &self.inspector_title,
            input.map_or(slot.label, |input| &input.label),
        );
        set_text(&self.readings, &reading(slot, input));
        let detail = input
            .map(|input| input.detail.as_str())
            .filter(|detail| !detail.is_empty());
        set_text(&self.detail, detail.unwrap_or(if slot.rear() {
            "Rear-button positions are illustrative. Independent physical state is shown only when explicitly reported by the plugin."
        } else {
            "Select a control to inspect the input reported by your device. Unavailable means the plugin has no sample for this input."
        }));
        let control = input.and_then(|input| input.control.as_ref());
        set_text(
            &self.capability,
            if control.is_some() {
                "Plugin-provided control · Apply a change below."
            } else {
                "Inspection only · No mapping action is exposed for this input."
            },
        );
        let same_control = match (&*self.shown_control.borrow(), control) {
            (None, None) => true,
            (Some((old_index, old)), Some(new)) => *old_index == index && old == new,
            _ => false,
        };
        if same_control {
            if let Some(row) = self.action_row.borrow().as_ref() {
                row.set_title(input.map_or(slot.label, |input| &input.label));
            }
            return;
        }
        // Live samples and capability values must not discard an in-progress edit.
        // Input removal still revokes the action immediately.
        if control.is_some()
            && self
                .shown_control
                .borrow()
                .as_ref()
                .is_some_and(|(old_index, old)| {
                    *old_index == index
                        && control.is_some_and(|new| crate::products::compatible_control(old, new))
                })
            && self.action_row.borrow().as_ref().is_some_and(|row| {
                row.has_css_class("hd-pending-edit")
                    || row
                        .root()
                        .and_downcast::<gtk::Window>()
                        .and_then(|window| gtk::prelude::GtkWindowExt::focus(&window))
                        .is_some_and(|focus| focus.is_ancestor(row))
            })
        {
            return;
        }
        // Release sample borrows before invoking the host-provided renderer.
        let next = control.cloned();
        let title = input.map_or(slot.label, |input| &input.label).to_owned();
        drop(inputs);
        if let Some(row) = self.action_row.borrow_mut().take() {
            self.actions.remove(&row);
        }
        *self.shown_control.borrow_mut() = next.as_ref().map(|control| (index, control.clone()));
        self.actions.set_visible(next.is_some());
        if let Some(control) = next {
            let row = adw::ActionRow::builder().use_markup(false).build();
            row.set_title(&title);
            row.set_subtitle("Provided by the plugin; no mapping is inferred from live input.");
            (self.on_control)(&row, control);
            self.actions.add(&row);
            *self.action_row.borrow_mut() = Some(row);
        }
    }
    fn hit(&self, x: f64, y: f64) -> Option<usize> {
        let (scale, ox, oy) = fit(f64::from(self.area.width()), f64::from(self.area.height()));
        if scale <= 0.0 {
            return None;
        }
        let x = (x - ox) / scale;
        let y = (y - oy) / scale;
        SLOTS.iter().position(|slot| {
            slot.rear() == self.rear.get()
                && match slot.shape {
                    Shape::Circle {
                        x: cx,
                        y: cy,
                        radius,
                    } => (x - cx).hypot(y - cy) <= radius + 4.0,
                    Shape::Rect {
                        x: rx,
                        y: ry,
                        width,
                        height,
                    } => {
                        x >= rx - 3.0
                            && x <= rx + width + 3.0
                            && y >= ry - 3.0
                            && y <= ry + height + 3.0
                    }
                }
        })
    }
    fn draw(&self, cr: &cairo::Context, width: f64, height: f64) {
        let (scale, ox, oy) = fit(width, height);
        if scale <= 0.0 || cr.save().is_err() {
            return;
        }
        cr.translate(ox, oy);
        cr.scale(scale, scale);
        cr.set_line_join(cairo::LineJoin::Round);
        cr.set_line_cap(cairo::LineCap::Round);
        cr.select_font_face("Sans", cairo::FontSlant::Normal, cairo::FontWeight::Bold);
        // Artwork is built directly from constants; no per-frame geometry vectors,
        // asset parsing, string formatting or gradient construction.
        cr.translate(0.0, 7.0);
        body_path(cr);
        cr.set_source_rgba(0.0, 0.0, 0.0, 0.32);
        let _ = cr.fill();
        cr.translate(0.0, -7.0);
        body_path(cr);
        let _ = cr.set_source(&self.body_fill);
        let _ = cr.fill_preserve();
        cr.set_source_rgba(0.64, 0.75, 0.76, 0.32);
        cr.set_line_width(1.5);
        let _ = cr.stroke();
        grips(cr);
        cr.move_to(249.0, 98.0);
        cr.curve_to(302.0, 86.0, 498.0, 86.0, 551.0, 98.0);
        cr.set_source_rgba(GREEN.0, GREEN.1, GREEN.2, 0.42);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
        if self.rear.get() {
            rounded_rect(cr, 361.0, 130.0, 78.0, 123.0, 15.0);
            cr.set_source_rgba(0.05, 0.08, 0.10, 0.65);
            let _ = cr.fill_preserve();
            cr.set_source_rgba(0.53, 0.62, 0.64, 0.23);
            let _ = cr.stroke();
            cr.set_source_rgba(0.73, 0.81, 0.81, 0.6);
            centered_text(cr, "REAR", 400.0, 183.0, 11.0);
            centered_text(cr, "INSPECT", 400.0, 201.0, 9.0);
        } else {
            cr.arc(315.0, 270.0, 48.0, 0.0, 2.0 * PI);
            cr.set_source_rgba(0.04, 0.07, 0.08, 0.65);
            let _ = cr.fill();
            cr.set_source_rgba(0.60, 0.70, 0.71, 0.42);
            centered_text(cr, "L", 251.0, 250.0, 10.0);
            centered_text(cr, "R", 479.0, 330.0, 10.0);
        }
        let inputs = self.inputs.borrow();
        for (index, slot) in SLOTS.iter().enumerate() {
            if slot.rear() == self.rear.get() {
                draw_slot(
                    cr,
                    slot,
                    inputs[index].as_ref(),
                    self.selected.get() == index,
                );
            }
        }
        cr.set_source_rgba(0.65, 0.75, 0.76, 0.65);
        centered_text(
            cr,
            if self.rear.get() {
                "REAR CONTROLS · GENERIC LAYOUT"
            } else {
                "SELECT A CONTROL TO INSPECT"
            },
            400.0,
            431.0,
            10.0,
        );
        let _ = cr.restore();
    }
}

fn text(value: &str, class: &str) -> gtk::Label {
    let label = gtk::Label::builder()
        .label(value)
        .wrap(true)
        .xalign(0.0)
        .build();
    if !class.is_empty() {
        label.add_css_class(class);
    }
    label
}
fn set_text(label: &gtk::Label, value: &str) {
    if label.text().as_str() != value {
        label.set_text(value);
    }
}
fn ensure_css() {
    thread_local! { static INSTALLED: Cell<bool> = const { Cell::new(false) }; }
    if INSTALLED.get() {
        return;
    }
    if let Some(display) = gtk::gdk::Display::default() {
        let provider = gtk::CssProvider::new();
        provider.load_from_string(CSS);
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
        INSTALLED.set(true);
    }
}
fn reading(slot: &Slot, input: Option<&ControllerInput>) -> String {
    if slot.rear() && input.and_then(|input| input.pressed).is_none() {
        return "Independent physical state: unavailable".to_owned();
    }
    let pressed = match input.and_then(|input| input.pressed) {
        Some(true) => "Pressed",
        Some(false) => "Released",
        None => "Press state unavailable",
    };
    match slot.kind {
        Kind::Stick => {
            let x = input
                .and_then(|input| input.x)
                .map(|value| format!("{value:+.2}"))
                .unwrap_or_else(|| "unavailable".into());
            let y = input
                .and_then(|input| input.y)
                .map(|value| format!("{value:+.2}"))
                .unwrap_or_else(|| "unavailable".into());
            format!("{pressed} · X {x} · Y {y}\nAxes −1…+1; negative Y points up.")
        }
        Kind::Trigger => match input.and_then(|input| input.value) {
            Some(value) => format!("{pressed} · {:.0}% · {value:.3} / 1.000", value * 100.0),
            None => format!("{pressed} · Travel unavailable"),
        },
        _ => match input.and_then(|input| input.value) {
            Some(value) => format!("{pressed} · Value {value:.3} / 1.000"),
            None => pressed.to_owned(),
        },
    }
}
fn fit(width: f64, height: f64) -> (f64, f64, f64) {
    let scale = ((width - 16.0) / WIDTH)
        .min((height - 12.0) / HEIGHT)
        .max(0.0);
    (
        scale,
        (width - WIDTH * scale) * 0.5,
        (height - HEIGHT * scale) * 0.5,
    )
}
fn body_path(cr: &cairo::Context) {
    cr.new_path();
    cr.move_to(235.0, 92.0);
    cr.curve_to(285.0, 77.0, 515.0, 77.0, 565.0, 92.0);
    cr.curve_to(615.0, 102.0, 652.0, 145.0, 663.0, 194.0);
    cr.curve_to(683.0, 243.0, 708.0, 344.0, 675.0, 397.0);
    cr.curve_to(664.0, 414.0, 640.0, 416.0, 624.0, 402.0);
    cr.curve_to(578.0, 361.0, 566.0, 300.0, 537.0, 292.0);
    cr.curve_to(491.0, 305.0, 309.0, 305.0, 263.0, 292.0);
    cr.curve_to(234.0, 300.0, 222.0, 361.0, 176.0, 402.0);
    cr.curve_to(160.0, 416.0, 136.0, 414.0, 125.0, 397.0);
    cr.curve_to(92.0, 344.0, 117.0, 243.0, 137.0, 194.0);
    cr.curve_to(148.0, 145.0, 185.0, 102.0, 235.0, 92.0);
    cr.close_path();
}
fn grips(cr: &cairo::Context) {
    for mirror in [false, true] {
        let _ = cr.save();
        if mirror {
            cr.translate(WIDTH, 0.0);
            cr.scale(-1.0, 1.0);
        }
        cr.move_to(166.0, 226.0);
        cr.curve_to(181.0, 225.0, 217.0, 245.0, 236.0, 280.0);
        cr.curve_to(219.0, 307.0, 201.0, 359.0, 175.0, 386.0);
        cr.curve_to(167.0, 395.0, 150.0, 399.0, 142.0, 383.0);
        cr.curve_to(126.0, 347.0, 144.0, 269.0, 166.0, 226.0);
        cr.close_path();
        cr.set_source_rgba(0.025, 0.045, 0.05, 0.48);
        let _ = cr.fill_preserve();
        let _ = cr.save();
        cr.clip();
        cr.set_source_rgba(0.56, 0.65, 0.66, 0.12);
        cr.set_line_width(1.0);
        for n in 0..9 {
            let y = 247.0 + f64::from(n) * 17.0;
            cr.move_to(130.0, y);
            cr.line_to(230.0, y + 35.0);
        }
        let _ = cr.stroke();
        let _ = cr.restore();
        let _ = cr.restore();
    }
}
fn rounded_rect(cr: &cairo::Context, x: f64, y: f64, width: f64, height: f64, radius: f64) {
    let r = radius.min(width * 0.5).min(height * 0.5);
    cr.new_sub_path();
    cr.arc(x + width - r, y + r, r, -PI * 0.5, 0.0);
    cr.arc(x + width - r, y + height - r, r, 0.0, PI * 0.5);
    cr.arc(x + r, y + height - r, r, PI * 0.5, PI);
    cr.arc(x + r, y + r, r, PI, PI * 1.5);
    cr.close_path();
}
fn shape_path(cr: &cairo::Context, shape: Shape, extra: f64) {
    cr.new_path();
    match shape {
        Shape::Circle { x, y, radius } => cr.arc(x, y, radius + extra, 0.0, 2.0 * PI),
        Shape::Rect {
            x,
            y,
            width,
            height,
        } => rounded_rect(
            cr,
            x - extra,
            y - extra,
            width + extra * 2.0,
            height + extra * 2.0,
            8.0 + extra,
        ),
    }
}
fn center(shape: Shape) -> (f64, f64) {
    match shape {
        Shape::Circle { x, y, .. } => (x, y),
        Shape::Rect {
            x,
            y,
            width,
            height,
        } => (x + width * 0.5, y + height * 0.5),
    }
}
fn centered_text(cr: &cairo::Context, value: &str, x: f64, y: f64, size: f64) {
    cr.set_font_size(size);
    if let Ok(extents) = cr.text_extents(value) {
        cr.move_to(
            x - extents.width() * 0.5 - extents.x_bearing(),
            y - extents.height() * 0.5 - extents.y_bearing(),
        );
        let _ = cr.show_text(value);
    }
}
fn draw_slot(cr: &cairo::Context, slot: &Slot, input: Option<&ControllerInput>, selected: bool) {
    // Only explicit input samples are used; never infer a rear press from a
    // mapped front button. Missing rear inputs remain inspectable placeholders.
    let pressed = input.and_then(|input| input.pressed) == Some(true);
    let available = input.is_some_and(|input| match slot.kind {
        Kind::Trigger => input.value.is_some() || input.pressed.is_some(),
        Kind::Stick => input.x.is_some() || input.y.is_some() || input.pressed.is_some(),
        _ => input.pressed.is_some(),
    });
    let press_known = input.and_then(|input| input.pressed).is_some();
    let border_known = if matches!(slot.kind, Kind::Trigger) {
        available
    } else {
        press_known
    };
    if selected {
        shape_path(cr, slot.shape, 5.0);
        cr.set_source_rgba(GREEN.0, GREEN.1, GREEN.2, 0.94);
        cr.set_line_width(2.0);
        let _ = cr.stroke();
    }
    shape_path(cr, slot.shape, 0.0);
    if pressed {
        cr.set_source_rgb(GREEN.0, GREEN.1, GREEN.2);
    } else {
        cr.set_source_rgb(0.08, 0.12, 0.14);
    }
    let _ = cr.fill_preserve();
    cr.set_source_rgba(0.67, 0.77, 0.79, if border_known { 0.58 } else { 0.28 });
    cr.set_line_width(1.5);
    if !border_known {
        cr.set_dash(&[3.0, 4.0], 0.0);
    }
    let _ = cr.stroke();
    cr.set_dash(&[], 0.0);
    let (x, y) = center(slot.shape);
    if let Kind::Trigger = slot.kind
        && let Shape::Rect {
            x,
            y,
            width,
            height,
        } = slot.shape
    {
        if let Some(value) = input.and_then(|input| input.value) {
            if value > 0.0 {
                rounded_rect(
                    cr,
                    x + 4.0,
                    y + 4.0,
                    (width - 8.0) * value,
                    height - 8.0,
                    5.0,
                );
                cr.set_source_rgba(GREEN.0, GREEN.1, GREEN.2, 0.8);
                let _ = cr.fill();
            }
        } else {
            // Hatching is not an empty trigger bar: there is no value.
            let _ = cr.save();
            shape_path(cr, slot.shape, -3.0);
            cr.clip();
            cr.set_source_rgba(0.61, 0.71, 0.73, 0.22);
            cr.set_line_width(1.0);
            for n in 0..12 {
                let px = x + f64::from(n) * 14.0;
                cr.move_to(px, y + height);
                cr.line_to(px + height, y);
            }
            let _ = cr.stroke();
            let _ = cr.restore();
        }
    }
    if pressed {
        cr.set_source_rgb(0.04, 0.12, 0.08);
    } else {
        cr.set_source_rgba(0.85, 0.91, 0.92, if available { 0.95 } else { 0.46 });
    }
    match slot.kind {
        Kind::Stick => {
            let Shape::Circle { radius, .. } = slot.shape else {
                return;
            };
            let travel = radius * 0.53;
            cr.set_line_width(1.0);
            if pressed {
                cr.set_source_rgba(0.04, 0.12, 0.08, 0.3);
            } else {
                cr.set_source_rgba(0.61, 0.73, 0.75, 0.24);
            }
            cr.move_to(x - travel, y);
            cr.line_to(x + travel, y);
            cr.move_to(x, y - travel);
            cr.line_to(x, y + travel);
            let _ = cr.stroke();
            cr.arc(x, y, travel, 0.0, 2.0 * PI);
            let _ = cr.stroke();
            let ax = input.and_then(|input| input.x);
            let ay = input.and_then(|input| input.y);
            if pressed {
                cr.set_source_rgb(0.04, 0.12, 0.08);
            } else {
                cr.set_source_rgb(GREEN.0, GREEN.1, GREEN.2);
            }
            match (ax, ay) {
                (Some(ax), Some(ay)) => {
                    cr.arc(x + ax * travel, y + ay * travel, 8.0, 0.0, 2.0 * PI);
                    let _ = cr.fill();
                }
                (Some(ax), None) => {
                    cr.move_to(x + ax * travel, y - 6.0);
                    cr.line_to(x + ax * travel, y + 6.0);
                    cr.set_line_width(3.0);
                    let _ = cr.stroke();
                }
                (None, Some(ay)) => {
                    cr.move_to(x - 6.0, y + ay * travel);
                    cr.line_to(x + 6.0, y + ay * travel);
                    cr.set_line_width(3.0);
                    let _ = cr.stroke();
                }
                (None, None) => {
                    cr.set_source_rgba(0.78, 0.86, 0.87, 0.46);
                    centered_text(cr, "?", x, y, 18.0);
                }
            }
        }
        Kind::Dpad(dx, dy) => {
            let (dx, dy) = (f64::from(dx), f64::from(dy));
            cr.move_to(x + dx * 6.0, y + dy * 6.0);
            cr.line_to(x - dx * 4.0 - dy * 5.0, y - dy * 4.0 + dx * 5.0);
            cr.line_to(x - dx * 4.0 + dy * 5.0, y - dy * 4.0 - dx * 5.0);
            cr.close_path();
            let _ = cr.fill();
        }
        Kind::Menu => {
            cr.set_line_width(1.5);
            for offset in [-4.0, 0.0, 4.0] {
                cr.move_to(x - 6.0, y + offset);
                cr.line_to(x + 6.0, y + offset);
            }
            let _ = cr.stroke();
        }
        Kind::View => {
            cr.set_line_width(1.3);
            cr.rectangle(x - 6.0, y - 3.0, 8.0, 7.0);
            cr.rectangle(x - 2.0, y - 6.0, 8.0, 7.0);
            let _ = cr.stroke();
        }
        Kind::Guide => {
            cr.set_line_width(1.7);
            cr.move_to(x - 7.0, y);
            cr.line_to(x, y - 6.0);
            cr.line_to(x + 7.0, y);
            cr.move_to(x - 5.0, y - 1.0);
            cr.line_to(x - 5.0, y + 6.0);
            cr.line_to(x + 5.0, y + 6.0);
            cr.line_to(x + 5.0, y - 1.0);
            let _ = cr.stroke();
        }
        Kind::Rear => {
            centered_text(cr, slot.glyph, x, y - 5.0, 12.0);
            centered_text(
                cr,
                if available {
                    if pressed { "●" } else { "○" }
                } else {
                    "—"
                },
                x,
                y + 11.0,
                12.0,
            );
        }
        _ => centered_text(
            cr,
            slot.glyph,
            x,
            y,
            if matches!(slot.kind, Kind::Trigger) || matches!(slot.shape, Shape::Rect { .. }) {
                11.0
            } else {
                17.0
            },
        ),
    }
}
