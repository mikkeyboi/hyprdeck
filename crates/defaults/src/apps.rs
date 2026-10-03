//! Installed desktop apps as plain data, so lookups can run off the GTK main
//! thread and results can be sent back to it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use gtk::gio;
use gtk::gio::prelude::*;

#[derive(Debug, Clone, PartialEq)]
pub struct App {
    /// Desktop entry id (`org.example.App.desktop`).
    pub id: String,
    pub name: String,
    /// Serialized `GIcon` (see [`gio::Icon::for_string`]).
    pub icon: Option<String>,
    /// Command line without `%U`-style field codes.
    pub command: Option<String>,
    /// Shown in app menus (not `NoDisplay`, not hidden for this desktop).
    pub visible: bool,
}

impl App {
    pub fn from_info(info: &gio::AppInfo) -> Option<App> {
        Some(App {
            id: info.id()?.to_string(),
            name: info.display_name().to_string(),
            icon: info
                .icon()
                .and_then(|i| IconExt::to_string(&i))
                .map(Into::into),
            command: info
                .commandline()
                .map(|c| command_from_exec(&c.to_string_lossy())),
            visible: info.should_show(),
        })
    }

    /// GTK image for the app's icon (generic executable icon as fallback).
    pub fn image(&self, size: i32) -> gtk::Image {
        let image = gtk::Image::builder().pixel_size(size).build();
        match self
            .icon
            .as_deref()
            .and_then(|s| gio::Icon::for_string(s).ok())
        {
            Some(icon) => image.set_from_gicon(&icon),
            None => image.set_icon_name(Some("application-x-executable")),
        }
        image
    }
}

/// Normalise a user-supplied app id (`firefox` → `firefox.desktop`).
pub fn desktop_id(id: &str) -> String {
    if id.ends_with(".desktop") {
        id.to_owned()
    } else {
        format!("{id}.desktop")
    }
}

/// Every installed app (including hidden helper entries), sorted by name. Blocking.
pub fn all() -> Vec<App> {
    let mut apps: Vec<App> = gio::AppInfo::all()
        .iter()
        .filter_map(App::from_info)
        .collect();
    apps.sort_by_cached_key(|a| a.name.to_lowercase());
    apps
}

/// Look up an installed app by desktop id. Blocking.
pub fn find(id: &str) -> Option<App> {
    let id = desktop_id(id);
    gio::AppInfo::all()
        .iter()
        .find(|a| a.id().is_some_and(|i| i == id.as_str()))
        .and_then(App::from_info)
}

/// Apps that declare support for `mime` (directly, via a parent type, or via
/// an added association). Blocking.
pub fn for_type(mime: &str) -> Vec<App> {
    gio::AppInfo::all_for_type(mime)
        .iter()
        .filter_map(App::from_info)
        .collect()
}

/// The app that currently opens `mime`. Blocking.
pub fn default_for(mime: &str) -> Option<App> {
    gio::AppInfo::default_for_type(mime, false)
        .as_ref()
        .and_then(App::from_info)
}

/// Turn a desktop entry `Exec` line into a plain command (drop `%U`-style field codes).
pub fn command_from_exec(exec: &str) -> String {
    exec.split_whitespace()
        .filter(|t| !(t.len() == 2 && t.starts_with('%')))
        .collect::<Vec<_>>()
        .join(" ")
        .replace("%%", "%")
}

/// `$XDG_DATA_HOME` followed by `$XDG_DATA_DIRS`, most important first.
pub fn data_dirs() -> Vec<PathBuf> {
    let mut dirs =
        vec![dirs::data_dir().unwrap_or_else(|| hyprdeck_core::store::home().join(".local/share"))];
    let system = std::env::var("XDG_DATA_DIRS")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "/usr/local/share:/usr/share".to_owned());
    dirs.extend(
        system
            .split(':')
            .filter(|d| d.starts_with('/'))
            .map(PathBuf::from),
    );
    dirs
}

/// Desktop entry files by id (`applications/kde/foo.desktop` → `kde-foo.desktop`);
/// earlier data dirs win. Blocking.
pub fn desktop_files() -> BTreeMap<String, PathBuf> {
    fn walk(root: &Path, dir: &Path, depth: u32, out: &mut BTreeMap<String, PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if path.is_dir() {
                if depth < 3 {
                    walk(root, &path, depth + 1, out);
                }
            } else if path.extension().is_some_and(|x| x == "desktop") {
                let Ok(rel) = path.strip_prefix(root) else {
                    continue;
                };
                let id = rel.to_string_lossy().replace('/', "-");
                out.entry(id).or_insert(path);
            }
        }
    }
    let mut out = BTreeMap::new();
    for d in data_dirs() {
        let root = d.join("applications");
        walk(&root, &root, 0, &mut out);
    }
    out
}

/// Value of `key` in the `[Desktop Entry]` group of a desktop file.
pub fn desktop_entry_value<'a>(text: &'a str, key: &str) -> Option<&'a str> {
    let mut in_entry = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_entry = t == "[Desktop Entry]";
            continue;
        }
        if !in_entry {
            continue;
        }
        if let Some((k, v)) = t.split_once('=')
            && k.trim() == key
        {
            return Some(v.trim());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_field_codes() {
        assert_eq!(
            command_from_exec("/usr/lib/firefox/firefox %u"),
            "/usr/lib/firefox/firefox"
        );
        assert_eq!(command_from_exec("kitty"), "kitty");
        assert_eq!(command_from_exec("app --name %F --x"), "app --name --x");
        assert_eq!(command_from_exec("app --pct 50%%"), "app --pct 50%");
    }

    #[test]
    fn reads_desktop_entry_keys() {
        let text = "[Desktop Entry]\nName=Example Terminal\nCategories=System;TerminalEmulator;\n\n\
                    [Desktop Action new]\nCategories=Other\n";
        assert_eq!(
            desktop_entry_value(text, "Categories"),
            Some("System;TerminalEmulator;")
        );
        assert_eq!(desktop_entry_value(text, "Hidden"), None);
        assert_eq!(desktop_id("kitty"), "kitty.desktop");
        assert_eq!(desktop_id("kitty.desktop"), "kitty.desktop");
    }
}
