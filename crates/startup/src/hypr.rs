//! Login commands from the Hyprland Lua config (`hl.exec_cmd` inside
//! `hl.on("hyprland.start", …)`). Disabling comments out the single source line
//! with [`MARKER`]; enabling removes it again.

use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyprdeck_core::hypr::{ctl, model};
use hyprdeck_core::{cmd, store};

pub const MARKER: &str = "-- [hyprdeck disabled] ";

#[derive(Debug, Clone)]
pub struct HyprExec {
    pub command: String,
    pub file: PathBuf,
    /// 1-based.
    pub line: u32,
    pub enabled: bool,
    /// Why the line can't be toggled (None = editable).
    pub readonly: Option<String>,
    /// First real program of the command (basename).
    pub program: String,
    pub running: bool,
}

impl HyprExec {
    /// `config/autostart.lua:6`.
    pub fn location(&self) -> String {
        model::Source {
            file: self.file.clone(),
            line: self.line,
        }
        .display()
    }
    /// Stable-ish CLI id.
    pub fn id(&self) -> String {
        format!("hypr:{}", self.program)
    }
    /// Warning shown before disabling a command the session depends on.
    pub fn essential(&self) -> Option<String> {
        essential(&self.command)
    }
}

/// What a session-critical startup command does, phrased for a confirmation
/// dialog; `None` for ordinary commands.
pub fn essential(command: &str) -> Option<String> {
    let program = program_of(command);
    let words = || command.split_whitespace();
    let env = "It passes your Wayland session environment to systemd and D-Bus. Without it, portals, screen sharing, file pickers and apps started by systemd may break after the next login.";
    let named = |name: &str, what: &str| Some(format!("{name} {what}"));
    match program.as_str() {
        "dbus-update-activation-environment" => Some(env.to_owned()),
        "systemctl" if words().any(|w| w == "import-environment") => Some(env.to_owned()),
        "uwsm" if words().any(|w| w == "finalize") => Some(env.to_owned()),
        "noctalia" | "noctalia-shell" => named("Noctalia", SHELL),
        "quickshell" | "qs" => named("Quickshell", SHELL),
        "hyprpanel" => named("HyprPanel", SHELL),
        "ags" | "astal" => named("AGS", SHELL),
        "waybar" => named(
            "Waybar",
            "draws your status bar and system tray. Without it you have neither after the next login.",
        ),
        "swaync" => named("SwayNC", NOTIFICATIONS),
        "mako" => named("mako", NOTIFICATIONS),
        "dunst" => named("Dunst", NOTIFICATIONS),
        "hyprpaper" | "swww" | "swww-daemon" | "swaybg" => named(
            &program,
            "draws your wallpaper. Without it the desktop background stays empty after the next login.",
        ),
        "hypridle" => named(
            "hypridle",
            "locks the screen and turns displays off when you are away. Without it nothing happens on idle after the next login.",
        ),
        p if p.contains("polkit") => named(
            p,
            "is the authentication agent that asks for your password when an app needs administrator rights. Without it those requests fail after the next login.",
        ),
        _ => None,
    }
}

const SHELL: &str = "is your desktop shell: it draws your bar and may also provide notifications, the launcher and the lock screen. Without it these are missing after the next login.";
const NOTIFICATIONS: &str =
    "shows your notifications. Without it apps can't show notifications after the next login.";

/// Launchers that run the command after `--` (`uwsm app -- foo`).
const LAUNCHERS: [&str; 3] = ["uwsm", "uwsm-app", "app2unit"];

