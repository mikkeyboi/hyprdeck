//! "Tweaks" page: game mode, config health, OpenLinkHub (when installed),
//! Hyprland reload and log.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::hypr::{ctl, model};
use hyprdeck_core::rt;
use hyprdeck_core::ui::{self, Ctx};

use crate::gamemode;
use crate::health::{self, BindFate};
use crate::openlinkhub::{self, OlhInfo, Scope};
use crate::widgets::{self, busy, row};

struct Page {
    ctx: Ctx,
    gamemode: adw::SwitchRow,
    /// Set while the switch is updated programmatically.
    syncing: Cell<bool>,
    /// Config health groups (rebuilt on refresh).
    health: gtk::Box,
    /// OpenLinkHub group, empty when it is not installed (rebuilt on refresh).
    olh: gtk::Box,
    log_view: gtk::TextView,
    log_scroll: gtk::ScrolledWindow,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let (scroller, content) = ui::page_scaffold();

    let gm_group = adw::PreferencesGroup::builder().title("Game mode").build();
    let gamemode = adw::SwitchRow::builder()
        .title("Game mode")
        .subtitle(
            "Turns off animations, blur, shadows, rounded corners and gaps, and allows tearing for windows that ask for \
             it. Not saved: ends when turned off or when the config reloads.",
        )
        .sensitive(false)
        .build();
    gm_group.add(&gamemode);
    content.append(&gm_group);

    let health = gtk::Box::new(gtk::Orientation::Vertical, 24);
    content.append(&health);

    let olh = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&olh);

    let hypr = adw::PreferencesGroup::builder().title("Hyprland").build();
    let reload = widgets::button("Reload");
    let reload_row = row(
        "Reload Hyprland config",
        "Re-reads hyprland.lua and everything it requires",
    );
    reload_row.add_suffix(&reload);
    hypr.add(&reload_row);
    let open = widgets::button("Open");
    let open_row = row("Open config folder", &widgets::tilde(&model::hypr_dir()));
    open_row.add_suffix(&open);
    hypr.add(&open_row);
    content.append(&hypr);

    let log_group = adw::PreferencesGroup::builder()
        .title("Hyprland log")
        .description("hyprctl rollinglog — the newest compositor messages")
        .build();
    let follow = gtk::ToggleButton::builder()
        .icon_name("media-playback-start-symbolic")
        .tooltip_text("Follow: refresh every 2 seconds")
        .valign(gtk::Align::Center)
        .css_classes(["flat"])
        .build();
    let log_refresh = widgets::icon_button("view-refresh-symbolic", "Refresh");
    let log_copy = widgets::icon_button("edit-copy-symbolic", "Copy");
    let suffix = gtk::Box::new(gtk::Orientation::Horizontal, 4);
    suffix.append(&follow);
    suffix.append(&log_refresh);
    suffix.append(&log_copy);
    log_group.set_header_suffix(Some(&suffix));
    let log_view = widgets::mono_view();
    let log_scroll = gtk::ScrolledWindow::builder()
        .child(&log_view)
        .height_request(420)
        .build();
    log_group.add(&gtk::Frame::builder().child(&log_scroll).build());
    content.append(&log_group);

    let page = Rc::new(Page {
        ctx: ctx.clone(),
        gamemode,
        syncing: Cell::new(false),
        health,
        olh,
        log_view,
        log_scroll,
    });

    let p = page.clone();
    page.gamemode.connect_active_notify(move |sw| {
        if p.syncing.get() {
            return;
        }
        let on = sw.is_active();
        sw.set_sensitive(false);
        let p = p.clone();
        p.ctx.clone().spawn(async move {
            match rt::blocking(move || gamemode::set(on)).await {
                Ok(()) => p.ctx.toast(if on {
                    "Game mode on"
                } else {
                    "Game mode off — config reloaded"
                }),
                Err(e) => p.ctx.error("Game mode", &e),
            }
            p.sync_gamemode().await;
        });
    });

    let c = ctx.clone();
    reload.connect_clicked(move |b| {
        let (ctx, b) = (c.clone(), b.clone());
        c.spawn(async move {
            busy(&b, "Reload", true);
            let r = rt::blocking(|| {
                ctl::reload(false)?;
                ctl::config_errors()
            })
            .await;
            busy(&b, "Reload", false);
            match r {
                Ok(errs) if errs.is_empty() => ctx.toast("Hyprland config reloaded"),
                Ok(errs) => ctx.error(
                    "Reloaded with config errors",
                    &anyhow::anyhow!("{}", errs.join("; ")),
                ),
                Err(e) => ctx.error("Reloading Hyprland", &e),
            }
        });
    });
    let c = ctx.clone();
    open.connect_clicked(move |_| widgets::open_path(&c, model::hypr_dir()));

    let p = page.clone();
    log_refresh.connect_clicked(move |_| p.load_log());
    let p = page.clone();
    log_copy.connect_clicked(move |_| {
        let b = p.log_view.buffer();
        widgets::copy_text(&p.ctx, &b.text(&b.start_iter(), &b.end_iter(), false));
    });
    let p = page.clone();
    follow.connect_toggled(move |t| {
        if !t.is_active() {
            return;
        }
        p.load_log();
        let (p, t) = (p.clone(), t.clone());
        glib::timeout_add_local(Duration::from_secs(2), move || {
            if !t.is_active() {
                return glib::ControlFlow::Break;
            }
            if p.log_view.is_mapped() {
                p.load_log();
            }
            glib::ControlFlow::Continue
        });
    });

    let p = page.clone();
    ui::on_shown(&scroller, move || p.refresh());
    scroller.upcast()
}

