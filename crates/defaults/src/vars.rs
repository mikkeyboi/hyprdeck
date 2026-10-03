//! Hyprland launcher variables: top-level string globals in the Lua config
//! (`TERMINAL = "kitty"`) that keybinds launch (`hl.dsp.exec_cmd(TERMINAL)`).
//! Edits replace only the string literal on the variable's line, keeping
//! alignment and comments.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hyprdeck_core::hypr::managed;
use hyprdeck_core::hypr::model::{self, BindEntry, ConfigModel};
use hyprdeck_core::store;

/// A launcher variable found in the config.
#[derive(Debug, Clone, PartialEq)]
pub struct LauncherVar {
    pub name: String,
    pub value: String,
    pub file: PathBuf,
    /// 1-based line of the assignment.
    pub line: u32,
    /// Keys of the binds that launch it (`SUPER + Return`).
    pub binds: Vec<String>,
}

impl LauncherVar {
    pub fn label(&self) -> String {
        label(&self.name)
    }

    /// `config/variables.lua:3` relative to the hypr dir.
    pub fn location(&self) -> String {
        model::Source {
            file: self.file.clone(),
            line: self.line,
        }
        .display()
    }
}

/// Readable name for a variable (`FILE_MANAGER` → "File manager").
pub fn label(name: &str) -> String {
    match name {
        "TERMINAL" | "TERM" => "Terminal".into(),
        "FILE_MANAGER" | "FILEMANAGER" | "FILES" => "File manager".into(),
        "BROWSER" | "WEB_BROWSER" => "Web browser".into(),
        "EDITOR" | "TEXT_EDITOR" | "GUI_EDITOR" => "Text editor".into(),
        "CALCULATOR" => "Calculator".into(),
        _ => {
            let words = name.replace('_', " ").to_lowercase();
            let mut c = words.chars();
            c.next()
                .map(|f| f.to_uppercase().chain(c).collect())
                .unwrap_or_default()
        }
    }
}

/// Default-app category a variable mirrors (see [`crate::categories`]).
pub fn category(name: &str) -> Option<&'static str> {
    match name {
        "TERMINAL" | "TERM" => Some("terminal"),
        "FILE_MANAGER" | "FILEMANAGER" | "FILES" => Some("files"),
        "BROWSER" | "WEB_BROWSER" => Some("browser"),
        "EDITOR" | "TEXT_EDITOR" | "GUI_EDITOR" => Some("editor"),
        "MAIL" | "EMAIL" => Some("email"),
        _ => None,
    }
}

fn is_const_name(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase())
        && name
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
}

/// Byte range of the string literal's contents and its quote char on a
/// top-level (unindented) line assigning `name`, e.g. `TERMINAL     = "kitty" -- x`.
fn literal_span(line: &str, name: &str) -> Option<(usize, usize, char)> {
    let rest = line.strip_prefix(name)?;
    if rest.starts_with(|c: char| c.is_alphanumeric() || c == '_') {
        return None;
    }
    let after_eq = rest.trim_start().strip_prefix('=')?;
    if after_eq.starts_with('=') {
        return None;
    }
    let lit = after_eq.trim_start();
    let quote = lit.chars().next().filter(|&c| c == '"' || c == '\'')?;
    let start = line.len() - lit.len() + 1;
    let mut escaped = false;
    for (i, c) in line[start..].char_indices() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            c if c == quote => return Some((start, start + i, quote)),
            _ => {}
        }
    }
    None
}

fn unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            match chars.next() {
                Some('n') => out.push('\n'),
                Some('t') => out.push('\t'),
                Some(o) => out.push(o),
                None => {}
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn escape(s: &str, quote: char) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            c => out.push(c),
        }
    }
    out
}

/// Replace the string value of `name`, leaving every other byte unchanged.
pub fn set(text: &str, name: &str, value: &str) -> Result<String> {
    let mut offset = 0;
    for line in text.split_inclusive('\n') {
        if let Some((a, b, quote)) = literal_span(line, name) {
            let mut out = String::with_capacity(text.len() + value.len());
            out.push_str(&text[..offset + a]);
            out.push_str(&escape(value, quote));
            out.push_str(&text[offset + b..]);
            return Ok(out);
        }
        offset += line.len();
    }
    bail!("{name} is not assigned a string at the top level")
}