/// Arguments of a line that is exactly one `hl.exec_cmd(…)` statement
/// (optionally followed by `;` and/or a `--` comment).
pub fn single_exec(line: &str) -> Option<&str> {
    let rest = line
        .trim()
        .strip_prefix("hl.exec_cmd")?
        .trim_start()
        .strip_prefix('(')?;
    let bytes = rest.as_bytes();
    let mut depth = 1u32;
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            q @ (b'"' | b'\'') => {
                i += 1;
                while i < bytes.len() && bytes[i] != q {
                    if bytes[i] == b'\\' {
                        i += 1;
                    }
                    i += 1;
                }
                if i >= bytes.len() {
                    return None;
                }
            }
            b'[' if matches!(bytes.get(i + 1), Some(b'[' | b'=')) => {
                let eq = bytes[i + 1..].iter().take_while(|&&b| b == b'=').count();
                if bytes.get(i + 1 + eq) != Some(&b'[') {
                    i += 1;
                    continue;
                }
                let close = format!("]{}]", "=".repeat(eq));
                let start = i + 2 + eq;
                i = start + rest[start..].find(&close)? + close.len() - 1;
            }
            b'-' if bytes.get(i + 1) == Some(&b'-') => return None,
            b'(' | b'{' => depth += 1,
            b')' | b'}' => {
                depth -= 1;
                if depth == 0 {
                    let tail = rest[i + 1..].trim_start();
                    let tail = tail.strip_prefix(';').unwrap_or(tail).trim_start();
                    return (tail.is_empty() || tail.starts_with("--")).then(|| rest[..i].trim());
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// Value of a lone Lua string literal (`"…"` / `'…'`).
pub fn string_literal(expr: &str) -> Option<String> {
    let q = expr.chars().next().filter(|c| *c == '"' || *c == '\'')?;
    let inner = expr.strip_prefix(q)?.strip_suffix(q)?;
    let mut out = String::with_capacity(inner.len());
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        if c == q {
            return None;
        }
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            't' => out.push('\t'),
            o => out.push(o),
        }
    }
    Some(out)
}

/// Program basename of a shell command, skipping `VAR=x`, `env` and launcher
/// wrappers such as `uwsm app --`.
pub fn program_of(command: &str) -> String {
    let base = |w: &str| {
        w.trim_matches(['"', '\''])
            .rsplit('/')
            .next()
            .unwrap_or(w)
            .to_owned()
    };
    let mut words = command
        .split_whitespace()
        .skip_while(|w| w.contains('=') || *w == "env" || w.starts_with('-'));
    let Some(first) = words.next().map(base) else {
        return String::new();
    };
    if LAUNCHERS.contains(&first.as_str()) {
        let rest: Vec<&str> = words.collect();
        if let Some(i) = rest.iter().position(|w| *w == "--") {
            return program_of(&rest[i + 1..].join(" "));
        }
    }
    first
}

fn lua_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            lua_files(&p, out);
        } else if p.extension().is_some_and(|x| x == "lua") {
            out.push(p);
        }
    }
}

/// All `.lua` files under the Hyprland config dir.
pub fn config_files() -> Vec<PathBuf> {
    let mut out = Vec::new();
    lua_files(&model::hypr_dir(), &mut out);
    out.sort();
    out
}

/// Startup commands (active ones from the evaluated config, disabled ones from
/// the marker), in file/line order, plus config evaluation errors.
pub fn scan() -> (Vec<HyprExec>, Vec<String>) {
    let m = model::load();
    let managed = model::hypr_dir().join("hyprdeck.lua");
    let mut texts: HashMap<PathBuf, Vec<String>> = HashMap::new();
    let mut per_line: HashMap<(PathBuf, u32), usize> = HashMap::new();
    for e in m.startup_execs() {
        *per_line
            .entry((e.source.file.clone(), e.source.line))
            .or_default() += 1;
    }
    let procs = processes();
    let mut out = Vec::new();
    for e in m.startup_execs() {
        let lines = texts.entry(e.source.file.clone()).or_insert_with(|| {
            std::fs::read_to_string(&e.source.file)
                .map(|t| t.lines().map(str::to_owned).collect())
                .unwrap_or_default()
        });
        let text = lines
            .get((e.source.line as usize).saturating_sub(1))
            .map_or("", String::as_str);
        let readonly = if e.source.file == managed {
            Some("Generated by hyprdeck".to_owned())
        } else if per_line[&(e.source.file.clone(), e.source.line)] > 1 {
            Some("Runs several commands from one line".to_owned())
        } else if single_exec(text).is_none() {
            Some("Not a plain hl.exec_cmd(…) line".to_owned())
        } else {
            None
        };
        let program = program_of(&e.cmd);
        out.push(HyprExec {
            running: is_running(&procs, &program),
            command: e.cmd.clone(),
            file: e.source.file.clone(),
            line: e.source.line,
            enabled: true,
            readonly,
            program,
        });
    }
    for file in config_files() {
        let Ok(text) = std::fs::read_to_string(&file) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let Some(stmt) = line.trim_start().strip_prefix(MARKER) else {
                continue;
            };
            let Some(args) = single_exec(stmt) else {
                continue;
            };
            let command = string_literal(args).unwrap_or_else(|| args.to_owned());
            let program = program_of(&command);
            out.push(HyprExec {
                running: is_running(&procs, &program),
                command,
                file: file.clone(),
                line: i as u32 + 1,
                enabled: false,
                readonly: None,
                program,
            });
        }
    }
    out.sort_by(|a, b| (&a.file, a.line).cmp(&(&b.file, b.line)));
    (out, m.errors)
}