impl Page {
    fn refresh(self: &Rc<Self>) {
        self.load_log();
        let p = self.clone();
        self.ctx.spawn(async move {
            p.sync_gamemode().await;
            let (m, live, olh) = rt::blocking(|| {
                let olh = openlinkhub::detect()
                    .map(|i| openlinkhub::read(&i).map_err(|e| format!("{e:#}")));
                (model::load(), ctl::config_errors(), olh)
            })
            .await;
            p.fill_health(&m, live);
            ui::clear(&p.olh);
            if let Some(olh) = olh {
                p.olh.append(&olh_group(&p.ctx, olh));
            }
        });
    }

    async fn sync_gamemode(&self) {
        match rt::blocking(gamemode::is_on).await {
            Ok(on) => {
                self.syncing.set(true);
                self.gamemode.set_active(on);
                self.syncing.set(false);
                self.gamemode.set_sensitive(true);
            }
            Err(e) => {
                self.gamemode.set_sensitive(false);
                self.ctx.error("Reading game mode state", &e);
            }
        }
    }

    fn load_log(self: &Rc<Self>) {
        let p = self.clone();
        self.ctx.spawn(async move {
            let text = match rt::blocking(ctl::rolling_log).await {
                Ok(t) => t,
                Err(e) => {
                    p.ctx.error("Reading the Hyprland log", &e);
                    return;
                }
            };
            let buf = p.log_view.buffer();
            if buf.text(&buf.start_iter(), &buf.end_iter(), false) == text {
                return;
            }
            let adj = p.log_scroll.vadjustment();
            let at_bottom = adj.value() + adj.page_size() >= adj.upper() - 4.0;
            buf.set_text(&text);
            if at_bottom {
                // After layout, so the new end is known.
                glib::idle_add_local_once(move || adj.set_value(adj.upper()));
            }
        });
    }

