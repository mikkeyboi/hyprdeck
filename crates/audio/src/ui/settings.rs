//! Settings tab: volume ceiling, sound server info and self-test.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::rt;
use hyprdeck_core::ui::{Ctx, page_scaffold};

use super::{act_then, follow};
use crate::{engine, pw};

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = page_scaffold();

    let behaviour = adw::PreferencesGroup::builder().title("Behaviour").build();
    let max = adw::SpinRow::with_range(100.0, 150.0, 5.0);
    max.set_title("Maximum volume");
    max.set_subtitle("Ceiling for every slider, preset and tray action (percent)");
    behaviour.add(&max);
    let restore = adw::ActionRow::builder()
        .title("Restore at login")
        .subtitle("When simultaneous output was on, it is turned back on at login and after PipeWire restarts, as soon as two of its outputs are connected")
        .build();
    behaviour.add(&restore);
    content.append(&behaviour);

    let server = adw::PreferencesGroup::builder()
        .title("Sound server")
        .build();
    let info = adw::ActionRow::builder()
        .title("Server")
        .subtitle("Checking…")
        .build();
    info.set_subtitle_lines(2);
    let file = adw::ActionRow::builder()
        .title("Settings file")
        .subtitle(glib::markup_escape_text(
            &hyprdeck_core::store::config_dir()
                .join("audio.toml")
                .display()
                .to_string(),
        ))
        .subtitle_selectable(true)
        .build();
    let test = adw::ButtonRow::builder()
        .title("Run PipeWire self-test")
        .start_icon_name("system-run-symbolic")
        .build();
    server.add(&info);
    server.add(&file);
    server.add(&test);
    content.append(&server);

    let updating = Rc::new(Cell::new(false));
    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let (u, c) = (updating.clone(), ctx.clone());
    max.connect_value_notify(move |row| {
        if u.get() {
            return;
        }
        if let Some(id) = pending.take() {
            id.remove();
        }
        let (value, c, p2) = (row.value() as u32, c.clone(), pending.clone());
        let id = glib::timeout_add_local_once(Duration::from_millis(400), move || {
            p2.take();
            act_then(
                &c,
                "Could not save settings",
                async move {
                    engine::update_settings(move |cfg| cfg.max_volume = value)
                        .await
                        .map(|()| format!("Maximum volume: {value}%"))
                },
                |_| {},
            );
        });
        pending.replace(Some(id));
    });
    let c = ctx.clone();
    test.connect_activated(move |row| {
        row.set_sensitive(false);
        row.set_title("Running self-test…");
        let row = row.clone();
        act_then(&c, "Self-test failed", engine::self_test(), move |_| {
            row.set_sensitive(true);
            row.set_title("Run PipeWire self-test");
        });
    });

    let (u, m) = (updating, max);
    follow(&scroller, move |snap| {
        let Ok(st) = snap else { return };
        u.set(true);
        m.set_value(st.config.max_volume as f64);
        u.set(false);
    });

    let info2 = info.clone();
    hyprdeck_core::ui::on_shown(&scroller, move || {
        let info = info2.clone();
        glib::spawn_future_local(async move {
            let text = match rt::run(pw::server_info()).await {
                Ok(out) => {
                    let field = |k: &str| {
                        out.lines()
                            .find_map(|l| l.strip_prefix(k))
                            .map(str::trim)
                            .unwrap_or("")
                            .to_owned()
                    };
                    field("Server Name:")
                }
                Err(e) => format!("Not reachable: {e:#}"),
            };
            info.set_subtitle(&glib::markup_escape_text(&text));
        });
    });
    scroller.upcast()
}
