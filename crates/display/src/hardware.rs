//! "Monitor hardware" groups: DDC/CI brightness, contrast, input source and
//! power, shown under each output whose connector ddcutil can reach.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use adw::prelude::*;
use anyhow::Result;
use gtk::glib;
use hyprdeck_core::rt;

use crate::ddc::{self, Capabilities, DdcDisplay, Vcp};
use crate::page::Page;

/// Delay after the last slider movement before writing to the monitor.
const DEBOUNCE: Duration = Duration::from_millis(350);

struct Probe {
    output: String,
    display: DdcDisplay,
    caps: Capabilities,
    brightness: Option<(u16, u16)>,
    contrast: Option<(u16, u16)>,
    input: Option<u8>,
}

fn continuous(bus: u32, code: u8) -> Option<(u16, u16)> {
    match ddc::get(bus, code) {
        Ok(Vcp::Continuous { current, max }) if max > 0 => Some((current, max)),
        _ => None,
    }
}

/// Probe reachable outputs; an empty list plus a hint when DDC/CI is unusable.
fn probe(outputs: Vec<String>) -> Result<(Vec<Probe>, Option<String>)> {
    let access = ddc::access();
    if access != ddc::Access::Ready {
        return Ok((Vec::new(), access.hint()));
    }
    let displays = ddc::detect()?;
    let mut out = Vec::new();
    for output in outputs {
        let Some(display) = ddc::for_output(&displays, &output).cloned() else {
            continue;
        };
        let bus = display.bus;
        let caps = match ddc::capabilities(bus) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("ddcutil capabilities on bus {bus}: {e:#}");
                continue;
            }
        };
        let brightness = caps
            .has(ddc::BRIGHTNESS)
            .then(|| continuous(bus, ddc::BRIGHTNESS))
            .flatten();
        let contrast = caps
            .has(ddc::CONTRAST)
            .then(|| continuous(bus, ddc::CONTRAST))
            .flatten();
        let input = match caps.values(ddc::INPUT_SOURCE).is_empty() {
            true => None,
            false => match ddc::get(bus, ddc::INPUT_SOURCE) {
                Ok(Vcp::Value(v)) => Some(v),
                _ => None,
            },
        };
        out.push(Probe {
            output,
            display,
            caps,
            brightness,
            contrast,
            input,
        });
    }
    Ok((out, None))
}

/// Probe DDC/CI asynchronously, cache one group per reachable output and
/// place them in the outputs' slots.
pub(crate) fn load(page: &Rc<Page>) {
    let outputs: Vec<String> = page.ddc_slots().into_iter().map(|s| s.0).collect();
    let page = page.clone();
    glib::spawn_future_local(async move {
        let (probes, hint) = rt::blocking(move || probe(outputs))
            .await
            .unwrap_or_else(|e| {
                tracing::info!("DDC/CI unavailable: {e:#}");
                (Vec::new(), None)
            });
        page.ddc_hint.set_label(hint.as_deref().unwrap_or_default());
        page.ddc_hint.set_visible(hint.is_some());
        let groups: Vec<(String, adw::PreferencesGroup)> = probes
            .into_iter()
            .map(|p| (p.output.clone(), group(&page, p)))
            .collect();
        // Slots may have been rebuilt while probing; fill the current ones.
        for (name, slot) in page.ddc_slots() {
            hyprdeck_core::ui::clear(&slot);
            if let Some((_, g)) = groups.iter().find(|(o, _)| *o == name) {
                slot.append(g);
            }
        }
        *page.ddc_groups.borrow_mut() = groups;
    });
}

fn group(page: &Rc<Page>, p: Probe) -> adw::PreferencesGroup {
    let bus = p.display.bus;
    let model = p
        .display
        .monitor
        .split(':')
        .nth(1)
        .unwrap_or(&p.display.monitor)
        .to_owned();
    let group = adw::PreferencesGroup::builder()
        .title(glib::markup_escape_text(&format!(
            "Monitor Hardware — {model}"
        )))
        .description(format!(
            "DDC/CI on /dev/i2c-{bus}; changes the monitor's own settings"
        ))
        .build();
    if let Some((cur, max)) = p.brightness {
        group.add(&slider(
            page,
            "Brightness",
            "Backlight level",
            bus,
            ddc::BRIGHTNESS,
            cur,
            max,
        ));
    }
    if let Some((cur, max)) = p.contrast {
        group.add(&slider(
            page,
            "Contrast",
            "Monitor contrast",
            bus,
            ddc::CONTRAST,
            cur,
            max,
        ));
    }
    let inputs = p.caps.values(ddc::INPUT_SOURCE).to_vec();
    if let Some(current) = p.input
        && !inputs.is_empty()
    {
        group.add(&input_row(page, bus, inputs, current));
    }
    let power = p.caps.values(ddc::POWER_MODE);
    let off = power
        .iter()
        .find(|v| v.0 == 0x05)
        .or_else(|| power.iter().find(|v| v.0 == 0x04))
        .map(|v| v.0);
    if let Some(off) = off {
        group.add(&power_row(page, bus, off));
    }
    group
}

