//! systemd user units: hand-written ones in `~/.config/systemd/user` and
//! enabled vendor units from `/usr/lib/systemd/user`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use hyprdeck_core::{cmd, store};

pub const VENDOR_DIR: &str = "/usr/lib/systemd/user";
const UNIT_SUFFIXES: [&str; 4] = [".service", ".path", ".timer", ".socket"];

pub fn user_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| store::home().join(".config"))
        .join("systemd/user")
}

/// Runtime properties from `systemctl --user show`.
#[derive(Debug, Clone, Default)]
pub struct Props(HashMap<String, String>);

impl Props {
    pub fn get(&self, key: &str) -> &str {
        self.0.get(key).map_or("", String::as_str)
    }
    pub fn active(&self) -> &str {
        self.get("ActiveState")
    }
    pub fn loaded(&self) -> bool {
        self.get("LoadState") == "loaded"
    }
}

const SHOW_PROPS: &str = "Id,Names,Description,LoadState,ActiveState,SubState,UnitFileState,FragmentPath,DropInPaths,TriggeredBy";

/// Parse `systemctl show` output (blank-line separated blocks) keyed by every
/// name in `Names` plus `Id`.
pub fn parse_show(text: &str) -> HashMap<String, Props> {
    let mut out = HashMap::new();
    for block in text.split("\n\n") {
        let map: HashMap<String, String> = block
            .lines()
            .filter_map(|l| l.split_once('='))
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        if map.is_empty() {
            continue;
        }
        let mut names: Vec<String> = map
            .get("Names")
            .map(|n| n.split_whitespace().map(str::to_owned).collect())
            .unwrap_or_default();
        if let Some(id) = map.get("Id") {
            names.push(id.clone());
        }
        let props = Props(map);
        for n in names {
            out.insert(n, props.clone());
        }
    }
    out
}

/// One `systemctl --user show` call for all `names`.
pub fn show(names: &[&str]) -> Result<HashMap<String, Props>> {
    if names.is_empty() {
        return Ok(HashMap::new());
    }
    let mut args = vec!["--user", "show", "-p", SHOW_PROPS, "--"];
    args.extend_from_slice(names);
    Ok(parse_show(&cmd::run("systemctl", args)?))
}

/// Values of `key` in `[section]` of a unit file (continuation lines joined).
pub fn ini_values(text: &str, section: &str, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_section = false;
    let mut pending: Option<String> = None;
    for raw in text.lines() {
        if let Some(acc) = pending.as_mut() {
            let (part, more) = match raw.strip_suffix('\\') {
                Some(p) => (p, true),
                None => (raw, false),
            };
            acc.push(' ');
            acc.push_str(part.trim());
            if !more {
                out.push(pending.take().unwrap_or_default());
            }
            continue;
        }
        let line = raw.trim();
        if line.starts_with('[') {
            in_section = line == format!("[{section}]");
            continue;
        }
        if !in_section || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() != key {
            continue;
        }
        let v = v.trim();
        match v.strip_suffix('\\') {
            Some(p) => pending = Some(p.trim().to_owned()),
            None if v.is_empty() => out.clear(), // `Key=` resets the list
            None => out.push(v.to_owned()),
        }
    }
    if let Some(p) = pending {
        out.push(p);
    }
    out
}

/// Program path of an `Exec*=` value: strips `-@:+!` prefixes, expands `%h`.
pub fn exec_program(value: &str) -> Option<PathBuf> {
    let first = value.split_whitespace().next()?;
    let prog = first.trim_start_matches(['-', '@', ':', '+', '!']);
    if prog.is_empty() {
        return None;
    }
    Some(PathBuf::from(expand_specifiers(prog)))
}

pub fn expand_specifiers(s: &str) -> String {
    s.replace("%h", &store::home().to_string_lossy())
}

#[derive(Debug, Clone)]
pub struct Unit {
    pub name: String,
    pub path: PathBuf,
    /// From `/usr/lib/systemd/user` (never deleted).
    pub vendor: bool,
    pub description: String,
    pub exec: Option<String>,
    pub wanted_by: Vec<String>,
    /// `UnitFileState`: enabled, disabled, static, masked, …
    pub file_state: String,
    pub props: Props,
    pub drop_in_dir: Option<PathBuf>,
}

