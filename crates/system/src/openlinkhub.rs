//! OpenLinkHub (Corsair device daemon) resume tuning.
//!
//! On logind `PrepareForSleep(false)` OpenLinkHub waits `resumeDelay` ms, then
//! exits so systemd restarts it after `RestartSec`; the devices it drives
//! (keyboards, mice, fans, lighting) don't respond for that whole time after
//! every wake.
//!
//! OpenLinkHub runs either as a system unit (upstream `install.sh`, usually in
//! `/opt/OpenLinkHub`) or as a user unit (`install-user-space.sh`, any folder).
//! The config file is found from the unit's `WorkingDirectory=`/`ExecStart=`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hyprdeck_core::{cmd, store};

pub const UNIT: &str = "OpenLinkHub.service";
const KEY: &str = "resumeDelay";
const DROPIN: &str = "OpenLinkHub.service.d/fast-resume.conf";

/// Which systemd manager runs OpenLinkHub.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    User,
    System,
}

impl Scope {
    fn systemctl_args(self) -> &'static [&'static str] {
        match self {
            Scope::User => &["--user"],
            Scope::System => &[],
        }
    }

    /// `systemctl` invocation for commands shown to the user to run themselves.
    pub fn systemctl(self) -> &'static str {
        match self {
            Scope::User => "systemctl --user",
            Scope::System => "sudo systemctl",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Scope::User => "user service",
            Scope::System => "system service",
        }
    }
}

/// A detected OpenLinkHub installation.
#[derive(Debug, Clone)]
pub struct Install {
    pub scope: Scope,
    /// `config.json`, when found.
    pub config: Option<PathBuf>,
}

fn systemctl<I, S>(scope: Scope, args: I) -> Result<cmd::Output>
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut all: Vec<std::ffi::OsString> = scope.systemctl_args().iter().map(Into::into).collect();
    all.extend(args.into_iter().map(|a| a.as_ref().to_owned()));
    cmd::output("systemctl", all)
}

/// Find the OpenLinkHub unit (user manager first, then system) and its config. Blocking.
pub fn detect() -> Option<Install> {
    [Scope::User, Scope::System].into_iter().find_map(|scope| {
        let out = systemctl(
            scope,
            [
                "show",
                UNIT,
                "-p",
                "LoadState,WorkingDirectory,ExecStart,FragmentPath",
            ],
        )
        .ok()?;
        let props = parse_props(&out.stdout);
        if props.get("LoadState").map(String::as_str) != Some("loaded") {
            return None;
        }
        let config = config_candidates(&props, &store::home())
            .into_iter()
            .find(|p| p.is_file());
        Some(Install { scope, config })
    })
}

/// `Key=value` lines of `systemctl show`.
pub fn parse_props(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .collect()
}

/// The `path=` field of an `ExecStart=` value as printed by `systemctl show`.
pub fn exec_path(exec_start: &str) -> Option<&str> {
    let rest = exec_start.split("path=").nth(1)?;
    let path = rest.split(" ;").next()?.trim();
    (!path.is_empty()).then_some(path)
}