fn slider(
    page: &Rc<Page>,
    title: &str,
    subtitle: &str,
    bus: u32,
    code: u8,
    current: u16,
    max: u16,
) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title(title)
        .subtitle(subtitle)
        .build();
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, f64::from(max), 1.0);
    scale.set_value(f64::from(current));
    scale.set_digits(0);
    scale.set_draw_value(true);
    scale.set_value_pos(gtk::PositionType::Right);
    scale.set_hexpand(true);
    scale.set_width_request(280);
    scale.set_valign(gtk::Align::Center);
    row.add_suffix(&scale);

    let pending: Rc<RefCell<Option<glib::SourceId>>> = Rc::default();
    let latest = Arc::new(AtomicU64::new(0));
    let ctx = page.ctx.clone();
    scale.connect_value_changed(move |s| {
        if let Some(id) = pending.borrow_mut().take() {
            id.remove();
        }
        let value = s.value().round() as u16;
        let (ctx, latest, slot) = (ctx.clone(), latest.clone(), pending.clone());
        let id = glib::timeout_add_local_once(DEBOUNCE, move || {
            slot.borrow_mut().take();
            let ticket = latest.fetch_add(1, Ordering::SeqCst) + 1;
            glib::spawn_future_local(async move {
                // Writes queue on the bus lock; skip superseded values.
                let res = rt::blocking(move || {
                    if latest.load(Ordering::SeqCst) == ticket {
                        ddc::set(bus, code, value)
                    } else {
                        Ok(())
                    }
                })
                .await;
                if let Err(e) = res {
                    ctx.error("DDC/CI write failed", &e);
                }
            });
        });
        *pending.borrow_mut() = Some(id);
    });
    row
}

fn input_row(page: &Rc<Page>, bus: u32, inputs: Vec<(u8, String)>, current: u8) -> adw::ComboRow {
    let labels: Vec<&str> = inputs.iter().map(|i| i.1.as_str()).collect();
    let selected = inputs.iter().position(|i| i.0 == current);
    let list = gtk::StringList::new(&labels);
    if selected.is_none() {
        list.append(&format!("Unknown (0x{current:02x})"));
    }
    let row = adw::ComboRow::builder()
        .title("Input source")
        .subtitle("Switching away shows another device's signal on this monitor")
        .model(&list)
        .selected(selected.unwrap_or(inputs.len()) as u32)
        .build();
    let shown = Rc::new(Cell::new(row.selected()));
    let guard = Rc::new(Cell::new(false));
    let ctx = page.ctx.clone();
    row.connect_selected_notify(move |row| {
        if guard.get() {
            return;
        }
        let sel = row.selected();
        let Some((value, name)) = inputs.get(sel as usize).cloned() else { return };
        let (ctx, shown, guard, row) = (ctx.clone(), shown.clone(), guard.clone(), row.clone());
        glib::spawn_future_local(async move {
            let body = format!(
                "The monitor will show {name}. If nothing is connected there, switch back with the monitor's own buttons."
            );
            let ok = ctx.confirm("Switch input source?", &body, "Switch", false).await
                && match rt::blocking(move || ddc::set(bus, ddc::INPUT_SOURCE, u16::from(value))).await {
                    Ok(()) => true,
                    Err(e) => {
                        ctx.error("Switching input failed", &e);
                        false
                    }
                };
            if ok {
                shown.set(sel);
                ctx.toast(format!("Input switched to {name}"));
            } else {
                guard.set(true);
                row.set_selected(shown.get());
                guard.set(false);
            }
        });
    });
    row
}

fn power_row(page: &Rc<Page>, bus: u32, off: u8) -> adw::ActionRow {
    let row = adw::ActionRow::builder()
        .title("Power")
        .subtitle("Put the monitor in standby through DDC/CI")
        .build();
    let button = gtk::Button::builder()
        .label("Turn Off")
        .valign(gtk::Align::Center)
        .css_classes(["destructive-action"])
        .build();
    row.add_suffix(&button);
    let ctx = page.ctx.clone();
    button.connect_clicked(move |_| {
        let ctx = ctx.clone();
        glib::spawn_future_local(async move {
            let body =
                "The screen goes dark. Most monitors only wake from this with their power button.";
            if !ctx
                .confirm("Turn the monitor off?", body, "Turn Off", true)
                .await
            {
                return;
            }
            if let Err(e) =
                rt::blocking(move || ddc::set(bus, ddc::POWER_MODE, u16::from(off))).await
            {
                ctx.error("Turning the monitor off failed", &e);
            }
        });
    });
    row
}
