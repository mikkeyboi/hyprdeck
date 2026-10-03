//! Delete flow for user-owned startup entries: what is always removed, which
//! related user files can optionally go too, and what stays (root-owned files,
//! packages, hand-written Hyprland config).

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use hyprdeck_core::{cmd, hypr::model, store};

use crate::desktop::{self, DesktopFile};
use crate::units::{self, Unit};
use crate::xdg::{self, Autostart};

#[derive(Debug, Clone)]
pub struct Related {
    pub path: PathBuf,
    pub why: String,
    /// A related unit file: stopped and disabled before removal.
    pub unit: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct Plan {
    pub name: String,
    /// Unit to stop (and disable when `disable`).
    pub unit: Option<String>,
    pub stop: bool,
    pub disable: bool,
    pub reload: bool,
    /// Always removed.
    pub files: Vec<PathBuf>,
    /// Offered as opt-in checkboxes.
    pub related: Vec<Related>,
    /// Things that need root and are not removed.
    pub root_only: Vec<String>,
    /// References left in place (packages, hand-written config).
    pub left: Vec<String>,
    /// Shell command (run in a terminal) that cleans up the root-owned parts.
    pub root_cleanup: Option<String>,
}

pub fn tilde(p: &Path) -> String {
    match p.strip_prefix(store::home()) {
        Ok(rel) => format!("~/{}", rel.display()),
        Err(_) => p.display().to_string(),
    }
}

impl Plan {
    /// Fixed actions, in execution order.
    pub fn steps(&self) -> Vec<String> {
        let mut out = Vec::new();
        if let Some(u) = &self.unit {
            if self.stop {
                out.push(format!("Stop {u}"));
            }
            if self.disable {
                out.push(format!("Disable {u} (remove its .wants links)"));
            }
        }
        for f in &self.files {
            out.push(if f.is_dir() {
                format!("Remove folder {}", tilde(f))
            } else {
                format!("Remove {}", tilde(f))
            });
        }
        if self.reload {
            out.push("Reload systemd (daemon-reload)".into());
        }
        out
    }
}

/// Plan for a unit file in `~/.config/systemd/user`.
pub fn for_unit(u: &Unit) -> Result<Plan> {
    if u.vendor || !u.path.starts_with(units::user_dir()) {
        bail!("{} is provided by the system and can't be deleted", u.name);
    }
    let mut plan = Plan {
        name: u.name.clone(),
        unit: Some(u.name.clone()),
        stop: !matches!(u.props.active(), "inactive" | "failed" | ""),
        disable: u.enabled() || wants_links(&u.name).next().is_some(),
        reload: true,
        files: vec![u.path.clone()],
        ..Default::default()
    };
    plan.files.extend(u.drop_in_dir.clone());
    let text = std::fs::read_to_string(&u.path).unwrap_or_default();
    let mut main = None;
    let mut exes = Vec::new();
    for key in [
        "ExecStart",
        "ExecStartPre",
        "ExecStartPost",
        "ExecStop",
        "ExecStopPost",
        "ExecReload",
        "ExecCondition",
    ] {
        for v in units::ini_values(&text, "Service", key) {
            if let Some(p) = units::exec_program(&v) {
                if key == "ExecStart" && main.is_none() {
                    main = Some(p.clone());
                }
                exes.push(p);
            }
        }
    }
    let mut finder = Finder::new(&plan, u.stem(), &exes);
    finder.main_program(main.as_deref());
    finder.related_units(u);
    finder.scan_common(Vec::new());
    finder.finish(&mut plan);
    Ok(plan)
}

/// Plan for a user autostart file without a system counterpart.
pub fn for_autostart(a: &Autostart, running: bool) -> Result<Plan> {
    if !a.deletable() {
        bail!(
            "{} overrides a system autostart file and can't be deleted",
            a.id
        );
    }
    let mut plan = Plan {
        name: a.name.clone(),
        unit: Some(a.unit.clone()),
        stop: running,
        reload: true,
        files: vec![a.path.clone()],
        ..Default::default()
    };
    let args = desktop::exec_args(&a.file.get_string("Exec").unwrap_or_default());
    let main = args.first().and_then(|p| resolve(p));
    let mut exes: Vec<PathBuf> = main.iter().cloned().collect();
    exes.extend(a.file.get_string("TryExec").and_then(|t| resolve(&t)));
    let mut finder = Finder::new(&plan, a.id.trim_end_matches(".desktop"), &exes);
    finder.main_program(main.as_deref());
    finder.scan_common(a.icon.iter().cloned().collect());
    finder.finish(&mut plan);
    Ok(plan)
}

fn resolve(program: &str) -> Option<PathBuf> {
    let expanded = program.strip_prefix("~/").map_or_else(
        || program.to_owned(),
        |r| store::home().join(r).to_string_lossy().into_owned(),
    );
    xdg::find_executable(&expanded)
        .or_else(|| expanded.starts_with('/').then(|| PathBuf::from(&expanded)))
}

fn wants_links(unit: &str) -> impl Iterator<Item = PathBuf> {
    let unit = unit.to_owned();
    std::fs::read_dir(units::user_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| {
            e.file_name().to_string_lossy().ends_with(".wants")
                || e.file_name().to_string_lossy().ends_with(".requires")
        })
        .map(move |e| e.path().join(&unit))
        .filter(|p| p.symlink_metadata().is_ok())
}

fn exists(p: &Path) -> bool {
    p.symlink_metadata().is_ok()
}

const XDG_BASES: [&str; 4] = [".config", ".local/share", ".local/state", ".cache"];
const ROOT_PREFIXES: [&str; 5] = ["/etc/", "/opt/", "/var/", "/srv/", "/usr/local/"];

fn path_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || "._/+-@".contains(c)
}

