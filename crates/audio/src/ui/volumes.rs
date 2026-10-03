//! Volumes tab: master level for every output and a slider per sink (including combined sinks).

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::ui::{Ctx, page_scaffold};

use super::{VolumeControl, act, badge, follow};
use crate::engine::{self, Status};

const PRESETS: [u32; 5] = [0, 25, 50, 75, 100];

struct Row {
    name: String,
    row: adw::ActionRow,
    vol: VolumeControl,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = page_scaffold();

    let all = adw::PreferencesGroup::builder()
        .title("All outputs")
        .description("Applies to every real output at once; combined outputs follow their members.")
        .build();
    let master = adw::SpinRow::with_range(0.0, 150.0, 5.0);
    master.set_title("Master level");
    master.set_subtitle("Percent, used by “Apply to all outputs”");
    master.set_value(100.0);
    all.add(&master);
    let presets_row = adw::ActionRow::builder()
        .title("Presets")
        .subtitle("Set every output immediately")
        .build();
    let presets = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    presets.set_valign(gtk::Align::Center);
    for p in PRESETS {
        let b = gtk::Button::builder()
            .label(format!("{p}%"))
            .css_classes(["pill"])
            .build();
        let (ctx, master) = (ctx.clone(), master.clone());
        b.connect_clicked(move |_| {
            master.set_value(p as f64);
            act(&ctx, "Could not set volume", engine::apply_to_all(p as f64));
        });
        presets.append(&b);
    }
    presets_row.add_suffix(&presets);
    all.add(&presets_row);
    let apply = adw::ButtonRow::builder()
        .title("Apply to all outputs")
        .start_icon_name("object-select-symbolic")
        .build();
    apply.add_css_class("suggested-action");
    let m = master.clone();
    let c = ctx.clone();
    apply.connect_activated(move |_| {
        act(&c, "Could not set volume", engine::apply_to_all(m.value()))
    });
    let mute = adw::ButtonRow::builder()
        .title("Mute all")
        .start_icon_name("audio-volume-muted-symbolic")
        .build();
    let c = ctx.clone();
    mute.connect_activated(move |_| act(&c, "Could not mute", engine::mute_all(true)));
    let unmute = adw::ButtonRow::builder()
        .title("Unmute all")
        .start_icon_name("audio-volume-high-symbolic")
        .build();
    let c = ctx.clone();
    unmute.connect_activated(move |_| act(&c, "Could not unmute", engine::mute_all(false)));
    all.add(&apply);
    all.add(&mute);
    all.add(&unmute);
    content.append(&all);

    let group = adw::PreferencesGroup::builder().title("Outputs").build();
    let empty = adw::ActionRow::builder()
        .title("No audio outputs detected")
        .subtitle("Outputs appear here as soon as PipeWire reports them.")
        .build();
    group.add(&empty);
    content.append(&group);

    let rows: Rc<RefCell<Vec<Row>>> = Rc::default();
    let ctx = ctx.clone();
    follow(&scroller, move |snap| {
        let Ok(st) = snap else { return };
        master.set_range(0.0, st.config.max_volume as f64);
        update(&ctx, &group, &empty, &rows, st);
    });
    scroller.upcast()
}

fn update(
    ctx: &Ctx,
    group: &adw::PreferencesGroup,
    empty: &adw::ActionRow,
    rows: &RefCell<Vec<Row>>,
    st: &Status,
) {
    let same = {
        let r = rows.borrow();
        r.len() == st.sinks.len() && r.iter().zip(&st.sinks).all(|(a, b)| a.name == b.name)
    };
    if !same {
        for r in rows.borrow_mut().drain(..) {
            group.remove(&r.row);
        }
        let new: Vec<Row> = st
            .sinks
            .iter()
            .map(|s| {
                let row = adw::ActionRow::builder().tooltip_text(&s.name).build();
                row.set_title_lines(1);
                row.add_prefix(&gtk::Image::from_icon_name(s.conn.icon()));
                row.add_suffix(&badge(s));
                let vol = VolumeControl::new(ctx, 260);
                row.add_suffix(&vol.root);
                group.add(&row);
                Row {
                    name: s.name.clone(),
                    row,
                    vol,
                }
            })
            .collect();
        rows.replace(new);
    }
    empty.set_visible(st.sinks.is_empty());
    for (r, s) in rows.borrow().iter().zip(&st.sinks) {
        r.row.set_title(&glib::markup_escape_text(&s.label));
        r.row
            .set_subtitle(if s.is_default { "Default output" } else { "" });
        r.vol.update(s, st.config.max_volume);
    }
}