impl Unit {
    pub fn enabled(&self) -> bool {
        matches!(
            self.file_state.as_str(),
            "enabled" | "enabled-runtime" | "linked" | "alias"
        )
    }
    /// Units without `[Install]` (`static`) or with an unusual state can't be toggled.
    pub fn toggle_blocker(&self) -> Option<String> {
        match self.file_state.as_str() {
            "enabled" | "disabled" | "masked" => None,
            "static" => Some(match self.props.get("TriggeredBy") {
                "" => "Started on demand (no [Install] section), not at login".into(),
                by => format!("Started by {by}"),
            }),
            "" => Some("Unit not loaded".into()),
            other => Some(format!("Unit file state: {other}")),
        }
    }
    pub fn stem(&self) -> &str {
        self.name.rsplit_once('.').map_or(&self.name, |(s, _)| s)
    }
    /// Human summary of when it starts.
    pub fn starts_with(&self) -> Option<&'static str> {
        self.wanted_by.iter().find_map(|w| match w.as_str() {
            "graphical-session.target" => Some("with the desktop session"),
            "default.target" => Some("at login"),
            "sockets.target" => Some("on demand (socket)"),
            "timers.target" => Some("on a timer"),
            _ => None,
        })
    }
}

fn unit_files(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut out: Vec<String> = rd
        .flatten()
        .filter(|e| e.path().is_file())
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|n| UNIT_SUFFIXES.iter().any(|s| n.ends_with(s)) && !n.contains('@'))
        .collect();
    out.sort_by_key(|n| n.to_lowercase());
    out
}

fn enabled_vendor_names() -> Result<Vec<String>> {
    let text = cmd::run(
        "systemctl",
        [
            "--user",
            "list-unit-files",
            "--state=enabled",
            "--no-legend",
            "--plain",
        ],
    )?;
    Ok(text
        .lines()
        .filter_map(|l| l.split_whitespace().next())
        .map(str::to_owned)
        .collect())
}

fn build(name: String, path: PathBuf, vendor: bool, props: Props) -> Unit {
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let description = match props.get("Description") {
        "" => ini_values(&text, "Unit", "Description")
            .pop()
            .unwrap_or_default(),
        d => d.to_owned(),
    };
    let exec = ini_values(&text, "Service", "ExecStart")
        .into_iter()
        .next()
        .map(|e| expand_specifiers(&e));
    let wanted_by = ini_values(&text, "Install", "WantedBy")
        .iter()
        .chain(&ini_values(&text, "Install", "RequiredBy"))
        .flat_map(|v| v.split_whitespace().map(str::to_owned))
        .collect();
    let drop_in_dir = Some(user_dir().join(format!("{name}.d"))).filter(|d| d.is_dir());
    Unit {
        file_state: props.get("UnitFileState").to_owned(),
        name,
        path,
        vendor,
        description,
        exec,
        wanted_by,
        props,
        drop_in_dir,
    }
}

/// User-owned units, then enabled vendor units (each sorted by name).
pub fn scan() -> Result<Vec<Unit>> {
    let dir = user_dir();
    let own = unit_files(&dir);
    let vendor_candidates: Vec<String> = enabled_vendor_names()?
        .into_iter()
        .filter(|n| !own.contains(n))
        .collect();
    let mut names: Vec<&str> = own.iter().map(String::as_str).collect();
    names.extend(vendor_candidates.iter().map(String::as_str));
    let mut props = show(&names)?;
    let mut out = Vec::new();
    for name in own {
        let p = props.remove(&name).unwrap_or_default();
        out.push(build(name.clone(), dir.join(&name), false, p));
    }
    let mut vendor: Vec<Unit> = vendor_candidates
        .into_iter()
        .filter_map(|name| {
            let p = props.remove(&name)?;
            let frag = PathBuf::from(p.get("FragmentPath"));
            frag.starts_with(VENDOR_DIR)
                .then(|| build(name, frag, true, p))
        })
        .collect();
    vendor.sort_by_key(|u| u.name.to_lowercase());
    out.extend(vendor);
    Ok(out)
}