    fn fill_health(self: &Rc<Self>, m: &model::ConfigModel, live: anyhow::Result<Vec<String>>) {
        ui::clear(&self.health);

        let errors = adw::PreferencesGroup::builder()
            .title("Config health")
            .description("Errors, and settings or keybinds in ~/.config/hypr that are silently replaced later")
            .build();
        let recheck = widgets::icon_button("view-refresh-symbolic", "Re-check");
        let p = self.clone();
        recheck.connect_clicked(move |_| p.refresh());
        errors.set_header_suffix(Some(&recheck));
        let mut any = false;
        let mut error_row = |text: &str, origin: &str| {
            any = true;
            let r = row(text, origin);
            r.add_prefix(&gtk::Image::from_icon_name("dialog-error-symbolic"));
            errors.add(&r);
        };
        for e in &m.errors {
            error_row(e, "Error while evaluating the Lua config");
        }
        match live {
            Ok(list) => {
                for e in &list {
                    error_row(e, "Reported by Hyprland (hyprctl configerrors)");
                }
            }
            Err(e) => error_row(
                "Could not ask Hyprland for config errors",
                &format!("{e:#}"),
            ),
        }
        if !any {
            let r = row(
                "No config errors",
                "Hyprland and hyprdeck both load the config cleanly",
            );
            r.add_prefix(&gtk::Image::from_icon_name("emblem-ok-symbolic"));
            errors.add(&r);
        }
        self.health.append(&errors);

        let overrides = health::option_overrides(m);
        if !overrides.is_empty() {
            let g = adw::PreferencesGroup::builder()
                .title("Settings set in several places")
                .description("The last assignment wins; the earlier ones have no effect")
                .build();
            for o in overrides {
                let exp = adw::ExpanderRow::builder().use_markup(false).build();
                exp.set_title(o.key);
                exp.set_subtitle(&format!(
                    "= {} from {}",
                    o.winner.value.display(),
                    o.winner.source.display()
                ));
                for e in &o.overridden {
                    exp.add_row(&row(
                        &format!("{} (overridden)", e.value.display()),
                        &e.source.display(),
                    ));
                }
                g.add(&exp);
            }
            self.health.append(&g);
        }

        let binds = health::bind_overrides(m);
        if !binds.is_empty() {
            let g = adw::PreferencesGroup::builder()
                .title("Replaced keybinds")
                .description("Keybinds removed or doubled by a later bind on the same keys")
                .build();
            for b in binds {
                let title = format!("{} — {}", b.bind.keys, health::bind_action(b.bind));
                let subtitle = match &b.fate {
                    BindFate::Unbound {
                        by,
                        replacement: Some(r),
                    } => format!(
                        "{} is replaced by \"{}\" from {} (unbound at {})",
                        b.bind.source.display(),
                        health::bind_action(r),
                        r.source.display(),
                        by.source.display()
                    ),
                    BindFate::Unbound {
                        by,
                        replacement: None,
                    } => {
                        format!(
                            "{} is disabled by hl.unbind at {}",
                            b.bind.source.display(),
                            by.source.display()
                        )
                    }
                    BindFate::AlsoBound { by } => format!(
                        "{}: {} binds the same keys again (\"{}\"); both run",
                        b.bind.source.display(),
                        by.source.display(),
                        health::bind_action(by)
                    ),
                };
                g.add(&row(&title, &subtitle));
            }
            self.health.append(&g);
        }
    }
}

/// Row showing a shell command to run by hand, with a copy button.
fn command_row(ctx: &Ctx, title: &str, command: &str) -> adw::ActionRow {
    let r = row(title, command);
    r.set_subtitle_selectable(true);
    r.add_prefix(&gtk::Image::from_icon_name("utilities-terminal-symbolic"));
    let copy = widgets::icon_button("edit-copy-symbolic", "Copy command");
    let (c, r2) = (ctx.clone(), r.clone());
    copy.connect_clicked(move |_| widgets::copy_text(&c, &r2.subtitle().unwrap_or_default()));
    r.add_suffix(&copy);
    r
}

