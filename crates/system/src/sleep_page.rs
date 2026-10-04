//! "Sleep & Wake" page.

use std::cell::RefCell;
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::hypr::ctl;
use hyprdeck_core::hypr::model::Combo;
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::diagnose::{self, Diagnosis};
use crate::guard::{self, WakeLog};
use crate::nvidia::{Level, NvidiaInfo};
use crate::settings::{self, RescuePolicy, Settings};
use crate::shortcut;
use crate::widgets::{self, busy, row};

struct Page {
    ctx: Ctx,
    wakes: adw::PreferencesGroup,
    wake_rows: RefCell<Vec<gtk::Widget>>,
    shortcut_entry: adw::EntryRow,
    shortcut_status: adw::ActionRow,
    shortcut_add: gtk::Button,
    shortcut_remove: gtk::Button,
    /// Diagnosis and NVIDIA groups (rebuilt on refresh).
    dynamic: gtk::Box,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = ui::page_scaffold();

    let guard_group = adw::PreferencesGroup::builder()
        .title("Resume guard")
        .description(
            "Runs a few seconds after every wake from sleep: saves diagnostics and resets the displays if they came back dark.",
        )
        .build();
    content.append(&guard_group);

    let rescue_group = adw::PreferencesGroup::builder()
        .title("Black screen after wake")
        .description(
            "Keyboard shortcuts keep working while the screen is black (and over the lock screen), so a shortcut is the \
             quickest way back without cutting power.",
        )
        .build();
    let fix = widgets::button("Reset displays");
    fix.add_css_class("suggested-action");
    let fix_row = row(
        "Fix black screen now",
        "Turns every display off and on again (re-training the link) and reloads Hyprland. The screen blinks for about 2 s.",
    );
    fix_row.add_suffix(&fix);
    rescue_group.add(&fix_row);

    let shortcut_entry = adw::EntryRow::builder()
        .title("Rescue shortcut keys")
        .text(shortcut::DEFAULT_KEYS)
        .build();
    rescue_group.add(&shortcut_entry);
    let shortcut_status = row("Rescue keyboard shortcut", "Checking…");
    let shortcut_add = widgets::button("Add shortcut");
    let shortcut_remove = widgets::icon_button("user-trash-symbolic", "Remove the shortcut");
    shortcut_remove.set_visible(false);
    shortcut_status.add_suffix(&shortcut_remove);
    shortcut_status.add_suffix(&shortcut_add);
    rescue_group.add(&shortcut_status);
    if !shortcut::on_path() {
        let warn = row(
            "hyprdeck is not on PATH",
            "The shortcut runs \"hyprdeck display rescue\"; install hyprdeck so the command can be found.",
        );
        warn.add_prefix(&gtk::Image::from_icon_name("dialog-warning-symbolic"));
        rescue_group.add(&warn);
    }
    content.append(&rescue_group);

    let wakes = adw::PreferencesGroup::builder()
        .title("Recent wakes")
        .description(glib::markup_escape_text(&format!(
            "Diagnostics saved by the resume guard in {}",
            widgets::tilde(&guard::dir())
        )))
        .build();
    let refresh = widgets::icon_button("view-refresh-symbolic", "Refresh");
    wakes.set_header_suffix(Some(&refresh));
    content.append(&wakes);

    let dynamic = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.append(&dynamic);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        wakes,
        wake_rows: RefCell::default(),
        shortcut_entry,
        shortcut_status,
        shortcut_add,
        shortcut_remove,
        dynamic,
    });

    build_guard_settings(ctx, &guard_group);

    let p = page.clone();
    fix.connect_clicked(move |b| {
        let (b, ctx) = (b.clone(), p.ctx.clone());
        p.ctx.spawn(async move {
            busy(&b, "Reset displays", true);
            let r = rt::blocking(ctl::rescue_displays).await;
            busy(&b, "Reset displays", false);
            match r {
                Ok(()) => ctx.toast("Displays reset"),
                Err(e) => ctx.error("Resetting displays", &e),
            }
        });
    });

    let p = page.clone();
    page.shortcut_add
        .connect_clicked(move |_| install_shortcut(&p));
    let p = page.clone();
    page.shortcut_remove.connect_clicked(move |_| {
        let p = p.clone();
        p.ctx.clone().spawn(async move {
            match rt::blocking(shortcut::remove).await {
                Ok(errors) => {
                    report_apply(&p.ctx, "Shortcut removed", &errors);
                    refresh_shortcut(&p).await;
                }
                Err(e) => p.ctx.error("Removing the shortcut", &e),
            }
        });
    });

    let p = page.clone();
    refresh.connect_clicked(move |_| refresh_all(&p));
    let p = page.clone();
    ui::on_shown(&scroller, move || refresh_all(&p));
    scroller.upcast()
}