/// Comment out (`enabled = false`) or restore the exec line `file:line`.
pub fn set_enabled(file: &Path, line: u32, enabled: bool) -> Result<()> {
    let text =
        std::fs::read_to_string(file).with_context(|| format!("reading {}", file.display()))?;
    let mut lines: Vec<&str> = text.split_inclusive('\n').collect();
    let idx = (line as usize)
        .checked_sub(1)
        .filter(|&i| i < lines.len())
        .context("line no longer exists")?;
    let new = toggle_line(lines[idx], enabled)?;
    lines[idx] = &new;
    store::write_atomic(file, lines.concat().as_bytes())
}

/// Pure line transformation behind [`set_enabled`].
pub fn toggle_line(line: &str, enabled: bool) -> Result<String> {
    let indent_len = line.len() - line.trim_start().len();
    let (indent, body) = line.split_at(indent_len);
    if enabled {
        let Some(stmt) = body.strip_prefix(MARKER) else {
            bail!("line is not disabled by hyprdeck: {}", line.trim())
        };
        Ok(format!("{indent}{stmt}"))
    } else {
        if single_exec(body.trim_end()).is_none() {
            bail!(
                "line is not a single hl.exec_cmd(…) statement: {}",
                line.trim()
            );
        }
        Ok(format!("{indent}{MARKER}{body}"))
    }
}

/// Own processes as `(pid, argv0 basename, comm)`.
fn processes() -> Vec<(u32, String, String)> {
    let uid = std::fs::metadata("/proc/self")
        .map(|m| m.uid())
        .unwrap_or(u32::MAX);
    let Ok(rd) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let pid: u32 = e.file_name().to_str()?.parse().ok()?;
            if e.metadata().ok()?.uid() != uid {
                return None;
            }
            let dir = e.path();
            let cmdline = std::fs::read(dir.join("cmdline")).ok()?;
            let argv0 = cmdline.split(|&b| b == 0).next().unwrap_or_default();
            let argv0 = String::from_utf8_lossy(argv0);
            let base = argv0.rsplit('/').next().unwrap_or_default().to_owned();
            let comm = std::fs::read_to_string(dir.join("comm"))
                .unwrap_or_default()
                .trim_end()
                .to_owned();
            Some((pid, base, comm))
        })
        .collect()
}

fn matches_program(base: &str, comm: &str, program: &str) -> bool {
    !program.is_empty()
        && (base == program || comm == program.get(..program.len().min(15)).unwrap_or(program))
}

fn is_running(procs: &[(u32, String, String)], program: &str) -> bool {
    procs.iter().any(|(_, b, c)| matches_program(b, c, program))
}

fn pids_of(program: &str) -> Vec<u32> {
    let me = std::process::id();
    processes()
        .into_iter()
        .filter(|(pid, b, c)| *pid != me && matches_program(b, c, program))
        .map(|(p, ..)| p)
        .collect()
}

