//! App-level preferences: startup, tray behaviour and notification delivery.

use adw::prelude::*;
use anyhow::Result;
use hyprdeck_core::{cmd, notify, rt, store};
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
    add_notification_preferences(ctx, &page);
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

fn add_notification_preferences(ctx: &hyprdeck_core::ui::Ctx, page: &adw::PreferencesPage) {
    let group = adw::PreferencesGroup::builder()
        .title("Notifications")
        .description("Desktop popups become in-app toasts while Hyprdeck is open. In-app only never opens a window. Updates remain available in the tray and Updates page.")
        .build();
    let delivery_labels: Vec<_> = notify::Delivery::ALL.iter().map(|d| d.label()).collect();
    let delivery = adw::ComboRow::builder()
        .title("Default delivery")
        .model(&gtk::StringList::new(&delivery_labels))
        .build();
    let timeout_labels: Vec<_> = notify::PopupTimeout::ALL
        .iter()
        .map(|t| t.label())
        .collect();
    let timeout = adw::ComboRow::builder()
        .title("Popup lifetime")
        .subtitle("Action buttons do not make a popup persistent")
        .model(&gtk::StringList::new(&timeout_labels))
        .build();
    let duration = adw::SpinRow::with_range(1.0, 3600.0, 1.0);
    duration.set_title("Popup duration");
    duration.set_subtitle("Seconds; the desktop notification server controls expiration");
    let error_duration = adw::SpinRow::with_range(1.0, 3600.0, 1.0);
    error_duration.set_title("Problem popup duration");
    error_duration.set_subtitle("Seconds for failures and wake-recovery problems");
    for row in [
        delivery.upcast_ref::<gtk::Widget>(),
        timeout.upcast_ref(),
        duration.upcast_ref(),
        error_duration.upcast_ref(),
    ] {
        group.add(row);
    }
    page.add(&group);

    let categories = adw::PreferencesGroup::builder()
        .title("Notification categories")
        .description("Delivery controls do not disable update checks, automatic installation or wake recovery.")
        .build();
    let mut override_labels = vec!["Use default"];
    override_labels.extend(delivery_labels);
    let rows: Vec<_> = notify::Category::ALL
        .iter()
        .map(|&category| {
            let row = adw::ComboRow::builder()
                .title(category.label())
                .model(&gtk::StringList::new(&override_labels))
                .build();
            categories.add(&row);
            (category, row)
        })
        .collect();
    page.add(&categories);
    group.set_sensitive(false);
    categories.set_sensitive(false);

    let ctx = ctx.clone();
    ctx.clone().spawn(async move {
        let settings = match rt::blocking(notify::load_settings).await {
            Ok(settings) => settings,
            Err(e) => {
                ctx.error("Loading notification preferences failed", &e);
                return;
            }
        };
        delivery.set_selected(
            notify::Delivery::ALL
                .iter()
                .position(|d| *d == settings.delivery)
                .unwrap() as u32,
        );
        timeout.set_selected(
            notify::PopupTimeout::ALL
                .iter()
                .position(|t| *t == settings.timeout)
                .unwrap() as u32,
        );
        duration.set_value(f64::from(settings.duration_seconds));
        error_duration.set_value(f64::from(settings.error_duration_seconds));
        let timed = settings.timeout == notify::PopupTimeout::Timed;
        duration.set_sensitive(timed);
        error_duration.set_sensitive(timed);
        for (category, row) in &rows {
            row.set_selected(settings.overrides.get(category).map_or(0, |delivery| {
                notify::Delivery::ALL
                    .iter()
                    .position(|d| d == delivery)
                    .unwrap() as u32
                    + 1
            }));
        }
        group.set_sensitive(true);
        categories.set_sensitive(true);

        let persist = {
            let ctx = ctx.clone();
            move |change: &dyn Fn(&mut notify::Settings)| {
                let mut settings = notify::settings();
                change(&mut settings);
                if let Err(e) = notify::save_settings(settings) {
                    ctx.error("Saving notification preferences failed", &e);
                }
            }
        };
        let save = persist.clone();
        delivery.connect_selected_notify(move |row| {
            if let Some(&delivery) = notify::Delivery::ALL.get(row.selected() as usize) {
                save(&|settings| settings.delivery = delivery);
            }
        });
        let save = persist.clone();
        let normal = duration.clone();
        let errors = error_duration.clone();
        timeout.connect_selected_notify(move |row| {
            if let Some(&timeout) = notify::PopupTimeout::ALL.get(row.selected() as usize) {
                save(&|settings| settings.timeout = timeout);
                normal.set_sensitive(timeout == notify::PopupTimeout::Timed);
                errors.set_sensitive(timeout == notify::PopupTimeout::Timed);
            }
        });
        let save = persist.clone();
        duration.connect_value_notify(move |row| {
            save(&|settings| settings.duration_seconds = row.value() as u32);
        });
        let save = persist.clone();
        error_duration.connect_value_notify(move |row| {
            save(&|settings| settings.error_duration_seconds = row.value() as u32);
        });
        for (category, row) in rows {
            let save = persist.clone();
            row.connect_selected_notify(move |row| {
                let selected = row.selected();
                save(&|settings| {
                    if selected == 0 {
                        settings.overrides.remove(&category);
                    } else if let Some(&delivery) = notify::Delivery::ALL.get(selected as usize - 1)
                    {
                        settings.overrides.insert(category, delivery);
                    }
                });
            });
        }
    });
}