fn refresh_all(p: &Rc<Page>) {
    let p = p.clone();
    p.ctx.clone().spawn(async move {
        refresh_shortcut(&p).await;
        let logs = rt::blocking(guard::recent).await;
        fill_wakes(&p, &logs);
        let d = rt::blocking(diagnose::run).await;
        fill_dynamic(&p, &d);
    });
}

fn build_guard_settings(ctx: &Ctx, group: &adw::PreferencesGroup) {
    let enabled = adw::SwitchRow::builder()
        .title("Check after every wake")
        .subtitle("Save monitor state, kernel and session logs a few seconds after waking")
        .build();
    let labels: Vec<&str> = RescuePolicy::ALL.iter().map(|p| p.label()).collect();
    let policy = adw::ComboRow::builder()
        .title("Reset displays")
        .subtitle("Problems: HDMI link-training failure, a monitor left powered off, or a monitor missing")
        .model(&gtk::StringList::new(&labels))
        .build();
    let delay = adw::SpinRow::builder()
        .title("Check delay")
        .subtitle("Seconds to wait after wake before checking the displays")
        .adjustment(&gtk::Adjustment::new(3.0, 1.0, 60.0, 1.0, 5.0, 0.0))
        .build();
    let notify = adw::SwitchRow::builder()
        .title("Notify")
        .subtitle("Show a notification when a problem is found or the displays are reset")
        .build();
    for r in [
        enabled.upcast_ref::<gtk::Widget>(),
        policy.upcast_ref(),
        delay.upcast_ref(),
        notify.upcast_ref(),
    ] {
        r.set_sensitive(false);
        group.add(r);
    }

    let ctx = ctx.clone();
    ctx.clone().spawn(async move {
        let s = match rt::blocking(settings::load).await {
            Ok(s) => s,
            Err(e) => {
                ctx.error("Loading sleep settings", &e);
                return;
            }
        };
        enabled.set_active(s.resume.enabled);
        policy.set_selected(
            RescuePolicy::ALL
                .iter()
                .position(|p| *p == s.resume.rescue)
                .unwrap_or(1) as u32,
        );
        delay.set_value(f64::from(s.resume.delay_secs));
        notify.set_active(s.resume.notify);
        for r in [
            enabled.upcast_ref::<gtk::Widget>(),
            policy.upcast_ref(),
            delay.upcast_ref(),
            notify.upcast_ref(),
        ] {
            r.set_sensitive(true);
        }
        let dependents = [
            policy.clone().upcast::<gtk::Widget>(),
            delay.clone().upcast(),
            notify.clone().upcast(),
        ];
        for d in &dependents {
            d.set_sensitive(s.resume.enabled);
        }

        let save = {
            let ctx = ctx.clone();
            move |f: Box<dyn FnOnce(&mut Settings) + Send>| {
                let ctx = ctx.clone();
                ctx.clone().spawn(async move {
                    match rt::blocking(move || settings::update(f)).await {
                        Ok(_) => ctx.toast("Saved"),
                        Err(e) => ctx.error("Saving sleep settings", &e),
                    }
                });
            }
        };
        let s1 = save.clone();
        enabled.connect_active_notify(move |r| {
            let on = r.is_active();
            for d in &dependents {
                d.set_sensitive(on);
            }
            s1(Box::new(move |s| s.resume.enabled = on));
        });
        let s2 = save.clone();
        policy.connect_selected_notify(move |r| {
            let p = RescuePolicy::ALL[(r.selected() as usize).min(2)];
            s2(Box::new(move |s| s.resume.rescue = p));
        });
        let s3 = save.clone();
        widgets::on_spin_settled(&delay, move |v| {
            let secs = v.round() as u32;
            s3(Box::new(move |s| s.resume.delay_secs = secs));
        });
        notify.connect_active_notify(move |r| {
            let on = r.is_active();
            save(Box::new(move |s| s.resume.notify = on));
        });
    });
}

async fn refresh_shortcut(p: &Page) {
    match rt::blocking(shortcut::current).await {
        Ok(Some(keys)) => {
            p.shortcut_entry.set_text(&keys);
            p.shortcut_status.set_subtitle(&format!(
                "{keys} runs \"{}\" — works on a black screen and on the lock screen",
                shortcut::RESCUE_CMD
            ));
            p.shortcut_add.set_label("Update");
            p.shortcut_remove.set_visible(true);
        }
        Ok(None) => {
            p.shortcut_status
                .set_subtitle("Not set. Adds a hyprdeck keybind that resets the displays.");
            p.shortcut_add.set_label("Add shortcut");
            p.shortcut_remove.set_visible(false);
        }
        Err(e) => p.ctx.error("Reading hyprdeck keybinds", &e),
    }
}

