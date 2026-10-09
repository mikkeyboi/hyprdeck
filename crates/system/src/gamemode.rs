//! Game mode: a runtime-only `hl.config` override that drops animations,
//! blur, shadows, rounding and gaps and allows tearing. Turning it off reloads
//! the config, which restores every hand-written and managed value.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Result;
use hyprdeck_core::hypr::ctl;
use hyprdeck_core::tray::{self, TrayItem, TrayProvider};
use hyprdeck_core::{notify, rt};
use tokio::io::AsyncBufReadExt;

const ON_LUA: &str = "hl.config({ \
    animations = { enabled = false }, \
    decoration = { rounding = 0, blur = { enabled = false }, shadow = { enabled = false } }, \
    general = { gaps_in = 0, gaps_out = 0, allow_tearing = true } \
})";
const STATE_LUA: &str =
    "return (not hl.get_config('animations.enabled')) and hl.get_config('general.allow_tearing')";
const TRAY_ID: &str = "system.gamemode";

/// Last known state (for the tray menu, which must not block).
static ACTIVE: AtomicBool = AtomicBool::new(false);

fn remember(on: bool) {
    if ACTIVE.swap(on, Ordering::Relaxed) != on {
        tray::refresh();
    }
}

/// Query the compositor. Blocking.
pub fn is_on() -> Result<bool> {
    let on = ctl::repl(STATE_LUA)? == "true";
    remember(on);
    Ok(on)
}

/// Turn game mode on or off. Blocking.
pub fn set(on: bool) -> Result<()> {
    if on {
        ctl::eval(ON_LUA)?;
    } else {
        ctl::reload(true)?;
    }
    remember(is_on()?);
    Ok(())
}

/// Flip the live state; returns the new state. Blocking.
pub fn toggle() -> Result<bool> {
    let on = !is_on()?;
    set(on)?;
    Ok(on)
}

struct Tray;

impl TrayProvider for Tray {
    fn items(&self) -> Vec<TrayItem> {
        vec![TrayItem::Check {
            label: "Game mode".into(),
            id: TRAY_ID.into(),
            checked: ACTIVE.load(Ordering::Relaxed),
            enabled: true,
        }]
    }

    fn activate(&self, id: &str) {
        if id != TRAY_ID {
            return;
        }
        rt::spawn(async {
            if let Err(e) = tokio::task::spawn_blocking(toggle)
                .await
                .map_err(anyhow::Error::from)
                .and_then(|r| r)
            {
                notify::notify_bg(
                    notify::Category::System,
                    notify::Severity::Error,
                    "Game mode",
                    format!("{e:#}"),
                );
            }
        });
    }
}

/// Register the tray item and keep its state in sync with config reloads.
pub fn start() {
    tray::register(Arc::new(Tray));
    rt::spawn(async {
        loop {
            let _ = tokio::task::spawn_blocking(is_on).await;
            if let Err(e) = follow_events().await {
                tracing::debug!("game mode: hyprland event socket: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

/// Re-check the state after every `configreloaded` event (a reload ends game mode).
async fn follow_events() -> Result<()> {
    let runtime = std::env::var("XDG_RUNTIME_DIR")?;
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")?;
    let path = format!("{runtime}/hypr/{sig}/.socket2.sock");
    let stream = tokio::net::UnixStream::connect(&path).await?;
    let mut lines = tokio::io::BufReader::new(stream).lines();
    while let Some(line) = lines.next_line().await? {
        if line.starts_with("configreloaded>>") {
            let _ = tokio::task::spawn_blocking(is_on).await;
        }
    }
    Ok(())
}