/// Collects related files for a plan.
struct Finder {
    home: PathBuf,
    /// Lowercase names identifying the entry's files (stem, program names).
    keys: Vec<String>,
    exclude: HashSet<PathBuf>,
    related: Vec<Related>,
    root_only: Vec<String>,
    left: Vec<String>,
    root_cleanup: Option<String>,
    scripts: Vec<PathBuf>,
    uses_nmcli: bool,
}

impl Finder {
    fn new(plan: &Plan, stem: &str, exes: &[PathBuf]) -> Self {
        let home = store::home();
        let bin = home.join(".local/bin");
        let mut f = Finder {
            exclude: plan.files.iter().cloned().collect(),
            home,
            keys: vec![stem.to_lowercase()],
            related: Vec::new(),
            root_only: Vec::new(),
            left: Vec::new(),
            root_cleanup: None,
            scripts: Vec::new(),
            uses_nmcli: false,
        };
        for exe in exes {
            if exe.starts_with(&bin) && exists(exe) {
                if let Some(name) = exe.file_name().and_then(|n| n.to_str()) {
                    f.add_key(name);
                }
                let why = match std::fs::read_link(exe) {
                    Ok(target) => format!("Program it runs (link → {})", tilde(&target)),
                    Err(_) => "Program it runs".into(),
                };
                f.add(exe.clone(), why, None);
            }
        }
        f
    }

    fn add_key(&mut self, name: &str) {
        let k = name.to_lowercase();
        if k.len() >= 3 && !self.keys.contains(&k) {
            self.keys.push(k);
        }
    }

    fn add(&mut self, path: PathBuf, why: String, unit: Option<String>) {
        if self.exclude.insert(path.clone()) {
            if path.starts_with(self.home.join(".local/bin")) {
                self.scripts.push(path.clone());
            }
            self.related.push(Related { path, why, unit });
        }
    }

    fn key_match(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.keys.iter().any(|k| n.contains(k.as_str()))
    }

    /// Main program outside home: say what happens to it.
    fn main_program(&mut self, main: Option<&Path>) {
        let Some(p) = main else { return };
        if p.starts_with(&self.home) || !exists(p) {
            return;
        }
        match owning_package(p) {
            Some(pkg) => self.left.push(format!(
                "Program {} stays installed (package {pkg})",
                p.display()
            )),
            None => self
                .root_only
                .push(format!("Program {} (outside your home)", p.display())),
        }
    }