fn install_shortcut(p: &Rc<Page>) {
    let keys = Combo::parse(&p.shortcut_entry.text()).0;
    if keys.is_empty() || !keys.contains(char::is_alphanumeric) {
        p.ctx
            .toast("Enter a key combination such as SUPER + CTRL + SHIFT + R");
        return;
    }
    let p = p.clone();
    p.ctx.clone().spawn(async move {
        let k = keys.clone();
        let conflicts = rt::blocking(move || shortcut::conflicts(&k)).await;
        if !conflicts.is_empty()
            && !p
                .ctx
                .confirm(
                    &format!("Replace {keys}?"),
                    &format!(
                        "{keys} is already bound to:\n{}\n\nThe rescue shortcut will replace it.",
                        conflicts.join("\n")
                    ),
                    "Replace",
                    true,
                )
                .await
        {
            return;
        }
        let label = p
            .shortcut_add
            .label()
            .map(|l| l.to_string())
            .unwrap_or_default();
        busy(&p.shortcut_add, &label, true);
        let k = keys.clone();
        let r = rt::blocking(move || shortcut::install(&k)).await;
        busy(&p.shortcut_add, &label, false);
        match r {
            Ok(errors) => report_apply(&p.ctx, &format!("{keys} now resets the displays"), &errors),
            Err(e) => p.ctx.error("Adding the shortcut", &e),
        }
        refresh_shortcut(&p).await;
    });
}

fn report_apply(ctx: &Ctx, ok: &str, errors: &[String]) {
    if errors.is_empty() {
        ctx.toast(ok);
    } else {
        ctx.error(
            ok,
            &anyhow::anyhow!("Hyprland reports config errors: {}", errors.join("; ")),
        );
    }
}

fn fill_wakes(p: &Rc<Page>, logs: &[WakeLog]) {
    for r in p.wake_rows.borrow_mut().drain(..) {
        p.wakes.remove(&r);
    }
    let mut rows = Vec::new();
    if logs.is_empty() {
        let r = row(
            "No wakes recorded yet",
            "The resume guard saves a report after the next wake from sleep.",
        );
        rows.push(r.upcast::<gtk::Widget>());
    }
    for log in logs {
        let title = format!(
            "{}{}",
            log.woke,
            if log.simulated { " (simulated)" } else { "" }
        );
        let subtitle = if log.problems.is_empty() {
            format!("No display problems · rescue: {}", log.rescue)
        } else {
            format!("{} · rescue: {}", log.problems.join(" · "), log.rescue)
        };
        let r = row(&title, &subtitle);
        let icon = if log.problems.is_empty() {
            "object-select-symbolic"
        } else {
            "dialog-warning-symbolic"
        };
        r.add_prefix(&gtk::Image::from_icon_name(icon));
        let view = widgets::button("View");
        let (ctx, path) = (p.ctx.clone(), log.path.clone());
        view.connect_clicked(move |_| {
            let (ctx, path) = (ctx.clone(), path.clone());
            ctx.clone().spawn(async move {
                let p2 = path.clone();
                match rt::blocking(move || std::fs::read_to_string(&p2)).await {
                    Ok(text) => {
                        let name = path
                            .file_name()
                            .map(|n| n.to_string_lossy().into_owned())
                            .unwrap_or_default();
                        widgets::text_dialog(&ctx, &name, &text, Some(path));
                    }
                    Err(e) => ctx.error("Reading the report", &e.into()),
                }
            });
        });
        r.add_suffix(&view);
        rows.push(r.upcast());
    }
    for r in &rows {
        p.wakes.add(r);
    }
    *p.wake_rows.borrow_mut() = rows;
}

fn fill_dynamic(p: &Rc<Page>, d: &Diagnosis) {
    ui::clear(&p.dynamic);
    p.dynamic.append(&diagnosis_group(d));
    if let Some(n) = &d.nvidia {
        p.dynamic.append(&nvidia_group(n, d.mem_sleep.as_deref()));
    }
}

fn count_row(title: &str, subtitle: &str, value: String, bad: bool) -> adw::ActionRow {
    let r = row(title, subtitle);
    let label = gtk::Label::new(Some(&value));
    label.add_css_class("title-3");
    if bad {
        label.add_css_class("warning");
    }
    r.add_suffix(&label);
    r
}

