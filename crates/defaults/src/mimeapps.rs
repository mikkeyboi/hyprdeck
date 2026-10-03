//! The user's MIME default associations (`~/.config/mimeapps.list`, per the
//! XDG mime-apps spec). Edits touch only the `[Default Applications]` lines of
//! the types being changed; every other byte of the file is preserved, so
//! setting a type and resetting it again restores the original file exactly.

use std::path::PathBuf;

use anyhow::{Context, Result};
use hyprdeck_core::store;

const DEFAULTS: &str = "Default Applications";

fn group_name(trimmed: &str) -> Option<&str> {
    trimmed.strip_prefix('[')?.strip_suffix(']')
}

/// Key of a `key=value` line (`None` for comments, blanks and group headers).
fn line_key(line: &str) -> Option<&str> {
    let t = line.trim_start();
    if t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    t.split_once('=').map(|(k, _)| k.trim())
}

/// `line` with its value replaced by `app`, keeping the key's spacing, a
/// trailing `;` list terminator and the line ending.
fn replace_value(line: &str, app: &str) -> String {
    let (body, eol) = match line.strip_suffix('\n') {
        Some(b) => b.strip_suffix('\r').map_or((b, "\n"), |b| (b, "\r\n")),
        None => (line, ""),
    };
    let eq = body.find('=').map_or(body.len(), |i| i + 1);
    let value = &body[eq..];
    let lead = value.len() - value.trim_start().len();
    let semi = if value.trim_end().ends_with(';') {
        ";"
    } else {
        ""
    };
    format!("{}{app}{semi}{eol}", &body[..eq + lead])
}

/// First app listed as the default for `mime`.
pub fn get_default<'a>(text: &'a str, mime: &str) -> Option<&'a str> {
    let mut group = "";
    for line in text.lines() {
        let t = line.trim();
        if let Some(g) = group_name(t) {
            group = g;
        } else if group == DEFAULTS && line_key(line) == Some(mime) {
            let value = t.split_once('=')?.1;
            return value.split(';').map(str::trim).find(|s| !s.is_empty());
        }
    }
    None
}

/// Make `app` the default for `mime`: rewrite its existing line in place, or
/// add a line at the end of `[Default Applications]` (creating the group).
pub fn set_default(text: &str, mime: &str, app: &str) -> String {
    let mut out = String::with_capacity(text.len() + mime.len() + app.len() + 2);
    let mut group = "";
    let mut done = false;
    // Byte offset in `out` just after the last non-blank line of the group.
    let mut insert_at = None;
    for line in text.split_inclusive('\n') {
        let t = line.trim();
        if let Some(g) = group_name(t) {
            group = g;
        } else if group == DEFAULTS && line_key(line) == Some(mime) {
            // Later duplicates are dropped so the result is unambiguous.
            if !done {
                out.push_str(&replace_value(line, app));
                insert_at = Some(out.len());
                done = true;
            }
            continue;
        }
        out.push_str(line);
        if group == DEFAULTS && !t.is_empty() {
            insert_at = Some(out.len());
        }
    }
    if done {
        return out;
    }
    let entry = format!("{mime}={app}\n");
    match insert_at {
        Some(pos) => {
            if pos == out.len() && !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
                out.push_str(&entry);
            } else {
                out.insert_str(pos, &entry);
            }
        }
        None => {
            if !out.is_empty() {
                if !out.ends_with('\n') {
                    out.push('\n');
                }
                if !out.ends_with("\n\n") {
                    out.push('\n');
                }
            }
            out.push_str(&format!("[{DEFAULTS}]\n"));
            out.push_str(&entry);
        }
    }
    out
}

/// Drop the `[Default Applications]` line(s) for `mime`.
pub fn remove_default(text: &str, mime: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut group = "";
    for line in text.split_inclusive('\n') {
        if let Some(g) = group_name(line.trim()) {
            group = g;
        } else if group == DEFAULTS && line_key(line) == Some(mime) {
            continue;
        }
        out.push_str(line);
    }
    out
}

fn config_home() -> PathBuf {
    dirs::config_dir().unwrap_or_else(|| store::home().join(".config"))
}

/// `~/.config/mimeapps.list`, where new defaults are written.
pub fn user_path() -> PathBuf {
    config_home().join("mimeapps.list")
}

/// Desktop-specific lists (`~/.config/hyprland-mimeapps.list`) that exist;
/// they take precedence over `mimeapps.list`.
fn desktop_paths() -> Vec<PathBuf> {
    let desktops = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    desktops
        .split(':')
        .filter(|d| !d.is_empty())
        .map(|d| config_home().join(format!("{}-mimeapps.list", d.to_lowercase())))
        .filter(|p| p.is_file())
        .collect()
}