    /// Unit files with the same stem or that trigger this unit (`Unit=`).
    fn related_units(&mut self, u: &Unit) {
        let dir = units::user_dir();
        let Ok(rd) = std::fs::read_dir(&dir) else {
            return;
        };
        for e in rd.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let path = e.path();
            if name == u.name || !path.is_file() {
                continue;
            }
            let stem = name.rsplit_once('.').map_or(name.as_str(), |(s, _)| s);
            let text = std::fs::read_to_string(&path).unwrap_or_default();
            let triggers = ["Path", "Timer", "Socket"]
                .iter()
                .any(|s| units::ini_values(&text, s, "Unit").contains(&u.name));
            if stem == u.stem() || triggers {
                let why = if triggers {
                    format!("Unit that starts {}", u.name)
                } else {
                    "Unit with the same name".into()
                };
                let drop_in = dir.join(format!("{name}.d"));
                self.add(path, why, Some(name.clone()));
                if drop_in.is_dir() {
                    self.add(drop_in, format!("Drop-in folder of {name}"), None);
                }
            }
        }
    }

    /// Script references (helpers, data dirs, root paths), XDG folders named
    /// after the entry, launchers, icons, Hyprland config mentions.
    fn scan_common(&mut self, icons: Vec<String>) {
        let mut i = 0;
        while i < self.scripts.len() {
            let script = self.scripts[i].clone();
            self.scan_script(&script);
            i += 1;
        }
        self.scan_dirs_and_config(icons);
    }

    /// Distinctive name fragments (≥ 5 chars) for root paths / NM connections.
    fn tokens(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .keys
            .iter()
            .flat_map(|k| k.split(['-', '.', '_']))
            .filter(|t| t.len() >= 5 && !GENERIC.contains(t))
            .map(str::to_owned)
            .collect();
        out.sort();
        out.dedup();
        out
    }

    fn scan_dirs_and_config(&mut self, mut icons: Vec<String>) {
        for base in XDG_BASES {
            for k in self.keys.clone() {
                let p = self.home.join(base).join(&k);
                if exists(&p) {
                    self.add(p, format!("Folder in {base} named after it"), None);
                }
            }
        }
        self.launchers(&mut icons);
        self.icons(&icons);
        self.hypr_mentions();
        if self.uses_nmcli {
            self.nm_connections();
        }
    }

    fn scan_script(&mut self, script: &Path) {
        let Ok(bytes) = std::fs::read(script) else {
            return;
        };
        if bytes.len() > 4 << 20 || bytes[..bytes.len().min(4096)].contains(&0) {
            return;
        }
        let text = String::from_utf8_lossy(&bytes);
        let name = script
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let home_str = self.home.to_string_lossy().into_owned();
        let home_prefixes = [
            "~/".to_owned(),
            "$HOME/".to_owned(),
            "${HOME}/".to_owned(),
            "%h/".to_owned(),
            format!("{home_str}/"),
        ];
        let mut found_root = false;
        for prefix in &home_prefixes {
            for (at, _) in text.match_indices(prefix.as_str()) {
                let rel: String = text[at + prefix.len()..]
                    .chars()
                    .take_while(|&c| path_char(c))
                    .collect();
                self.home_reference(&rel, &name);
            }
        }
        for prefix in ROOT_PREFIXES {
            for (at, _) in text.match_indices(prefix) {
                if text[..at].chars().next_back().is_some_and(path_char) {
                    continue;
                }
                let full: String = text[at..].chars().take_while(|&c| path_char(c)).collect();
                if self.root_reference(&full, &name) {
                    found_root = true;
                }
            }
        }
        let nm = text.contains("nmcli");
        self.uses_nmcli |= nm;
        if self.root_cleanup.is_none() && text.contains("--remove") && (found_root || nm) {
            self.root_cleanup = Some(format!(
                "sudo {} --remove",
                cmd::shell_quote(&script.to_string_lossy())
            ));
        }
    }

    fn home_reference(&mut self, rel: &str, script: &str) {
        let rel = rel.trim_end_matches(['/', '.']);
        if let Some(name) = rel.strip_prefix(".local/bin/") {
            let name = name.split('/').next().unwrap_or(name);
            let p = self.home.join(".local/bin").join(name);
            if self.key_match(name) && exists(&p) {
                self.add(p, format!("Helper used by {script}"), None);
            }
            return;
        }
        for base in XDG_BASES {
            let Some(rest) = rel.strip_prefix(base).and_then(|r| r.strip_prefix('/')) else {
                continue;
            };
            let top = rest.split('/').next().unwrap_or(rest);
            let p = self.home.join(base).join(top);
            if !top.is_empty() && self.key_match(top) && exists(&p) {
                self.add(p, format!("Data used by {script}"), None);
            }
            return;
        }
    }

    /// Record a root-owned path whose name contains one of the entry's tokens.
    fn root_reference(&mut self, full: &str, script: &str) -> bool {
        let tokens = self.tokens();
        let mut acc = PathBuf::from("/");
        for comp in full
            .trim_end_matches(['/', '.'])
            .split('/')
            .filter(|c| !c.is_empty())
        {
            acc.push(comp);
            let lower = comp.to_lowercase();
            if tokens.iter().any(|t| lower.contains(t.as_str())) {
                if !matches!(acc.try_exists(), Ok(true)) {
                    return false;
                }
                let line = format!("{} (used by {script})", acc.display());
                if !self.root_only.contains(&line) {
                    self.root_only.push(line);
                }
                return true;
            }
        }
        false
    }

    fn launchers(&mut self, icons: &mut Vec<String>) {
        let user_exes: Vec<PathBuf> = self
            .related
            .iter()
            .filter(|r| r.path.starts_with(self.home.join(".local/bin")))
            .map(|r| r.path.clone())
            .collect();
        let dirs = [
            (self.home.join(".local/share/applications"), "App launcher"),
            (xdg::user_dir(), "Autostart entry"),
        ];
        for (dir, what) in dirs {
            let Ok(rd) = std::fs::read_dir(&dir) else {
                continue;
            };
            for e in rd.flatten() {
                let path = e.path();
                let name = e.file_name().to_string_lossy().into_owned();
                let Some(stem) = name.strip_suffix(".desktop") else {
                    continue;
                };
                if self.exclude.contains(&path) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let f = DesktopFile::parse(&text);
                let program = desktop::exec_args(&f.get_string("Exec").unwrap_or_default())
                    .into_iter()
                    .next()
                    .and_then(|p| resolve(&p));
                let by_exec = program.is_some_and(|p| user_exes.contains(&p));
                if by_exec || self.keys.iter().any(|k| *k == stem.to_lowercase()) {
                    icons.extend(f.get_string("Icon").filter(|i| !i.is_empty()));
                    self.add(path, what.into(), None);
                }
            }
        }
    }

    fn icons(&mut self, icons: &[String]) {
        let root = self.home.join(".local/share/icons");
        for icon in icons {
            let p = Path::new(icon);
            if p.is_absolute() {
                if p.starts_with(&root) && exists(p) {
                    self.add(p.to_path_buf(), "Icon".into(), None);
                }
                continue;
            }
            let mut hits = Vec::new();
            find_icons(&root, icon, &mut hits);
            for h in hits {
                self.add(h, "Icon".into(), None);
            }
        }
    }

    /// Lines of the hand-written Hyprland config mentioning the entry, reported
    /// at the enclosing top-level statement (e.g. the `hl.window_rule(` line).
    fn hypr_mentions(&mut self) {
        let managed = model::hypr_dir().join("hyprdeck.lua");
        for file in crate::hypr::config_files() {
            if file == managed {
                continue;
            }
            let Ok(text) = std::fs::read_to_string(&file) else {
                continue;
            };
            let lines: Vec<&str> = text.lines().collect();
            let mut reported = HashSet::new();
            for (i, line) in lines.iter().enumerate() {
                let t = line.trim_start();
                if t.starts_with("--") || !self.key_match(t) {
                    continue;
                }
                let start = (0..=i)
                    .rev()
                    .find(|&j| !lines[j].starts_with([' ', '\t']) && !lines[j].trim().is_empty())
                    .unwrap_or(i);
                if !reported.insert(start) {
                    continue;
                }
                let stmt = lines[start].trim();
                let what = stmt.split_once('(').map_or(stmt, |(head, _)| head);
                let loc = model::Source {
                    file: file.clone(),
                    line: start as u32 + 1,
                }
                .display();
                self.left.push(format!(
                    "{what} at {loc} mentions it (hand-written Hyprland config; edit it yourself)"
                ));
            }
        }
    }

    fn nm_connections(&mut self) {
        let Ok(out) = cmd::run("nmcli", ["-t", "-f", "NAME", "connection", "show"]) else {
            return;
        };
        let tokens = self.tokens();
        let names: Vec<&str> = out
            .lines()
            .filter(|n| {
                let l = n.to_lowercase();
                tokens.iter().any(|t| l.contains(t.as_str()))
            })
            .collect();
        if names.is_empty() {
            return;
        }
        let shown: Vec<&str> = names.iter().take(3).copied().collect();
        let more = if names.len() > 3 {
            format!(", … ({} total)", names.len())
        } else {
            String::new()
        };
        self.root_only.push(format!(
            "NetworkManager connections: {}{more}",
            shown.join(", ")
        ));
    }

    fn finish(self, plan: &mut Plan) {
        plan.related = self.related;
        plan.root_only = self.root_only;
        plan.left = self.left;
        plan.root_cleanup = self.root_cleanup;
    }
}