/// Launch the command now the way Hyprland does at login.
pub fn start(command: &str) -> Result<()> {
    ctl::dispatch(&format!("hl.dsp.exec_cmd({})", ctl::lua_str(command)))
}

/// SIGTERM every own process running `program`; waits up to 3 s for exit.
pub fn stop(program: &str) -> Result<()> {
    let pids = pids_of(program);
    if pids.is_empty() {
        bail!("{program} is not running");
    }
    let mut args = vec!["-TERM".to_owned()];
    args.extend(pids.iter().map(u32::to_string));
    cmd::run("kill", &args)?;
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline && !pids_of(program).is_empty() {
        std::thread::sleep(Duration::from_millis(100));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_single_exec_lines() {
        assert_eq!(
            single_exec(r#"    hl.exec_cmd("waybar")"#),
            Some(r#""waybar""#)
        );
        assert_eq!(
            single_exec(r#"hl.exec_cmd("a (b)") ; -- note"#),
            Some(r#""a (b)""#)
        );
        assert_eq!(single_exec(r#"hl.exec_cmd('x)\'y')"#), Some(r#"'x)\'y'"#));
        assert_eq!(single_exec(r#"hl.exec_cmd([[a")]])"#), Some(r#"[[a")]]"#));
        assert_eq!(
            single_exec(r#"hl.exec_cmd(cmd .. "x", { a = 1 })"#),
            Some(r#"cmd .. "x", { a = 1 }"#)
        );
        assert_eq!(single_exec(r#"hl.exec_cmd("a") hl.exec_cmd("b")"#), None);
        assert_eq!(single_exec(r#"hl.exec_cmd("a"); x = 1"#), None);
        assert_eq!(single_exec(r#"if x then hl.exec_cmd("a") end"#), None);
        assert_eq!(single_exec(r#"hl.exec_cmd("a""#), None);
        assert_eq!(single_exec(r#"hl.exec_cmd(-- c"#), None);
    }

    #[test]
    fn literals_and_programs() {
        assert_eq!(
            string_literal(r#""xhost +SI:localuser:root""#).unwrap(),
            "xhost +SI:localuser:root"
        );
        assert_eq!(string_literal(r#"'a\'b'"#).unwrap(), "a'b");
        assert!(string_literal(r#"cmd .. "x""#).is_none());
        assert!(string_literal(r#""a" .. "b""#).is_none());
        assert_eq!(
            program_of("dbus-update-activation-environment --systemd --all"),
            "dbus-update-activation-environment"
        );
        assert_eq!(program_of("env FOO=1 /usr/bin/qs -c x"), "qs");
        assert_eq!(program_of("uwsm app -s b -- waybar -c x"), "waybar");
        assert_eq!(program_of("uwsm finalize"), "uwsm");
    }

    #[test]
    fn essential_commands() {
        assert!(essential("dbus-update-activation-environment --systemd --all").is_some());
        assert!(essential("systemctl --user import-environment WAYLAND_DISPLAY").is_some());
        assert!(essential("systemctl --user start foo.service").is_none());
        assert!(
            essential("uwsm app -- waybar")
                .unwrap()
                .starts_with("Waybar ")
        );
        assert!(essential("qs -c shell").unwrap().starts_with("Quickshell "));
        assert!(
            essential("/usr/lib/polkit-gnome/polkit-gnome-authentication-agent-1")
                .unwrap()
                .starts_with("polkit-gnome-")
        );
        assert!(essential("swww-daemon").is_some());
        assert!(essential("firefox").is_none());
    }

    #[test]
    fn toggle_round_trip() {
        let line = "    hl.exec_cmd(\"waybar\")\n";
        let off = toggle_line(line, false).unwrap();
        assert_eq!(off, "    -- [hyprdeck disabled] hl.exec_cmd(\"waybar\")\n");
        assert_eq!(toggle_line(&off, true).unwrap(), line);
        assert!(toggle_line(line, true).is_err());
        assert!(toggle_line("for _, c in ipairs(x) do hl.exec_cmd(c) end\n", false).is_err());
    }
}