fn date(us: i64) -> String {
    glib::DateTime::from_unix_local(us / 1_000_000)
        .and_then(|d| d.format("%a %b %e, %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

fn diagnosis_group(d: &Diagnosis) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder().title("Diagnosis").build();
    match &d.evidence {
        Ok(ev) => {
            g.set_description(Some(&format!(
                "From the system journal: last {} boot(s), since {}",
                ev.boots,
                ev.since_us.map(date).unwrap_or_default()
            )));
            let wakes = ev.wakes.len();
            g.add(&count_row(
                "Wakes from sleep",
                "\"PM: suspend exit\" in the kernel log",
                wakes.to_string(),
                false,
            ));
            g.add(&count_row(
                "HDMI link-training failures",
                &format!(
                    "\"HDMI FRL link training failed\" — {} within {} s of a wake. Leaves the screen black.",
                    ev.frl_after_wake(),
                    crate::journal::FRL_WINDOW_SECS
                ),
                ev.frl_total.to_string(),
                ev.frl_total > 0,
            ));
            g.add(&count_row(
                "Power button right after a wake",
                &format!(
                    "Pressed within {} minutes of waking — likely a black screen",
                    crate::journal::POWER_WINDOW_SECS / 60
                ),
                ev.power_after_wake().to_string(),
                ev.power_after_wake() > 0,
            ));
            g.add(&count_row(
                "USB controller re-initialised",
                "\"xHC error in resume\" — every USB device reconnects after wake",
                format!(
                    "{} / {}",
                    ev.wakes.iter().filter(|w| w.xhci_reinit).count(),
                    wakes
                ),
                false,
            ));
            if !ev.wakes.is_empty() {
                let exp = adw::ExpanderRow::builder()
                    .title("Wake history")
                    .subtitle("Newest first")
                    .build();
                for w in ev.wakes.iter().rev() {
                    exp.add_row(&row(&date(w.at_us), &diagnose::wake_flags(w)));
                }
                g.add(&exp);
            }
        }
        Err(e) => g.add(&row("Journal unavailable", e)),
    }
    if let Some(mode) = &d.mem_sleep {
        let r = row("Sleep mode", diagnose::mem_sleep_explain(mode));
        r.add_suffix(&gtk::Label::new(Some(mode)));
        g.add(&r);
    }
    for rec in &d.recommendations {
        let r = row(rec, "");
        r.add_css_class("property");
        r.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        g.add(&r);
    }
    if let Some(o) = d.olh_slow() {
        let r = row(
            "Keyboard may be late after wake — see Tweaks",
            &format!(
                "OpenLinkHub devices respond only ≈ {:.1} s after every wake (resume delay + restart delay)",
                o.wake_delay()
            ),
        );
        r.add_prefix(&gtk::Image::from_icon_name("dialog-information-symbolic"));
        let open = widgets::button("Open Tweaks");
        open.connect_clicked(|_| events::send(AppEvent::ShowPage("tweaks".into())));
        r.add_suffix(&open);
        g.add(&r);
    }
    if d.recommendations.is_empty() && d.olh_slow().is_none() {
        g.add(&row(
            "No problems found",
            "Nothing in the journal points at a sleep/wake problem.",
        ));
    }
    g
}

fn nvidia_group(n: &NvidiaInfo, mem_sleep: Option<&str>) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title("NVIDIA power management")
        .description("/proc/driver/nvidia/params and the nvidia-* systemd services")
        .build();
    if let Some(v) = &n.version {
        g.add(&row("Driver", v));
    }
    let param_rows = [
        (
            "UseKernelSuspendNotifiers",
            "1 = the driver saves video memory itself on suspend",
        ),
        (
            "PreserveVideoMemoryAllocations",
            "Non-zero = video memory is kept across sleep",
        ),
        (
            "TemporaryFilePath",
            "Where video memory is saved during sleep",
        ),
    ];
    for (key, explain) in param_rows {
        if let Some(v) = n.param(key) {
            let r = row(key, explain);
            r.add_suffix(&gtk::Label::new(Some(v)));
            g.add(&r);
        }
    }
    if !n.services.is_empty() {
        let exp = adw::ExpanderRow::builder().use_markup(false).build();
        exp.set_title("Suspend services");
        exp.set_subtitle(
            &n.services
                .iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
        );
        for (unit, state) in &n.services {
            let r = row(unit, "");
            r.add_suffix(&gtk::Label::new(Some(state)));
            exp.add_row(&r);
        }
        g.add(&exp);
    }
    for (level, text) in n.interpret(mem_sleep) {
        let r = row(&text, "");
        r.add_prefix(&gtk::Image::from_icon_name(match level {
            Level::Ok => "object-select-symbolic",
            Level::Info => "dialog-information-symbolic",
            Level::Warning => "dialog-warning-symbolic",
        }));
        g.add(&r);
    }
    g
}