fn olh_group(ctx: &Ctx, olh: Result<OlhInfo, String>) -> adw::PreferencesGroup {
    let g = adw::PreferencesGroup::builder()
        .title("Corsair devices (OpenLinkHub)")
        .description(
            "After every wake OpenLinkHub waits the resume delay, then exits so systemd restarts it. Keyboards, mice \
             and other devices it drives don't respond until it is back: roughly resume delay + restart delay.",
        )
        .build();
    let o = match olh {
        Ok(o) => o,
        Err(e) => {
            g.add(&row("Could not read OpenLinkHub settings", &e));
            return g;
        }
    };
    let total = row(
        "Devices back after wake",
        "Resume delay + restart delay, plus device start-up",
    );
    let total_label = gtk::Label::new(Some(&format!("≈ {:.1} s", o.wake_delay())));
    total.add_suffix(&total_label);

    let delay = adw::SpinRow::builder()
        .use_markup(false)
        // OpenLinkHub needs the USB bus to settle after wake; don't offer less
        // than 1 s unless the config already has less.
        .adjustment(&gtk::Adjustment::new(
            o.resume_delay_ms as f64,
            (o.resume_delay_ms as f64).min(1000.0),
            60000.0,
            500.0,
            1000.0,
            0.0,
        ))
        .build();
    delay.set_title("Resume delay (ms)");
    delay.set_subtitle(&format!(
        "resumeDelay in {} — time for USB to settle; applies from the next start",
        widgets::tilde(&o.config)
    ));
    let restart = adw::SpinRow::builder()
        .use_markup(false)
        .adjustment(&gtk::Adjustment::new(
            o.restart_sec.round(),
            0.0,
            60.0,
            1.0,
            5.0,
            0.0,
        ))
        .build();
    restart.set_title("Restart delay (s)");
    restart.set_subtitle(&format!(
        "RestartSec in {}",
        widgets::tilde(&openlinkhub::dropin_path(o.scope))
    ));
    g.add(&delay);
    g.add(&restart);
    g.add(&total);

    // Changes hyprdeck cannot write itself (root-owned config, system unit)
    // become commands to run by hand.
    let delay_cmd = command_row(ctx, "Run as root to change the resume delay", "");
    delay_cmd.set_visible(false);
    g.add(&delay_cmd);
    let restart_cmd = command_row(ctx, "Run as root to change the restart delay", "");
    restart_cmd.set_visible(false);
    g.add(&restart_cmd);

    let update_total = {
        let (delay, restart, total_label) = (delay.clone(), restart.clone(), total_label.clone());
        move || {
            total_label.set_text(&format!(
                "≈ {:.1} s",
                delay.value() / 1000.0 + restart.value()
            ))
        }
    };
    let ctx2 = ctx.clone();
    let u = update_total.clone();
    let (config, writable) = (o.config.clone(), o.config_writable);
    widgets::on_spin_settled(&delay, move |v| {
        u();
        let ms = v.round() as u64;
        if !writable {
            delay_cmd.set_subtitle(&openlinkhub::resume_delay_command(&config, ms));
            delay_cmd.set_visible(true);
            return;
        }
        let (ctx, config) = (ctx2.clone(), config.clone());
        ctx2.spawn(async move {
            match rt::blocking(move || openlinkhub::save_resume_delay(&config, ms)).await {
                Ok(()) => ctx.toast(format!(
                    "Resume delay set to {ms} ms — restart OpenLinkHub to apply"
                )),
                Err(e) => ctx.error("Saving OpenLinkHub config", &e),
            }
        });
    });
    let ctx2 = ctx.clone();
    let scope = o.scope;
    widgets::on_spin_settled(&restart, move |v| {
        update_total();
        let secs = v.round() as u32;
        if scope == Scope::System {
            restart_cmd.set_subtitle(&openlinkhub::restart_sec_command(secs));
            restart_cmd.set_visible(true);
            return;
        }
        let ctx = ctx2.clone();
        ctx2.spawn(async move {
            match rt::blocking(move || openlinkhub::save_restart_sec(secs)).await {
                Ok(()) => ctx.toast(format!("OpenLinkHub restart delay set to {secs} s")),
                Err(e) => ctx.error("Saving the OpenLinkHub drop-in", &e),
            }
        });
    });

    let status = |active: &str| format!("{} ({}) is {active}", openlinkhub::UNIT, scope.label());
    match scope {
        Scope::System => {
            g.add(&row("Service", &status(&o.active)));
            g.add(&command_row(
                ctx,
                "Restart OpenLinkHub (needs root)",
                &openlinkhub::restart_command(scope),
            ));
        }
        Scope::User => {
            let service = row("Service", &status(&o.active));
            let restart_btn = widgets::button("Restart OpenLinkHub");
            service.add_suffix(&restart_btn);
            g.add(&service);
            let ctx = ctx.clone();
            restart_btn.connect_clicked(move |b| {
                let (ctx, b, service) = (ctx.clone(), b.clone(), service.clone());
                ctx.clone().spawn(async move {
                    if !ctx
                        .confirm(
                            "Restart OpenLinkHub?",
                            "Keyboards, mice and other OpenLinkHub devices stop responding for a second or two while it \
                             restarts.",
                            "Restart",
                            false,
                        )
                        .await
                    {
                        return;
                    }
                    busy(&b, "Restart OpenLinkHub", true);
                    let r = rt::blocking(|| {
                        openlinkhub::restart_user()?;
                        let install = openlinkhub::detect().ok_or_else(|| anyhow::anyhow!("OpenLinkHub is gone"))?;
                        openlinkhub::read(&install)
                    })
                    .await;
                    busy(&b, "Restart OpenLinkHub", false);
                    match r {
                        Ok(o) => {
                            service.set_subtitle(&format!(
                                "{} ({}) is {}",
                                openlinkhub::UNIT,
                                o.scope.label(),
                                o.active
                            ));
                            ctx.toast("OpenLinkHub restarted");
                        }
                        Err(e) => ctx.error("Restarting OpenLinkHub", &e),
                    }
                });
            });
        }
    }
    g
}