/// Package owning `path`, via whichever package manager is installed
/// (pacman, dpkg or rpm).
fn owning_package(path: &Path) -> Option<String> {
    let path = path.to_string_lossy();
    let query = |program: &str, args: &[&str]| {
        cmd::which(program)?;
        let o = cmd::output(program, args.iter().copied().chain([&*path])).ok()?;
        o.ok()
            .then(|| o.stdout.trim().to_owned())
            .filter(|s| !s.is_empty())
    };
    query("pacman", &["-Qqo"])
        .or_else(|| {
            query("dpkg", &["-S"]).and_then(|s| {
                s.lines()
                    .next()?
                    .split_once(':')
                    .map(|(pkg, _)| pkg.to_owned())
            })
        })
        .or_else(|| query("rpm", &["-qf", "--qf", "%{NAME}\\n"]))
}

/// Name fragments too common to identify an app's files.
const GENERIC: [&str; 10] = [
    "service",
    "desktop",
    "client",
    "daemon",
    "applet",
    "updater",
    "notifications",
    "server",
    "agent",
    "launcher",
];

fn find_icons(dir: &Path, name: &str, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Ok(ft) = e.file_type() else { continue };
        if ft.is_dir() {
            find_icons(&p, name, out);
        } else if p.file_stem().is_some_and(|s| s == name)
            && p.extension()
                .is_some_and(|x| x == "png" || x == "svg" || x == "xpm")
        {
            out.push(p);
        }
    }
}