/// Top-level `NAME = "string"` assignments with constant-style names:
/// (name, value, 1-based line).
pub fn assignments(text: &str) -> Vec<(String, String, u32)> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let name: String = l
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !is_const_name(&name) {
                return None;
            }
            let (a, b, _) = literal_span(l, &name)?;
            Some((name, unescape(&l[a..b]), i as u32 + 1))
        })
        .collect()
}

/// Whether identifier `name` occurs in `code` outside string literals.
fn references(code: &str, name: &str) -> bool {
    let bytes = code.as_bytes();
    let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut quote = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(q) => {
                if b == b'\\' {
                    i += 1;
                } else if b == q {
                    quote = None;
                }
            }
            None if b == b'"' || b == b'\'' => quote = Some(b),
            None if b == b'-' && bytes.get(i + 1) == Some(&b'-') => return false,
            None if code[i..].starts_with(name)
                && (i == 0 || !ident(bytes[i - 1]))
                && bytes.get(i + name.len()).is_none_or(|&n| !ident(n)) =>
            {
                return true;
            }
            None => {}
        }
        i += 1;
    }
    false
}

/// Whether two command lines run the same program with the same arguments,
/// ignoring the program's directory (`/usr/lib/firefox/firefox` == `firefox`).
pub fn same_command(a: &str, b: &str) -> bool {
    let words = |s: &str| -> Vec<String> {
        s.split_whitespace()
            .enumerate()
            .map(|(i, w)| {
                if i == 0 {
                    w.rsplit('/').next().unwrap_or(w).to_owned()
                } else {
                    w.to_owned()
                }
            })
            .collect()
    };
    words(a) == words(b)
}

/// Whether `exec` runs `value` as a whole word sequence (`uwsm app -- kitty -e x`).
fn runs(exec: &str, value: &str) -> bool {
    let value = value.trim();
    !value.is_empty() && format!(" {exec} ").contains(&format!(" {value} "))
}

/// Lua files under the hypr config dir (not hidden, a few levels deep).
fn lua_files(dir: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for e in entries.flatten() {
            let path = e.path();
            if e.file_name().to_string_lossy().starts_with('.') {
                continue;
            }
            if path.is_dir() {
                if depth < 4 {
                    walk(&path, depth + 1, out);
                }
            } else if path.extension().is_some_and(|x| x == "lua") {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, 0, &mut out);
    out.sort();
    out
}

/// Launcher variables: string globals that an `exec_cmd` bind uses, either by
/// name in the bind's action or by running the value. `files` maps each config
/// file to its text.
pub fn find(m: &ConfigModel, files: &BTreeMap<PathBuf, String>) -> Vec<LauncherVar> {
    let mut vars: Vec<LauncherVar> = Vec::new();
    for (file, text) in files {
        for (name, value, line) in assignments(text) {
            if vars.iter().any(|v| v.name == name) || value.ends_with(char::is_whitespace) {
                continue;
            }
            vars.push(LauncherVar {
                name,
                value,
                file: file.clone(),
                line,
                binds: Vec::new(),
            });
        }
    }
    for b in &m.binds {
        let Some(exec) = &b.exec else { continue };
        let action = files
            .get(&b.source.file)
            .and_then(|t| t.lines().nth((b.source.line as usize).saturating_sub(1)))
            .and_then(|l| l.find("exec_cmd").map(|i| &l[i..]))
            .unwrap_or("");
        for v in &mut vars {
            if (references(action, &v.name) || runs(exec, &v.value)) && !v.binds.contains(&b.keys) {
                v.binds.push(b.keys.clone());
            }
        }
    }
    vars.retain(|v| !v.binds.is_empty());
    vars
}

/// Discover launcher variables in the user's config. Blocking.
pub fn discover(m: &ConfigModel) -> Vec<LauncherVar> {
    let managed = managed::lua_path();
    let files: BTreeMap<PathBuf, String> = lua_files(&model::hypr_dir())
        .into_iter()
        .filter(|p| *p != managed)
        .filter_map(|p| std::fs::read_to_string(&p).ok().map(|t| (p, t)))
        .collect();
    find(m, &files)
}

/// Write one value into the variable's file. Blocking.
pub fn write(var: &LauncherVar, value: &str) -> Result<()> {
    // Follow symlinks (dotfile managers) so the link itself is kept.
    let path = std::fs::canonicalize(&var.file).unwrap_or_else(|_| var.file.clone());
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let new = set(&text, &var.name, value)?;
    if new != text {
        store::write_atomic(&path, new.as_bytes())?;
    }
    Ok(())
}

/// A hand-written bind launching `value` that a later bind replaced.
#[derive(Debug)]
pub struct Shadowed<'a> {
    pub original: &'a BindEntry,
    pub active: Option<&'a BindEntry>,
}

