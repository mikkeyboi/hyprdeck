//! Generic DPI editor. Host presets are explicitly separate from onboard stages.
use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use serde_json::{Value, json};

use crate::protocol::{Control, DpiStage};

type Callback = Rc<dyn Fn()>;
type Action = Rc<dyn Fn(Value)>;
struct StageRow {
    id: String,
    row: adw::ActionRow,
    label: gtk::Entry,
    x: gtk::SpinButton,
    y: gtk::SpinButton,
}
struct Editor {
    list: gtk::ListBox,
    stages: RefCell<Vec<StageRow>>,
    linked: gtk::Switch,
    min: f64,
    max: f64,
    step: f64,
    on_edit: Callback,
    on_action: Action,
}
fn spin(value: f64, min: f64, max: f64, step: f64) -> gtk::SpinButton {
    let adjustment = gtk::Adjustment::new(value, min, max, step, step * 4.0, 0.0);
    let widget = gtk::SpinButton::new(Some(&adjustment), step, 0);
    widget.set_width_chars(6);
    widget.set_valign(gtk::Align::Center);
    widget
}
fn button(label: &str) -> gtk::Button {
    gtk::Button::builder()
        .label(label)
        .valign(gtk::Align::Center)
        .build()
}
impl Editor {
    fn append(self: &Rc<Self>, stage: &DpiStage) {
        let row = adw::ActionRow::builder().use_markup(false).build();
        let label = gtk::Entry::builder()
            .text(&stage.label)
            .width_chars(10)
            .max_length(80)
            .build();
        label.update_property(&[gtk::accessible::Property::Label("Preset name")]);
        row.add_prefix(&label);
        let x = spin(stage.x, self.min, self.max, self.step);
        let y = spin(
            if self.linked.is_active() {
                stage.x
            } else {
                stage.y
            },
            self.min,
            self.max,
            self.step,
        );
        x.update_property(&[gtk::accessible::Property::Label("Preset horizontal DPI")]);
        y.update_property(&[gtk::accessible::Property::Label("Preset vertical DPI")]);
        y.set_sensitive(!self.linked.is_active());
        row.add_suffix(&gtk::Label::new(Some("X")));
        row.add_suffix(&x);
        row.add_suffix(&gtk::Label::new(Some("Y")));
        row.add_suffix(&y);
        let apply = button("Use");
        let remove = button("Remove");
        apply.set_tooltip_text(Some(
            "Apply these values to the current DPI; does not save the preset table",
        ));
        row.add_suffix(&apply);
        row.add_suffix(&remove);
        self.list.append(&row);
        let edit = self.on_edit.clone();
        label.connect_changed(move |_| edit());
        let weak = Rc::downgrade(self);
        let target = y.downgrade();
        x.connect_value_changed(move |x| {
            if let Some(editor) = weak.upgrade() {
                if editor.linked.is_active()
                    && let Some(y) = target.upgrade()
                {
                    y.set_value(x.value());
                }
                (editor.on_edit)();
            }
        });
        let edit = self.on_edit.clone();
        y.connect_value_changed(move |_| edit());
        let action = self.on_action.clone();
        let linked = self.linked.downgrade();
        let xx = x.downgrade();
        let yy = y.downgrade();
        apply.connect_clicked(move |_| {
            if let (Some(x), Some(y), Some(linked)) = (xx.upgrade(), yy.upgrade(), linked.upgrade()) {
                x.update(); y.update();
                action(json!({"operation":"apply","x":x.value() as u64,"y":y.value() as u64,"linked":linked.is_active()}));
            }
        });
        let weak = Rc::downgrade(self);
        let id = stage.id.clone();
        remove.connect_clicked(move |_| {
            if let Some(editor) = weak.upgrade() {
                let removed = {
                    let mut stages = editor.stages.borrow_mut();
                    if stages.len() <= 1 {
                        return;
                    }
                    stages
                        .iter()
                        .position(|stage| stage.id == id)
                        .map(|index| stages.remove(index))
                };
                if let Some(stage) = removed {
                    editor.list.remove(&stage.row);
                    (editor.on_edit)();
                }
            }
        });
        self.stages.borrow_mut().push(StageRow {
            id: stage.id.clone(),
            row,
            label,
            x,
            y,
        });
    }
    fn replace(self: &Rc<Self>, values: &[DpiStage]) {
        for stage in self.stages.borrow_mut().drain(..) {
            self.list.remove(&stage.row);
        }
        for stage in values {
            self.append(stage);
        }
    }
    fn save(&self) {
        let stages: Vec<Value> = self.stages.borrow().iter().map(|stage| {
            stage.x.update(); stage.y.update();
            json!({"id":stage.id,"label":stage.label.text().as_str(),"x":stage.x.value() as u64,"y":stage.y.value() as u64})
        }).collect();
        (self.on_action)(
            json!({"operation":"save","linked":self.linked.is_active(),"stages":stages}),
        );
    }
}

