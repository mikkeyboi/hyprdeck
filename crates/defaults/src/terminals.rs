//! Preferred terminal for `xdg-terminal-exec` (the launcher apps use for
//! `Terminal=true` desktop entries): `xdg-terminals.list` files, each a list of
//! desktop entry ids, first usable entry wins.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use hyprdeck_core::{cmd, store};

use crate::apps::{self, App};

pub const LAUNCHER: &str = "xdg-terminal-exec";

pub fn launcher_installed() -> bool {
    cmd::which(LAUNCHER).is_some()
}

fn desktops() -> Vec<String> {
    std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .split(':')
        .filter(|d| !d.is_empty())
        .map(str::to_lowercase)
        .collect()
}

fn names() -> Vec<String> {
    desktops()
        .into_iter()
        .map(|d| format!("{d}-xdg-terminals.list"))
        .chain(["xdg-terminals.list".to_owned()])
        .collect()
}

fn config_home() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| store::home().join(".config"))
}

/// Every list file in lookup order: config dirs, then
/// `<data dir>/xdg-terminal-exec/`, desktop-specific names first in each dir.
fn lookup_paths() -> Vec<PathBuf> {
    let mut dirs = vec![config_home()];
    let config_dirs = std::env::var("XDG_CONFIG_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/etc/xdg".to_owned());
    dirs.extend(
        config_dirs
            .split(':')
            .filter(|d| d.starts_with('/'))
            .map(PathBuf::from),
    );
    dirs.extend(
        apps::data_dirs()
            .into_iter()
            .map(|d| d.join("xdg-terminal-exec")),
    );
    let names = names();
    dirs.iter()
        .flat_map(|d| names.iter().map(move |n| d.join(n)))
        .collect()
}

/// User files hyprdeck edits: an existing desktop-specific list in
/// `~/.config` wins, otherwise `~/.config/xdg-terminals.list`.
pub fn user_path() -> PathBuf {
    let home = config_home();
    names()
        .into_iter()
        .map(|n| home.join(n))
        .find(|p| p.is_file())
        .unwrap_or_else(|| home.join("xdg-terminals.list"))
}

/// Desktop id of an entry line (`foot.desktop:server` → `foot.desktop`);
/// `None` for comments, blanks, directives and exclusions.
pub fn entry_id(line: &str) -> Option<&str> {
    let t = line.trim();
    if t.is_empty() || t.starts_with(['#', '/', '-']) {
        return None;
    }
    let t = t.strip_prefix('+').unwrap_or(t);
    let id = t.split(':').next().unwrap_or(t).trim();
    (!id.is_empty()).then_some(id)
}

/// Entry ids in file order.
pub fn entries(text: &str) -> Vec<&str> {
    text.lines().filter_map(entry_id).collect()
}

/// Put `id` first: drop its other lines and insert it before the first entry.
pub fn prefer(text: &str, id: &str) -> String {
    let mut out = String::with_capacity(text.len() + id.len() + 1);
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        if let Some(e) = entry_id(line) {
            if !inserted {
                out.push_str(id);
                out.push('\n');
                inserted = true;
            }
            if e == id {
                continue;
            }
        }
        out.push_str(line);
    }
    if !inserted {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(id);
        out.push('\n');
    }
    out
}

/// Remove every entry line, keeping comments and directives.
pub fn clear_entries(text: &str) -> String {
    text.split_inclusive('\n')
        .filter(|l| entry_id(l).is_none())
        .collect()
}

/// Installed terminal emulators (desktop entries in the `TerminalEmulator`
/// category). Blocking.
pub fn candidates() -> Vec<App> {
    let files = apps::desktop_files();
    apps::all()
        .into_iter()
        .filter(|a| {
            files.get(&a.id).is_some_and(|p| {
                std::fs::read_to_string(p).is_ok_and(|t| {
                    apps::desktop_entry_value(&t, "Categories")
                        .is_some_and(|c| c.split(';').any(|c| c.trim() == "TerminalEmulator"))
                })
            })
        })
        .collect()
}

#[derive(Debug, Clone)]
pub struct TerminalState {
    /// `xdg-terminal-exec` is on PATH.
    pub launcher: bool,
    /// Preferred terminal from the lists, if one names an installed terminal.
    pub current: Option<App>,
    /// File the preference comes from.
    pub source: Option<PathBuf>,
    pub candidates: Vec<App>,
    /// Terminal hyprdeck itself uses (`$TERMINAL` or the first one found).
    pub detected: Option<String>,
}

/// Blocking.
pub fn state() -> TerminalState {
    let candidates = candidates();
    let mut current = None;
    let mut source = None;
    'files: for path in lookup_paths() {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for id in entries(&text) {
            let id = apps::desktop_id(id);
            if let Some(app) = candidates.iter().find(|a| a.id == id) {
                current = Some(app.clone());
                source = Some(path);
                break 'files;
            }
        }
    }
    TerminalState {
        launcher: launcher_installed(),
        current,
        source,
        candidates,
        detected: cmd::terminal().map(|t| t.program),
    }
}

fn edit(path: &Path, f: impl Fn(&str) -> String) -> Result<()> {
    let path = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let new = f(&text);
    if new == text {
        return Ok(());
    }
    if new.trim().is_empty() && path.exists() {
        // Nothing left: remove the file so the system lists apply again.
        return std::fs::remove_file(&path).with_context(|| format!("removing {}", path.display()));
    }
    store::write_atomic(&path, new.as_bytes())
}

/// Make `id` the preferred terminal. Blocking.
pub fn set(id: &str) -> Result<()> {
    let id = apps::desktop_id(id);
    edit(&user_path(), |t| prefer(t, &id))
}

/// Drop the user's preference so the system lists (or xdg-terminal-exec's own
/// pick) apply. Blocking.
pub fn reset() -> Result<()> {
    let home = config_home();
    for n in names() {
        let p = home.join(n);
        if p.is_file() {
            edit(&p, clear_entries)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_entries() {
        let text = "# preferred\nfoot.desktop:server\n\n+kitty.desktop\n-xterm.desktop\n/execarg_default\nalacritty\n";
        assert_eq!(
            entries(text),
            ["foot.desktop", "kitty.desktop", "alacritty"]
        );
    }

    #[test]
    fn prefer_moves_to_front_and_round_trips() {
        let text = "# terminals\nfoot.desktop\nkitty.desktop\n";
        let out = prefer(text, "kitty.desktop");
        assert_eq!(out, "# terminals\nkitty.desktop\nfoot.desktop\n");
        assert_eq!(prefer(&out, "foot.desktop"), text);
        assert_eq!(prefer("", "kitty.desktop"), "kitty.desktop\n");
        assert_eq!(
            prefer("# only a comment", "kitty.desktop"),
            "# only a comment\nkitty.desktop\n"
        );
    }

    #[test]
    fn clears_entries_only() {
        assert_eq!(
            clear_entries("# c\nfoot.desktop\n/execarg_default\n"),
            "# c\n/execarg_default\n"
        );
        assert_eq!(clear_entries("kitty.desktop\n"), "");
    }
}