/// Hand-written launcher binds for `value` (e.g. `uwsm app -- firefox`) that
/// are unbound later — typically by a hyprdeck/HyprMod managed bind.
pub fn shadowed<'a>(m: &'a ConfigModel, value: &str) -> Vec<Shadowed<'a>> {
    if value.trim().is_empty() {
        return Vec::new();
    }
    let effective = m.effective_binds();
    m.binds
        .iter()
        .filter(|b| {
            b.exec
                .as_deref()
                .is_some_and(|e| e == value || e.ends_with(&format!(" {value}")))
        })
        .filter(|b| !effective.iter().any(|e| e.seq == b.seq))
        .map(|b| Shadowed {
            original: b,
            active: effective
                .iter()
                .copied()
                .filter(|e| e.submap == b.submap && e.combo.matches(&b.combo))
                .max_by_key(|e| e.seq),
        })
        .collect()
}

/// Whether a bind comes from a file managed by hyprdeck (or HyprMod before it).
pub fn is_managed(b: &BindEntry) -> bool {
    b.source.file == managed::lua_path()
        || b.source.file == model::hypr_dir().join("hyprland-gui.lua")
}

#[cfg(test)]
mod tests {
    use hyprdeck_core::hypr::model::{Combo, Source};

    use super::*;

    /// Current value of `name` (first top-level assignment).
    fn get(text: &str, name: &str) -> Option<String> {
        text.lines()
            .find_map(|l| literal_span(l, name).map(|(a, b, _)| unescape(&l[a..b])))
    }

    const TEXT: &str = "-- Default apps\n\nTERMINAL     = \"kitty\"\nFILE_MANAGER = \"dolphin\" -- files\n\
                        BROWSER      = 'firefox'\nEDITOR       = \"gnome-text-editor --new-window\"\n\
                        CALCULATOR   = \"gnome-calculator\"\nTERMINAL_X = \"no\"\n\n-- Monitors\nMONITOR1 = \"\"\n\
                        local prefix = \"app -- \"\n  INDENTED = \"x\"\nPREFIX = \"run \"\n";

    #[test]
    fn reads_values() {
        assert_eq!(get(TEXT, "TERMINAL").as_deref(), Some("kitty"));
        assert_eq!(get(TEXT, "BROWSER").as_deref(), Some("firefox"));
        assert_eq!(
            get(TEXT, "EDITOR").as_deref(),
            Some("gnome-text-editor --new-window")
        );
        assert_eq!(get(TEXT, "MONITOR1").as_deref(), Some(""));
        assert_eq!(get(TEXT, "NOPE"), None);
        assert_eq!(get(TEXT, "INDENTED"), None);
    }

    #[test]
    fn replaces_only_the_literal() {
        let out = set(TEXT, "FILE_MANAGER", "thunar").unwrap();
        assert_eq!(
            out,
            TEXT.replace("\"dolphin\" -- files", "\"thunar\" -- files")
        );
        let out = set(TEXT, "TERMINAL", "foot").unwrap();
        assert!(out.contains("TERMINAL     = \"foot\"\n"));
        assert!(out.contains("TERMINAL_X = \"no\""));
    }

