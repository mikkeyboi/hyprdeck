//! Opt-in XDG login launch. The desktop entry is the source of truth, so the
//! Gaming and Startup Apps switches agree without a second persisted boolean.

use anyhow::{Result, bail};
use hd_startup::xdg::{self, Autostart};
use hyprdeck_core::{cmd, store};

pub const ID: &str = "io.github.mikkeyboi.Hyprdeck.Gamescope.desktop";
const COMMAND: &str = "hyprdeck gamescope launch";
fn executable_path() -> Result<String> {
    if let Some(path) = cmd::which("hyprdeck")
        && path.starts_with('/')
    {
        return Ok(path);
    }
    let fallback = store::home().join(".local/bin/hyprdeck");
    if let Some(path) = fallback.to_str().and_then(cmd::which) {
        return Ok(path);
    }
    bail!(
        "Install Hyprdeck on PATH or in ~/.local/bin/hyprdeck before enabling its Gamescope login launcher"
    )
}

pub fn command_string() -> Result<String> {
    let exe = executable_path()?;
    if exe.contains(|c: char| {
        c.is_whitespace()
            || matches!(
                c,
                '"' | '\''
                    | '\\'
                    | '>'
                    | '<'
                    | '~'
                    | '|'
                    | '&'
                    | ';'
                    | '$'
                    | '*'
                    | '?'
                    | '#'
                    | '('
                    | ')'
                    | '`'
            )
    }) {
        let escaped = exe
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('`', "\\`")
            .replace('$', "\\$");
        Ok(format!("\"{escaped}\" gamescope launch"))
    } else {
        Ok(format!("{exe} gamescope launch"))
    }
}

pub fn is_matching_command(cmd: &str) -> bool {
    let trimmed = cmd.trim();
    if trimmed == COMMAND {
        return true;
    }
    let Some(prefix) = trimmed.strip_suffix("gamescope launch") else {
        return false;
    };
    let prog = prefix.trim().trim_matches('"');
    prog == "hyprdeck" || prog.ends_with("/hyprdeck")
}

fn entry() -> Result<Option<Autostart>> {
    let entry = xdg::scan().into_iter().find(|entry| entry.id == ID);
    if let Some(entry) = &entry {
        if !is_matching_command(&entry.command) {
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
    let existing = entry()?;
    if !on {
        if existing.is_some() {
            return xdg::set_enabled(ID, false);
        }
        return Ok(());
    }
    let cmd = command_string()?;
    let mut text = xdg::custom_entry("Steam Big Picture (Gamescope)", &cmd);
    text.push_str("Icon=input-gaming-symbolic\n");
    store::write_atomic(&xdg::user_dir().join(ID), text.as_bytes())?;
    xdg::set_enabled(ID, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_bare_and_absolute_hyprdeck_commands() {
        assert!(is_matching_command("hyprdeck gamescope launch"));
        assert!(is_matching_command(
            "/home/user/.local/bin/hyprdeck gamescope launch"
        ));
        assert!(is_matching_command(
            "\"/home/user/.local/bin/hyprdeck\" gamescope launch"
        ));
        assert!(!is_matching_command("steam -gamepadui"));
        assert!(!is_matching_command("hyprdeck gamescope plan"));
    }
}