/// Where `config.json` may be, most specific first: the unit's working
/// directory, the folder of its executable, then the two upstream install locations.
pub fn config_candidates(props: &BTreeMap<String, String>, home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    if let Some(wd) = props.get("WorkingDirectory") {
        // `-` (ignore missing) and `!` prefixes; `~` is the user's home.
        let wd = wd.trim_start_matches(['-', '!']);
        if wd == "~" {
            dirs.push(home.to_owned());
        } else if wd.starts_with('/') {
            dirs.push(PathBuf::from(wd));
        }
    }
    if let Some(dir) = props
        .get("ExecStart")
        .and_then(|e| exec_path(e))
        .and_then(|p| Path::new(p).parent())
    {
        dirs.push(dir.to_owned());
    }
    dirs.push(home.join("OpenLinkHub"));
    dirs.push(PathBuf::from("/opt/OpenLinkHub"));
    let mut out: Vec<PathBuf> = Vec::new();
    for d in dirs {
        let p = d.join("config.json");
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// Drop-in holding our `RestartSec=` override.
pub fn dropin_path(scope: Scope) -> PathBuf {
    match scope {
        Scope::User => std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|p| p.is_absolute())
            .unwrap_or_else(|| store::home().join(".config"))
            .join("systemd/user")
            .join(DROPIN),
        Scope::System => Path::new("/etc/systemd/system").join(DROPIN),
    }
}

#[derive(Debug, Clone)]
pub struct OlhInfo {
    pub scope: Scope,
    pub config: PathBuf,
    /// Whether hyprdeck can edit `config.json` (system installs may be root-owned).
    pub config_writable: bool,
    pub resume_delay_ms: u64,
    /// Effective `RestartSec` of the unit, in seconds.
    pub restart_sec: f64,
    /// `systemctl is-active` answer.
    pub active: String,
}

impl OlhInfo {
    /// Seconds the devices are gone after a wake: resume delay + restart delay.
    pub fn wake_delay(&self) -> f64 {
        self.resume_delay_ms as f64 / 1000.0 + self.restart_sec
    }

    /// Whether the delay after wake is longer than needed.
    pub fn slow(&self) -> bool {
        self.resume_delay_ms > 5000 || self.restart_sec > 2.0
    }
}

fn writable(p: &Path) -> bool {
    cmd::output("test", [OsStr::new("-w"), p.as_os_str()]).is_ok_and(|o| o.ok())
}

/// Read the current values. Blocking.
pub fn read(install: &Install) -> Result<OlhInfo> {
    let path = install.config.clone().context(
        "config.json not found (looked in the unit's working directory, next to its executable, ~/OpenLinkHub and \
         /opt/OpenLinkHub)",
    )?;
    let text =
        std::fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let resume_delay_ms = resume_delay(&text)?;
    let scope = install.scope;
    let restart = systemctl(scope, ["show", UNIT, "-p", "RestartUSec", "--value"])?.stdout;
    let restart_sec = parse_timespan(restart.trim())
        .with_context(|| format!("unexpected RestartUSec {restart:?}"))?;
    let active = systemctl(scope, ["is-active", UNIT])?
        .stdout
        .trim()
        .to_owned();
    let config_writable = writable(&path) && path.parent().is_some_and(writable);
    Ok(OlhInfo {
        scope,
        config: path,
        config_writable,
        resume_delay_ms,
        restart_sec,
        active,
    })
}

/// `resumeDelay` from the config text.
pub fn resume_delay(text: &str) -> Result<u64> {
    let v: serde_json::Value =
        serde_json::from_str(text).context("config.json is not valid JSON")?;
    v.get(KEY)
        .and_then(serde_json::Value::as_u64)
        .with_context(|| format!("config.json has no numeric {KEY}"))
}

/// Replace the top-level `resumeDelay` number in place, leaving every other
/// byte (key order, indentation, trailing newline or lack of it) untouched.
pub fn set_resume_delay(text: &str, ms: u64) -> Result<String> {
    let mut expected: serde_json::Value =
        serde_json::from_str(text).context("config.json is not valid JSON")?;
    if !expected.get(KEY).is_some_and(serde_json::Value::is_u64) {
        bail!("config.json has no numeric {KEY}");
    }
    expected[KEY] = serde_json::Value::from(ms);
    let needle = format!("\"{KEY}\"");
    for (pos, _) in text.match_indices(&needle) {
        let Some(rest) = text[pos + needle.len()..].trim_start().strip_prefix(':') else {
            continue;
        };
        let value = rest.trim_start();
        let digits = value.len() - value.trim_start_matches(|c: char| c.is_ascii_digit()).len();
        if digits == 0 {
            continue;
        }
        let start = text.len() - value.len();
        let candidate = format!("{}{ms}{}", &text[..start], &text[start + digits..]);
        // Accept the edit only if it changed exactly the top-level key (skips
        // same-named keys in nested objects or inside strings).
        if serde_json::from_str::<serde_json::Value>(&candidate)
            .ok()
            .as_ref()
            == Some(&expected)
        {
            return Ok(candidate);
        }
    }
    bail!("could not edit {KEY} safely (unexpected config.json layout)")
}

/// Write `resumeDelay`. Takes effect when OpenLinkHub next starts. Blocking.
pub fn save_resume_delay(path: &Path, ms: u64) -> Result<()> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let new = set_resume_delay(&text, ms)?;
    if new != text {
        store::write_atomic(path, new.as_bytes())?;
    }
    Ok(())
}