/// Resolve symlinks (dotfile managers) so the link itself is kept.
fn real_path(path: PathBuf) -> PathBuf {
    std::fs::canonicalize(&path).unwrap_or(path)
}

fn read(path: &PathBuf) -> Result<String> {
    match std::fs::read_to_string(path) {
        Ok(t) => Ok(t),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

fn edit(path: PathBuf, f: impl Fn(&str) -> String) -> Result<()> {
    let path = real_path(path);
    let text = read(&path)?;
    let new = f(&text);
    if new != text {
        store::write_atomic(&path, new.as_bytes())?;
    }
    Ok(())
}

/// Make `app` the default for every type in `mimes`. Desktop-specific lists
/// that already name a default for one of the types are updated too, since
/// they would otherwise win. Blocking.
pub fn write_defaults(mimes: &[&str], app: &str) -> Result<()> {
    for path in desktop_paths() {
        edit(path, |text| {
            mimes.iter().fold(text.to_owned(), |t, m| {
                if get_default(&t, m).is_some() {
                    set_default(&t, m, app)
                } else {
                    t
                }
            })
        })?;
    }
    edit(user_path(), |text| {
        mimes
            .iter()
            .fold(text.to_owned(), |t, m| set_default(&t, m, app))
    })
}

/// Remove the user's default for every type in `mimes`, falling back to the
/// system-wide choice. Blocking.
pub fn reset_defaults(mimes: &[&str]) -> Result<()> {
    for path in desktop_paths().into_iter().chain([user_path()]) {
        if !path.exists() {
            continue;
        }
        edit(path, |text| {
            mimes
                .iter()
                .fold(text.to_owned(), |t, m| remove_default(&t, m))
        })?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "[Default Applications]\nx-scheme-handler/http=firefox.desktop\n\
                        application/zip=org.example.Archiver.desktop;\n\n\
                        [Added Associations]\napplication/zip=org.example.Archiver.desktop;other.desktop;\n";

    #[test]
    fn reads_first_default() {
        assert_eq!(
            get_default(LIST, "application/zip"),
            Some("org.example.Archiver.desktop")
        );
        assert_eq!(
            get_default(LIST, "x-scheme-handler/http"),
            Some("firefox.desktop")
        );
        assert_eq!(get_default(LIST, "image/png"), None);
    }

    #[test]
    fn replaces_in_place_and_round_trips() {
        let out = set_default(LIST, "x-scheme-handler/http", "chromium.desktop");
        assert_eq!(
            out,
            LIST.replace("http=firefox.desktop", "http=chromium.desktop")
        );
        assert_eq!(
            set_default(&out, "x-scheme-handler/http", "firefox.desktop"),
            LIST
        );
        // List-style values keep their terminator; Added Associations untouched.
        let out = set_default(LIST, "application/zip", "x.desktop");
        assert!(out.contains("[Default Applications]\nx-scheme-handler/http=firefox.desktop\napplication/zip=x.desktop;\n"));
        assert!(out.ends_with("application/zip=org.example.Archiver.desktop;other.desktop;\n"));
    }

    #[test]
    fn adds_then_removes_exactly() {
        let out = set_default(LIST, "image/png", "org.example.Viewer.desktop");
        assert!(out.contains("application/zip=org.example.Archiver.desktop;\nimage/png=org.example.Viewer.desktop\n\n[Added"));
        assert_eq!(remove_default(&out, "image/png"), LIST);
    }

    #[test]
    fn creates_group_when_missing() {
        assert_eq!(
            set_default("", "image/png", "a.desktop"),
            "[Default Applications]\nimage/png=a.desktop\n"
        );
        let added = "[Added Associations]\nimage/png=a.desktop;\n";
        let out = set_default(added, "image/png", "a.desktop");
        assert_eq!(
            out,
            format!("{added}\n[Default Applications]\nimage/png=a.desktop\n")
        );
        // No trailing newline: the new line still starts on its own line.
        let out = set_default(
            "[Default Applications]\ntext/plain=a.desktop",
            "image/png",
            "b.desktop",
        );
        assert_eq!(
            out,
            "[Default Applications]\ntext/plain=a.desktop\nimage/png=b.desktop\n"
        );
    }

    #[test]
    fn only_touches_the_defaults_group() {
        let out = remove_default(LIST, "application/zip");
        assert!(!out.contains("application/zip=org.example.Archiver.desktop;\n\n"));
        assert!(out.contains("[Added Associations]\napplication/zip="));
        assert_eq!(remove_default(LIST, "image/png"), LIST);
    }
}
