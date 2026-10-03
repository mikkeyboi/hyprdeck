//! Process helpers. All functions here block; call them from [`crate::rt::blocking`]
//! or tokio tasks, never directly on the GTK main thread.

use std::ffi::OsStr;
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};

/// Captured result of a finished command.
#[derive(Debug, Clone)]
pub struct Output {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    pub fn ok(&self) -> bool {
        self.status == 0
    }
}

/// Run `program args…`, capture output, and return it regardless of exit status.
pub fn output<I, S>(program: &str, args: I) -> Result<Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("failed to spawn {program}"))?;
    Ok(Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

/// Run a command and return stdout; non-zero exit is an error carrying stderr.
pub fn run<I, S>(program: &str, args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let out = output(program, args)?;
    if !out.ok() {
        let msg = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        bail!("{program} exited with {}: {msg}", out.status);
    }
    Ok(out.stdout)
}

/// Run a command and parse its stdout as JSON.
pub fn json<T, I, S>(program: &str, args: I) -> Result<T>
where
    T: serde::de::DeserializeOwned,
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let stdout = run(program, args)?;
    serde_json::from_str(&stdout).with_context(|| format!("{program}: unexpected JSON output"))
}

/// Start a detached process (no output capture, not waited on).
pub fn spawn_detached<I, S>(program: &str, args: I) -> Result<()>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to spawn {program}"))?;
    Ok(())
}

/// Open a terminal running an interactive shell command (e.g. `paru -Syu`) that
/// waits for Enter before closing so the user can read the result.
pub fn spawn_in_terminal(title: &str, shell_cmd: &str) -> Result<()> {
    let script = format!(
        "{shell_cmd}; status=$?; echo; echo \"[exit $status] Press Enter to close\"; read -r _"
    );
    let term = terminal().context("no terminal emulator found (set $TERMINAL)")?;
    let mut args: Vec<&str> = Vec::new();
    match term.kind {
        TermKind::Kitty | TermKind::Foot | TermKind::Alacritty | TermKind::Ghostty => {
            args.extend(["--title", title, "-e"]);
        }
        TermKind::Wezterm => args.extend(["start", "--"]),
        TermKind::GnomeTerminal => args.extend(["--title", title, "--"]),
        TermKind::XdgTerminalExec => args.push("--"),
        TermKind::Konsole | TermKind::Other => args.push("-e"),
    }
    args.extend(["bash", "-lc", &script]);
    spawn_detached(&term.program, args)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TermKind {
    XdgTerminalExec,
    Kitty,
    Foot,
    Alacritty,
    Ghostty,
    Wezterm,
    Konsole,
    GnomeTerminal,
    Other,
}

#[derive(Debug, Clone)]
pub struct Terminal {
    pub program: String,
    pub kind: TermKind,
}

fn term_kind(program: &str) -> TermKind {
    match std::path::Path::new(program)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(program)
    {
        "xdg-terminal-exec" => TermKind::XdgTerminalExec,
        "kitty" => TermKind::Kitty,
        "foot" | "footclient" => TermKind::Foot,
        "alacritty" => TermKind::Alacritty,
        "ghostty" => TermKind::Ghostty,
        "wezterm" => TermKind::Wezterm,
        "konsole" => TermKind::Konsole,
        "gnome-terminal" | "kgx" | "ptyxis" => TermKind::GnomeTerminal,
        _ => TermKind::Other,
    }
}

/// The user's terminal: `$TERMINAL`, then `xdg-terminal-exec`, then common emulators on PATH.
pub fn terminal() -> Option<Terminal> {
    let env = std::env::var("TERMINAL")
        .ok()
        .filter(|t| !t.trim().is_empty());
    let candidates = env.iter().map(String::as_str).chain([
        "xdg-terminal-exec",
        "kitty",
        "foot",
        "alacritty",
        "ghostty",
        "wezterm",
        "konsole",
        "gnome-terminal",
        "ptyxis",
        "xfce4-terminal",
        "xterm",
    ]);
    candidates
        .filter_map(|c| which(c.split_whitespace().next()?))
        .map(|p| Terminal {
            kind: term_kind(&p),
            program: p,
        })
        .next()
}

/// Resolve a program name on `$PATH` (absolute paths are checked as-is).
pub fn which(program: &str) -> Option<String> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &std::path::Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        return is_exec(std::path::Path::new(program)).then(|| program.to_owned());
    }
    std::env::var_os("PATH")?
        .to_str()?
        .split(':')
        .map(|dir| std::path::Path::new(dir).join(program))
        .find(|p| is_exec(p))
        .map(|p| p.to_string_lossy().into_owned())
}

/// Prefix for launching apps from keybinds: `uwsm app -- ` inside a uwsm-managed
/// session (so apps get their own systemd scope), otherwise empty.
pub fn launch_prefix() -> &'static str {
    static PREFIX: std::sync::LazyLock<&'static str> = std::sync::LazyLock::new(|| {
        let uwsm =
            which("uwsm").is_some() && output("uwsm", ["check", "is-active"]).is_ok_and(|o| o.ok());
        if uwsm { "uwsm app -- " } else { "" }
    });
    &PREFIX
}

/// `systemctl --user <args…>`.
pub fn systemctl_user<I, S>(args: I) -> Result<String>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut all: Vec<std::ffi::OsString> = vec!["--user".into()];
    all.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
    run("systemctl", all)
}

/// Quote a string for safe inclusion in a POSIX shell command.
pub fn shell_quote(s: &str) -> String {
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@%+,".contains(c))
    {
        return s.to_owned();
    }
    format!("'{}'", s.replace('\'', r"'\''"))
}