/// Shell command that sets `resumeDelay` in a config hyprdeck cannot write.
pub fn resume_delay_command(path: &Path, ms: u64) -> String {
    format!(
        "sudo sed -i -E 's/(\"{KEY}\"[[:space:]]*:[[:space:]]*)[0-9]+/\\1{ms}/' {}",
        cmd::shell_quote(&path.to_string_lossy())
    )
}

/// Set `RestartSec=` in a `[Service]` drop-in, creating the file content when absent.
pub fn set_restart_sec(existing: Option<&str>, secs: u32) -> String {
    let line = format!("RestartSec={secs}");
    let Some(text) = existing else {
        return format!(
            "[Service]\n# Restart quickly after the post-resume re-init exit so devices return fast.\n{line}\n"
        );
    };
    let mut out: Vec<String> = Vec::new();
    let mut section = "";
    let mut replaced = false;
    let mut has_service = false;
    for l in text.lines() {
        let t = l.trim();
        if t.starts_with('[') {
            if section == "[Service]" && !replaced {
                out.push(line.clone());
                replaced = true;
            }
            section = if t == "[Service]" {
                "[Service]"
            } else {
                "other"
            };
            has_service |= t == "[Service]";
        }
        if section == "[Service]" && t.starts_with("RestartSec=") {
            if !replaced {
                out.push(line.clone());
                replaced = true;
            }
            continue;
        }
        out.push(l.to_owned());
    }
    if !replaced {
        if !has_service {
            out.push("[Service]".to_owned());
        }
        out.push(line);
    }
    out.join("\n") + "\n"
}

/// Write the user-unit drop-in and reload the user manager. Blocking.
pub fn save_restart_sec(secs: u32) -> Result<()> {
    let path = dropin_path(Scope::User);
    let existing = std::fs::read_to_string(&path).ok();
    let new = set_restart_sec(existing.as_deref(), secs);
    if existing.as_deref() != Some(new.as_str()) {
        store::write_atomic(&path, new.as_bytes())?;
        cmd::systemctl_user(["daemon-reload"])?;
    }
    Ok(())
}

/// Shell command that writes the system-unit drop-in (needs root).
pub fn restart_sec_command(secs: u32) -> String {
    let path = dropin_path(Scope::System);
    let dir = path
        .parent()
        .map(|d| d.display().to_string())
        .unwrap_or_default();
    format!(
        "sudo mkdir -p {dir} && printf '[Service]\\nRestartSec={secs}\\n' | sudo tee {} >/dev/null && sudo systemctl daemon-reload",
        path.display()
    )
}

/// Restart the user unit. System units need root: see [`restart_command`]. Blocking.
pub fn restart_user() -> Result<()> {
    cmd::systemctl_user(["restart", UNIT])?;
    Ok(())
}

pub fn restart_command(scope: Scope) -> String {
    format!("{} restart {UNIT}", scope.systemctl())
}

