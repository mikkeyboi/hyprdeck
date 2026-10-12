//! Opt-in XDG login launch. The desktop entry is the source of truth, so the
//! Gaming and Startup Apps switches agree without a second persisted boolean.

use anyhow::{Context, Result, bail};
use hd_startup::xdg::{self, Autostart};
use hyprdeck_core::{cmd, store};

pub const ID: &str = "io.github.mikkeyboi.Hyprdeck.Gamescope.desktop";
const COMMAND: &str = "hyprdeck gamescope launch";

fn entry() -> Result<Option<Autostart>> {
    let entry = xdg::scan().into_iter().find(|entry| entry.id == ID);
    if let Some(entry) = &entry {
        if entry.command.trim() != COMMAND {
            bail!(
                "{} has a different launch command. Repair it in Startup Apps; no entry was overwritten.",
                entry.path.display()
            );
        }
    } else if xdg::user_dir().join(ID).exists() {
        bail!("Cannot read the Gamescope login entry; repair its permissions in Startup Apps.");
    }
    Ok(entry)
}

/// Missing entry means off. Does not create files or start any processes.
pub fn enabled() -> Result<bool> {
    Ok(entry()?.is_some_and(|entry| entry.eval.enabled))
}

/// Schedule or unschedule the saved launcher for the next login only.
pub fn set_enabled(on: bool) -> Result<()> {
    if entry()?.is_some() {
        return xdg::set_enabled(ID, on);
    }
    if !on {
        return Ok(());
    }
    cmd::which("hyprdeck")
        .context("Install Hyprdeck on PATH before enabling its Gamescope login launcher")?;
    let mut text = xdg::custom_entry("Steam Big Picture (Gamescope)", COMMAND);
    text.push_str("Icon=input-gaming-symbolic\n");
    store::write_atomic(&xdg::user_dir().join(ID), text.as_bytes())
}