fn is_enabled(name: &str) -> String {
    cmd::output("systemctl", ["--user", "is-enabled", name])
        .map(|o| o.stdout.trim().to_owned())
        .unwrap_or_default()
}

/// Outcome of a disable request.
#[derive(Debug, PartialEq)]
pub enum Disabled {
    Done,
    /// Still enabled through `/etc/systemd/user` (only masking stops it per user).
    EnabledSystemWide,
}

pub fn enable(name: &str) -> Result<()> {
    if is_enabled(name) == "masked" {
        cmd::systemctl_user(["unmask", name])?;
    }
    if is_enabled(name) != "enabled" {
        cmd::systemctl_user(["enable", name])?;
    }
    Ok(())
}

pub fn disable(name: &str) -> Result<Disabled> {
    cmd::systemctl_user(["disable", name])?;
    Ok(if is_enabled(name) == "enabled" {
        Disabled::EnabledSystemWide
    } else {
        Disabled::Done
    })
}

pub fn mask(name: &str) -> Result<()> {
    cmd::systemctl_user(["mask", name]).map(drop)
}

/// Start/stop/restart without blocking on slow units; waits up to 5 s for the
/// unit to settle so the refreshed state is meaningful. A start that ends in
/// `failed` is an error.
pub fn control(verb: &str, name: &str) -> Result<()> {
    cmd::systemctl_user([verb, "--no-block", name])?;
    let started = Instant::now();
    let mut active = String::new();
    while started.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(200));
        active = show(&[name])?
            .remove(name)
            .unwrap_or_default()
            .active()
            .to_owned();
        let transitional = matches!(active.as_str(), "activating" | "deactivating" | "reloading");
        // A queued start job can leave the unit `inactive` for a moment.
        let pending =
            verb != "stop" && active == "inactive" && started.elapsed() < Duration::from_secs(1);
        if !transitional && !pending {
            break;
        }
    }
    if verb != "stop" && active == "failed" {
        bail!("{name} failed to start; open Show log for details");
    }
    Ok(())
}

pub fn daemon_reload() -> Result<()> {
    cmd::systemctl_user(["daemon-reload"]).map(drop)
}

/// Last 200 journal lines of a user unit.
pub fn log(name: &str) -> Result<String> {
    let out = cmd::run(
        "journalctl",
        [
            "--user",
            "-u",
            name,
            "-n",
            "200",
            "--no-pager",
            "--no-hostname",
            "-o",
            "short-iso",
        ],
    )?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ini_parsing() {
        let text = "[Unit]\nDescription=A b\n[Service]\nExecStart=%h/x --a \\\n  --b\n# ExecStart=nope\n[Install]\nWantedBy=graphical-session.target\nWantedBy=default.target\n";
        assert_eq!(ini_values(text, "Unit", "Description"), ["A b"]);
        assert_eq!(ini_values(text, "Service", "ExecStart"), ["%h/x --a --b"]);
        assert_eq!(
            ini_values(text, "Install", "WantedBy"),
            ["graphical-session.target", "default.target"]
        );
        assert!(ini_values(text, "Service", "Description").is_empty());
        assert!(
            ini_values(
                "[Service]\nExecStart=a\nExecStart=\n",
                "Service",
                "ExecStart"
            )
            .is_empty()
        );
    }

    #[test]
    fn exec_prefixes() {
        assert_eq!(
            exec_program("-/usr/bin/sleep 2").unwrap(),
            PathBuf::from("/usr/bin/sleep")
        );
        assert_eq!(
            exec_program("!!@/bin/x y").unwrap(),
            PathBuf::from("/bin/x")
        );
        assert!(exec_program("  ").is_none());
    }

    #[test]
    fn show_parsing() {
        let text = "Id=a.service\nNames=a.service alias.service\nActiveState=active\n\nId=b.service\nNames=b.service\nActiveState=failed\nDescription=x=y\n";
        let m = parse_show(text);
        assert_eq!(m["alias.service"].active(), "active");
        assert_eq!(m["b.service"].active(), "failed");
        assert_eq!(m["b.service"].get("Description"), "x=y");
    }
}