/// Parse a systemd timespan as printed by `systemctl show` (`1s`, `500ms`, `1min 30s`).
pub fn parse_timespan(s: &str) -> Option<f64> {
    if s == "0" {
        return Some(0.0);
    }
    let mut total = 0.0;
    for part in s.split_whitespace() {
        let split = part.find(|c: char| !(c.is_ascii_digit() || c == '.'))?;
        let (num, unit) = part.split_at(split);
        let n: f64 = num.parse().ok()?;
        total += n * match unit {
            "us" | "usec" => 1e-6,
            "ms" | "msec" => 1e-3,
            "s" | "sec" => 1.0,
            "min" | "m" => 60.0,
            "h" | "hr" => 3600.0,
            _ => return None,
        };
    }
    Some(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = "{\n  \"debug\": false,\n  \"listenPort\": 27003,\n  \"exclude\": [],\n  \"nested\": {\n    \"resumeDelay\": 7\n  },\n  \"resumeDelay\": 15000,\n  \"logFile\": \"\",\n  \"nvidiaGpuIndex\": [\n    0\n  ]\n}";

    #[test]
    fn edits_top_level_value_preserving_layout() {
        let out = set_resume_delay(CONFIG, 3000).unwrap();
        assert_eq!(
            out,
            CONFIG.replace("\"resumeDelay\": 15000", "\"resumeDelay\": 3000")
        );
        assert_eq!(resume_delay(&out).unwrap(), 3000);
        // Nested key untouched, no trailing newline added.
        assert!(out.contains("\"resumeDelay\": 7\n"));
        assert!(out.ends_with('}'));
    }

    #[test]
    fn round_trips_to_identical_bytes() {
        let changed = set_resume_delay(CONFIG, 1).unwrap();
        assert_eq!(set_resume_delay(&changed, 15000).unwrap(), CONFIG);
    }

    #[test]
    fn rejects_missing_key() {
        assert!(set_resume_delay("{\"a\": 1}", 5).is_err());
        assert!(set_resume_delay("{\"resumeDelay\": \"x\"}", 5).is_err());
    }

    #[test]
    fn restart_sec_dropin() {
        assert_eq!(
            set_restart_sec(None, 2).lines().last(),
            Some("RestartSec=2")
        );
        let existing = "[Service]\n# keep\nRestartSec=1\n";
        assert_eq!(
            set_restart_sec(Some(existing), 3),
            "[Service]\n# keep\nRestartSec=3\n"
        );
        assert_eq!(
            set_restart_sec(Some("[Unit]\nX=1\n"), 4),
            "[Unit]\nX=1\n[Service]\nRestartSec=4\n"
        );
        assert_eq!(
            set_restart_sec(Some("[Service]\nNice=5\n[Install]\nWantedBy=x\n"), 1),
            "[Service]\nNice=5\nRestartSec=1\n[Install]\nWantedBy=x\n"
        );
    }

    #[test]
    fn timespans() {
        assert_eq!(parse_timespan("1s"), Some(1.0));
        assert_eq!(parse_timespan("500ms"), Some(0.5));
        assert_eq!(parse_timespan("1min 30s"), Some(90.0));
        assert_eq!(parse_timespan("0"), Some(0.0));
        assert_eq!(parse_timespan("x"), None);
    }

    #[test]
    fn finds_config_from_unit() {
        let show = "LoadState=loaded\nWorkingDirectory=/srv/olh\nExecStart={ path=/usr/local/olh/OpenLinkHub ; \
                    argv[]=/usr/local/olh/OpenLinkHub ; ignore_errors=no ; start_time=[n/a] }\nFragmentPath=/x\n";
        let props = parse_props(show);
        assert_eq!(props["LoadState"], "loaded");
        assert_eq!(
            exec_path(&props["ExecStart"]),
            Some("/usr/local/olh/OpenLinkHub")
        );
        let home = Path::new("/home/example");
        assert_eq!(
            config_candidates(&props, home),
            [
                "/srv/olh/config.json",
                "/usr/local/olh/config.json",
                "/home/example/OpenLinkHub/config.json",
                "/opt/OpenLinkHub/config.json",
            ]
            .map(PathBuf::from)
        );
        // No unit details: only the upstream locations, without duplicates.
        let props = parse_props("WorkingDirectory=/opt/OpenLinkHub\nExecStart=\n");
        assert_eq!(
            config_candidates(&props, home),
            [
                "/opt/OpenLinkHub/config.json",
                "/home/example/OpenLinkHub/config.json"
            ]
            .map(PathBuf::from)
        );
    }

    #[test]
    fn root_commands() {
        assert_eq!(
            resume_delay_command(Path::new("/opt/OpenLinkHub/config.json"), 3000),
            "sudo sed -i -E 's/(\"resumeDelay\"[[:space:]]*:[[:space:]]*)[0-9]+/\\13000/' /opt/OpenLinkHub/config.json"
        );
        assert_eq!(
            restart_command(Scope::System),
            "sudo systemctl restart OpenLinkHub.service"
        );
        assert!(restart_sec_command(2).contains("RestartSec=2"));
    }
}