/// Outcome of [`execute`].
#[derive(Debug, Default)]
pub struct Report {
    pub removed: Vec<String>,
    pub errors: Vec<String>,
}

fn remove_path(p: &Path) -> std::io::Result<()> {
    let meta = p.symlink_metadata()?;
    if meta.is_dir() {
        std::fs::remove_dir_all(p)
    } else {
        std::fs::remove_file(p)
    }
}

fn stop_disable(unit: &str, stop: bool, disable: bool, report: &mut Report) {
    if stop && let Err(e) = cmd::systemctl_user(["stop", unit]) {
        report.errors.push(format!("stop {unit}: {e:#}"));
    }
    if disable {
        if let Err(e) = cmd::systemctl_user(["disable", unit]) {
            report.errors.push(format!("disable {unit}: {e:#}"));
        }
        for link in wants_links(unit) {
            if let Err(e) = std::fs::remove_file(&link) {
                report.errors.push(format!("{}: {e}", tilde(&link)));
            }
        }
    }
}

/// Run the plan; `chosen[i]` selects `plan.related[i]`.
pub fn execute(plan: &Plan, chosen: &[bool]) -> Report {
    let mut report = Report::default();
    if let Some(u) = &plan.unit {
        stop_disable(u, plan.stop, plan.disable, &mut report);
    }
    let selected: Vec<&Related> = plan
        .related
        .iter()
        .zip(chosen)
        .filter(|(_, c)| **c)
        .map(|(r, _)| r)
        .collect();
    for r in &selected {
        if let Some(u) = &r.unit {
            stop_disable(u, true, true, &mut report);
        }
    }
    for p in plan.files.iter().chain(selected.iter().map(|r| &r.path)) {
        match remove_path(p) {
            Ok(()) => report.removed.push(tilde(p)),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => report.errors.push(format!("{}: {e}", tilde(p))),
        }
    }
    if plan.reload {
        if let Err(e) = units::daemon_reload() {
            report.errors.push(format!("daemon-reload: {e:#}"));
        }
        if let Some(u) = &plan.unit {
            // Clears a lingering failed state; harmless when there is none.
            let _ = cmd::output("systemctl", ["--user", "reset-failed", u]);
        }
    }
    report
}