pub(crate) fn build(
    control: &Control,
    row: &adw::ActionRow,
    on_edit: impl Fn() + 'static,
    on_action: impl Fn(Value) + 'static,
) -> gtk::Widget {
    let Control::Dpi {
        x,
        y,
        min,
        max,
        step,
        linked,
        stages,
        active,
        storage,
        ..
    } = control
    else {
        unreachable!("DPI editor requires DPI control")
    };
    let on_edit: Callback = Rc::new(on_edit);
    let on_action: Action = Rc::new(on_action);
    let root = gtk::Box::new(gtk::Orientation::Vertical, 16);
    root.set_margin_top(14);
    root.set_margin_bottom(14);
    root.set_margin_start(14);
    root.set_margin_end(14);
    let heading = gtk::Label::builder().label(row.title()).xalign(0.0).build();
    heading.add_css_class("heading");
    root.append(&heading);
    let live_text = match (x, y) { (Some(x), Some(y)) => format!("Current device DPI: {x:.0} × {y:.0}"), _ => "Current DPI unavailable · values below are pending manual settings, not device readback".into() };
    let live = gtk::Label::builder()
        .label(&live_text)
        .xalign(0.0)
        .wrap(true)
        .build();
    live.add_css_class("dim-label");
    root.append(&live);
    live.set_widget_name("hd-dpi-readback");
    let list = gtk::ListBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .build();
    list.add_css_class("boxed-list");
    let link_row = adw::ActionRow::builder().use_markup(false).build();
    link_row.set_title("Link horizontal and vertical DPI");
    link_row
        .set_subtitle("When linked, changing X also changes Y. Nothing is written until Apply.");
    let linked_widget = gtk::Switch::builder()
        .active(*linked)
        .valign(gtk::Align::Center)
        .build();
    link_row.add_suffix(&linked_widget);
    list.append(&link_row);
    let current = adw::ActionRow::builder().use_markup(false).build();
    current.set_title("Current sensitivity");
    let pending_x = x.unwrap_or(stages[0].x);
    let pending_y = if *linked {
        pending_x
    } else {
        y.unwrap_or(stages[0].y)
    };
    let sx = spin(pending_x, *min, *max, *step);
    let sy = spin(pending_y, *min, *max, *step);
    sx.update_property(&[gtk::accessible::Property::Label("Horizontal DPI")]);
    sy.update_property(&[gtk::accessible::Property::Label("Vertical DPI")]);
    sy.set_sensitive(!*linked);
    current.add_suffix(&gtk::Label::new(Some("X")));
    current.add_suffix(&sx);
    current.add_suffix(&gtk::Label::new(Some("Y")));
    current.add_suffix(&sy);
    let apply = button("Apply DPI");
    apply.add_css_class("suggested-action");
    current.add_suffix(&apply);
    list.append(&current);
    root.append(&list);
    let editor = Rc::new(Editor {
        list: gtk::ListBox::builder()
            .selection_mode(gtk::SelectionMode::None)
            .build(),
        stages: RefCell::new(Vec::new()),
        linked: linked_widget.clone(),
        min: *min,
        max: *max,
        step: *step,
        on_edit: on_edit.clone(),
        on_action: on_action.clone(),
    });
    editor.list.add_css_class("boxed-list");
    let link = linked_widget.downgrade();
    let yy = sy.downgrade();
    let edit = on_edit.clone();
    sx.connect_value_changed(move |x| {
        if link.upgrade().is_some_and(|link| link.is_active())
            && let Some(y) = yy.upgrade()
        {
            y.set_value(x.value());
        }
        edit();
    });
    let edit = on_edit.clone();
    sy.connect_value_changed(move |_| edit());
    let xx = sx.downgrade();
    let yy = sy.downgrade();
    let weak = Rc::downgrade(&editor);
    let edit = on_edit.clone();
    linked_widget.connect_active_notify(move |linked| {
        if let (Some(x), Some(y), Some(editor)) = (xx.upgrade(), yy.upgrade(), weak.upgrade()) {
            y.set_sensitive(!linked.is_active());
            if linked.is_active() {
                y.set_value(x.value());
            }
            for stage in editor.stages.borrow().iter() {
                stage.y.set_sensitive(!linked.is_active());
                if linked.is_active() {
                    stage.y.set_value(stage.x.value());
                }
            }
            edit();
        }
    });
    let action = on_action.clone();
    let xx = sx.downgrade();
    let yy = sy.downgrade();
    let link = linked_widget.downgrade();
    apply.connect_clicked(move |_| {
        if let (Some(x),Some(y),Some(link))=(xx.upgrade(),yy.upgrade(),link.upgrade()) {
            x.update(); y.update(); action(json!({"operation":"apply","x":x.value() as u64,"y":y.value() as u64,"linked":link.is_active()}));
        }
    });
    let presets = gtk::Label::builder()
        .label(if storage == "host" {
            "Desktop DPI presets"
        } else {
            "Onboard DPI stages"
        })
        .xalign(0.0)
        .build();
    presets.add_css_class("heading");
    root.append(&presets);
    let explanation=gtk::Label::builder().label(if storage=="host" {"Saved on this computer, not the mouse. Use a preset or Previous/Next to apply it; the mouse's DPI buttons keep their onboard behavior."} else {"Stages reported by the device. Changes require explicit Apply."}).wrap(true).xalign(0.0).build();
    explanation.add_css_class("dim-label");
    root.append(&explanation);
    let active = gtk::Label::builder()
        .label(
            active
                .as_ref()
                .and_then(|id| stages.iter().find(|stage| &stage.id == id))
                .map_or_else(
                    || "Current DPI is outside the saved presets".to_owned(),
                    |stage| format!("Active preset: {}", stage.label),
                ),
        )
        .xalign(0.0)
        .build();
    root.append(&active);
    active.set_widget_name("hd-dpi-active");
    editor.replace(stages);
    root.append(&editor.list);
    let toolbar = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    for (label, operation) in [("Previous", "previous"), ("Next", "next")] {
        let widget = button(label);
        let action = on_action.clone();
        widget.connect_clicked(move |_| action(json!({"operation":operation})));
        toolbar.append(&widget);
    }
    let add = button("Add preset");
    let weak = Rc::downgrade(&editor);
    add.connect_clicked(move |_| {
        if let Some(editor) = weak.upgrade() {
            let id = {
                let stages = editor.stages.borrow();
                if stages.len() >= 8 {
                    return;
                }
                (1..=100)
                    .map(|n| format!("stage-{n}"))
                    .find(|id| !stages.iter().any(|stage| &stage.id == id))
                    .expect("free preset id")
            };
            let value = 800.0_f64.clamp(editor.min, editor.max);
            editor.append(&DpiStage {
                id,
                label: "New preset".into(),
                x: value,
                y: value,
            });
            (editor.on_edit)();
        }
    });
    toolbar.append(&add);
    let sample = button("Sample stages");
    let weak = Rc::downgrade(&editor);
    sample.connect_clicked(move |_| {
        if let Some(editor) = weak.upgrade() {
            let samples: Vec<DpiStage> = [400.0, 800.0, 1600.0, 3200.0]
                .into_iter()
                .filter(|value| (*value >= editor.min) && (*value <= editor.max))
                .enumerate()
                .map(|(index, value)| DpiStage {
                    id: format!("stage-{}", index + 1),
                    label: format!("{value:.0} DPI"),
                    x: value,
                    y: value,
                })
                .collect();
            if !samples.is_empty() {
                editor.replace(&samples);
                (editor.on_edit)();
            }
        }
    });
    toolbar.append(&sample);
    let save = button("Save presets");
    save.add_css_class("suggested-action");
    let weak = Rc::downgrade(&editor);
    save.connect_clicked(move |_| {
        if let Some(editor) = weak.upgrade() {
            editor.save();
        }
    });
    toolbar.append(&save);
    root.append(&toolbar);
    // Widget owns the editor lifetime; callbacks back into it use weak references.
    root.connect_destroy(move |_| {
        editor.stages.borrow_mut().clear();
    });
    let widget = root.upcast();
    update(&widget, control);
    widget
}

