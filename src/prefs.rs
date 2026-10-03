//! App-level preferences: login autostart, start minimized, close to tray.

use adw::prelude::*;
use anyhow::Result;
use hyprdeck_core::{cmd, rt, store};
use serde::{Deserialize, Serialize};

pub const UNIT: &str = "hyprdeck.service";
const STORE: &str = "app";

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct AppPrefs {
    /// When launched at login (`--background`), stay in the tray instead of opening the window.
    pub start_minimized: bool,
    /// Closing the window keeps hyprdeck running in the tray.
    pub close_to_tray: bool,
}

impl Default for AppPrefs {
    fn default() -> Self {
        AppPrefs {
            start_minimized: true,
            close_to_tray: true,
        }
    }
}

pub fn load() -> AppPrefs {
    store::load(STORE).unwrap_or_else(|e| {
        tracing::error!("{e:#}; using default app preferences");
        AppPrefs::default()
    })
}

fn save(p: &AppPrefs) -> Result<()> {
    store::save(STORE, p)
}

/// `systemctl --user is-enabled hyprdeck.service` == enabled.
fn login_enabled() -> bool {
    cmd::output("systemctl", ["--user", "is-enabled", UNIT])
        .is_ok_and(|o| o.stdout.trim() == "enabled")
}

fn set_login_enabled(on: bool) -> Result<()> {
    cmd::systemctl_user([if on { "enable" } else { "disable" }, UNIT]).map(drop)
}

pub fn show_dialog(ctx: &hyprdeck_core::ui::Ctx) {
    let prefs = load();
    let dialog = adw::PreferencesDialog::new();
    dialog.set_title("Preferences");
    let page = adw::PreferencesPage::new();
    let group = adw::PreferencesGroup::builder()
        .title("Startup")
        .description("Hyprdeck runs in the tray to keep audio routing, the wake guard and update checks active.")
        .build();

    let login = adw::SwitchRow::builder()
        .title("Launch at login")
        .subtitle("Start hyprdeck with the graphical session (systemd user unit)")
        .sensitive(false)
        .build();
    let minimized = adw::SwitchRow::builder()
        .title("Start minimized")
        .subtitle("At login, start in the tray without opening this window")
        .active(prefs.start_minimized)
        .build();
    let to_tray = adw::SwitchRow::builder()
        .title("Close to tray")
        .subtitle("Closing the window keeps hyprdeck running; quit from the tray menu")
        .active(prefs.close_to_tray)
        .build();
    group.add(&login);
    group.add(&minimized);
    group.add(&to_tray);
    page.add(&group);
    dialog.add(&page);

    // Load the unit state off the main thread, then wire the switch.
    let c = ctx.clone();
    let login_row = login.clone();
    ctx.spawn(async move {
        let enabled = rt::blocking(login_enabled).await;
        login_row.set_active(enabled);
        login_row.set_sensitive(true);
        login_row.connect_active_notify(move |row| {
            let on = row.is_active();
            let c = c.clone();
            let row = row.clone();
            c.clone().spawn(async move {
                row.set_sensitive(false);
                match rt::blocking(move || set_login_enabled(on)).await {
                    Ok(()) => c.toast(if on {
                        "Hyprdeck will start at login"
                    } else {
                        "Hyprdeck will no longer start at login"
                    }),
                    Err(e) => c.error("Changing login autostart failed", &e),
                }
                row.set_sensitive(true);
            });
        });
    });

    let c = ctx.clone();
    let persist = move |f: &dyn Fn(&mut AppPrefs)| {
        let mut p = load();
        f(&mut p);
        if let Err(e) = save(&p) {
            c.error("Saving preferences failed", &e);
        }
    };
    let p1 = persist.clone();
    minimized.connect_active_notify(move |r| {
        let v = r.is_active();
        p1(&|p| p.start_minimized = v);
    });
    to_tray.connect_active_notify(move |r| {
        let v = r.is_active();
        persist(&|p| p.close_to_tray = v);
    });

    dialog.present(Some(&ctx.window));
}