    #[test]
    fn escapes_and_round_trips() {
        let out = set(TEXT, "BROWSER", "it's \"x\"").unwrap();
        assert!(out.contains(r#"BROWSER      = 'it\'s "x"'"#));
        assert_eq!(get(&out, "BROWSER").as_deref(), Some("it's \"x\""));
        assert_eq!(set(&out, "BROWSER", "firefox").unwrap(), TEXT);
        assert!(set(TEXT, "NOPE", "x").is_err());
    }

    #[test]
    fn lists_constant_string_globals() {
        let names: Vec<String> = assignments(TEXT).into_iter().map(|(n, _, _)| n).collect();
        assert_eq!(
            names,
            [
                "TERMINAL",
                "FILE_MANAGER",
                "BROWSER",
                "EDITOR",
                "CALCULATOR",
                "TERMINAL_X",
                "MONITOR1",
                "PREFIX"
            ]
        );
        assert_eq!(assignments(TEXT)[0], ("TERMINAL".into(), "kitty".into(), 3));
    }

    #[test]
    fn identifier_references() {
        assert!(references("exec_cmd(prefix .. TERMINAL)", "TERMINAL"));
        assert!(!references("exec_cmd(prefix .. TERMINAL_X)", "TERMINAL"));
        assert!(!references("exec_cmd(\"TERMINAL\")", "TERMINAL"));
        assert!(!references("exec_cmd(x) -- TERMINAL", "TERMINAL"));
        assert!(runs("app -- kitty -e btop", "kitty"));
        assert!(!runs("kittyx", "kitty"));
        assert!(!runs("anything", ""));
        assert!(same_command("/usr/lib/firefox/firefox", "firefox"));
        assert!(same_command(
            "gnome-text-editor  --new-window",
            "gnome-text-editor --new-window"
        ));
        assert!(!same_command("kitty +open", "kitty"));
    }

    fn bind(seq: u64, keys: &str, exec: &str, line: u32) -> BindEntry {
        BindEntry {
            seq,
            keys: keys.into(),
            combo: Combo::parse(keys),
            dispatcher: Some("exec_cmd".into()),
            action_lua: format!("hl.dsp.exec_cmd(\"{exec}\")"),
            exec: Some(exec.into()),
            opts_lua: String::new(),
            description: None,
            submap: String::new(),
            source: Source {
                file: PathBuf::from("/cfg/binds.lua"),
                line,
            },
        }
    }

    #[test]
    fn finds_vars_used_by_exec_binds() {
        let binds_lua = "local mod = \"SUPER\"\n\
                         hl.bind(mod .. \" + Return\", hl.dsp.exec_cmd(prefix .. TERMINAL))\n\
                         hl.bind(mod .. \" + W\",\n    hl.dsp.exec_cmd(BROWSER))\n\
                         hl.bind(MONITOR1 .. \" + 1\", hl.dsp.exec_cmd(\"x\"))\n";
        let files = BTreeMap::from([
            (PathBuf::from("/cfg/binds.lua"), binds_lua.to_owned()),
            (PathBuf::from("/cfg/variables.lua"), TEXT.to_owned()),
        ]);
        let m = ConfigModel {
            binds: vec![
                bind(1, "SUPER + Return", "app -- kitty", 2),
                // Multi-line call: the recorded line has no exec_cmd; matched by value.
                bind(2, "SUPER + W", "firefox", 3),
                bind(3, "SUPER + 1", "x", 5),
            ],
            ..Default::default()
        };
        let vars = find(&m, &files);
        let got: Vec<(&str, &str, u32, Vec<String>)> = vars
            .iter()
            .map(|v| (v.name.as_str(), v.value.as_str(), v.line, v.binds.clone()))
            .collect();
        assert_eq!(
            got,
            [
                ("TERMINAL", "kitty", 3, vec!["SUPER + Return".to_owned()]),
                ("BROWSER", "firefox", 5, vec!["SUPER + W".to_owned()]),
            ]
        );
        assert_eq!(label("FILE_MANAGER"), "File manager");
        assert_eq!(label("SCREENSHOT_TOOL"), "Screenshot tool");
        assert_eq!(category("BROWSER"), Some("browser"));
    }
}