/// Update only device readback while preserving editable values and presets.
pub(crate) fn update(widget: &gtk::Widget, control: &Control) {
    let Control::Dpi {
        x,
        y,
        active,
        stages,
        ..
    } = control
    else {
        return;
    };
    let readback = match (x, y) { (Some(x), Some(y)) => format!("Current device DPI: {x:.0} × {y:.0}"), _ => "Current DPI unavailable · values below are pending manual settings, not device readback".into() };
    let current = active
        .as_ref()
        .and_then(|id| stages.iter().find(|stage| &stage.id == id))
        .map_or_else(
            || {
                if x.is_none() || y.is_none() {
                    "Active preset unavailable without device readback".into()
                } else {
                    "Current DPI is outside the saved presets".into()
                }
            },
            |stage| format!("Active preset: {}", stage.label),
        );
    fn apply(widget: &gtk::Widget, readback: &str, current: &str) {
        if let Some(label) = widget.downcast_ref::<gtk::Label>() {
            let value = match widget.widget_name().as_str() {
                "hd-dpi-readback" => Some(readback),
                "hd-dpi-active" => Some(current),
                _ => None,
            };
            if let Some(value) = value
                && label.text().as_str() != value
            {
                label.set_text(value);
            }
        }
        let mut child = widget.first_child();
        while let Some(widget) = child {
            child = widget.next_sibling();
            apply(&widget, readback, current);
        }
    }
    apply(widget, &readback, &current);
}
