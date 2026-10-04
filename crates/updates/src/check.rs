//! Update checks: pending repo/AUR upgrades and the upstream releases of
//! detected desktop components. Everything here blocks; run it off the GTK thread.

use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;

use anyhow::Result;
use hyprdeck_core::{cmd, store};
use serde::{Deserialize, Serialize};

use crate::aur;
use crate::github::{self, Release};
use crate::parse::{self, SyncPkg, Update};
use crate::vercmp::{pkgver, vercmp};

/// A desktop component whose upstream releases are worth following. It is only
/// tracked when one of its packages is installed (or, without pacman, when its
/// binary is on `$PATH`).
pub struct Project {
    pub title: &'static str,
    /// `owner/repo` on GitHub.
    pub github: &'static str,
    /// Release package in the official Arch repositories.
    pub repo_pkg: &'static str,
    /// Development package in the AUR (switching is offered only if it still exists there).
    pub git_pkg: &'static str,
    /// Executable used for detection when pacman is unavailable.
    pub binary: Option<&'static str>,
}

/// Known components. Package names verified against the Arch repos and the AUR.
pub const PROJECTS: &[Project] = &[
    Project {
        title: "Hyprland",
        github: "hyprwm/Hyprland",
        repo_pkg: "hyprland",
        git_pkg: "hyprland-git",
        binary: Some("Hyprland"),
    },
    Project {
        title: "Noctalia",
        github: "noctalia-dev/noctalia",
        repo_pkg: "noctalia",
        git_pkg: "noctalia-git",
        binary: Some("noctalia"),
    },
    Project {
        title: "DankMaterialShell",
        github: "AvengeMedia/DankMaterialShell",
        repo_pkg: "dms-shell",
        git_pkg: "dms-shell-git",
        binary: Some("dms"),
    },
    Project {
        title: "Quickshell",
        github: "quickshell-mirror/quickshell",
        repo_pkg: "quickshell",
        git_pkg: "quickshell-git",
        binary: Some("quickshell"),
    },
    Project {
        title: "Waybar",
        github: "Alexays/Waybar",
        repo_pkg: "waybar",
        git_pkg: "waybar-git",
        binary: Some("waybar"),
    },
    Project {
        title: "SwayNotificationCenter",
        github: "ErikReider/SwayNotificationCenter",
        repo_pkg: "swaync",
        git_pkg: "swaync-git",
        binary: Some("swaync"),
    },
    Project {
        title: "hyprlock",
        github: "hyprwm/hyprlock",
        repo_pkg: "hyprlock",
        git_pkg: "hyprlock-git",
        binary: Some("hyprlock"),
    },
    Project {
        title: "hypridle",
        github: "hyprwm/hypridle",
        repo_pkg: "hypridle",
        git_pkg: "hypridle-git",
        binary: Some("hypridle"),
    },
    Project {
        title: "hyprpaper",
        github: "hyprwm/hyprpaper",
        repo_pkg: "hyprpaper",
        git_pkg: "hyprpaper-git",
        binary: Some("hyprpaper"),
    },
    Project {
        title: "hyprsunset",
        github: "hyprwm/hyprsunset",
        repo_pkg: "hyprsunset",
        git_pkg: "hyprsunset-git",
        binary: Some("hyprsunset"),
    },
    Project {
        title: "xdg-desktop-portal-hyprland",
        github: "hyprwm/xdg-desktop-portal-hyprland",
        repo_pkg: "xdg-desktop-portal-hyprland",
        git_pkg: "xdg-desktop-portal-hyprland-git",
        binary: None,
    },
];

/// Package tooling available on this system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tooling {
    pub pacman: bool,
    /// `checkupdates` from pacman-contrib.
    pub checkupdates: bool,
    /// `paru` or `yay`, in that order of preference. Only used for the explicit
    /// "open in a terminal" fallbacks; listing and updating work without one.
    pub aur_helper: Option<String>,
}

impl Tooling {
    pub fn detect() -> Self {
        let pacman = cmd::which("pacman").is_some();
        Tooling {
            pacman,
            checkupdates: pacman && cmd::which("checkupdates").is_some(),
            aur_helper: if pacman {
                ["paru", "yay"]
                    .into_iter()
                    .find(|h| cmd::which(h).is_some())
                    .map(Into::into)
            } else {
                None
            },
        }
    }

    /// Full system upgrade, run interactively in a terminal (the fallback for
    /// the in-app update).
    pub fn upgrade_command(&self) -> String {
        match &self.aur_helper {
            Some(h) => format!("{h} -Syu"),
            None => "sudo pacman -Syu".into(),
        }
    }

    /// Install an AUR package; needs an AUR helper.
    pub fn aur_install_command(&self, pkg: &str) -> Option<String> {
        self.aur_helper
            .as_ref()
            .map(|h| format!("{h} -S {}", cmd::shell_quote(pkg)))
    }

    /// Install a package from the sync repositories (replacing a conflicting AUR package).
    pub fn repo_install_command(&self, pkg: &str) -> String {
        match &self.aur_helper {
            Some(h) => format!("{h} -S --repo {}", cmd::shell_quote(pkg)),
            None => format!("sudo pacman -S {}", cmd::shell_quote(pkg)),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// Unix seconds.
    pub checked_at: i64,
    pub tooling: Tooling,
    pub repo: Vec<Update>,
    pub repo_error: Option<String>,
    /// Installed foreign packages with a newer AUR version.
    pub aur: Vec<Update>,
    pub aur_error: Option<String>,
    /// Installed foreign packages the AUR doesn't know (never updated).
    #[serde(default)]
    pub not_in_aur: Vec<aur::Foreign>,
    /// Detected components only.
    pub tracked: Vec<Tracked>,
}

impl Report {
    pub fn total(&self) -> usize {
        self.repo.len() + self.aur.len()
    }

    pub fn important(&self) -> impl Iterator<Item = &Update> {
        self.repo.iter().chain(&self.aur).filter(|u| u.important)
    }

    pub fn failed(&self) -> bool {
        self.repo_error.is_some() || self.aur_error.is_some()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installed {
    pub package: String,
    pub version: String,
    /// Sync repository providing the package, `None` for foreign (AUR/local) packages.
    pub repo: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StatusKind {
    UpToDate,
    RepoUpdate,
    BehindUpstream,
    Git,
    /// Found on `$PATH` without pacman; the installed version is unknown.
    Unmanaged,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub kind: StatusKind,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tracked {
    pub title: String,
    pub github: String,
    pub repo_pkg: String,
    pub git_pkg: String,
    /// `git_pkg` exists in the AUR.
    pub git_in_aur: bool,
    /// Installed package; `None` when detected via `binary` (no pacman).
    pub installed: Option<Installed>,
    /// Path of the detected executable when pacman is unavailable.
    pub binary: Option<String>,
    /// What `pacman -Syu` would install for `repo_pkg` (highest-priority repo).
    pub repo: Option<SyncPkg>,
    pub upstream: Option<Release>,
    pub upstream_fetched_at: Option<i64>,
    pub upstream_error: Option<String>,
    /// Status at check time (the UI recomputes it so relative dates stay fresh).
    pub status: Status,
}

impl Tracked {
    pub fn on_git(&self) -> bool {
        self.installed
            .as_ref()
            .is_some_and(|i| i.package == self.git_pkg)
    }

    pub fn status(&self, now: i64) -> Status {
        let st = |kind, text: String| Status { kind, text };
        let Some(inst) = &self.installed else {
            let path = self.binary.as_deref().unwrap_or("found on PATH");
            return st(
                StatusKind::Unmanaged,
                match &self.upstream {
                    Some(r) => {
                        format!(
                            "Installed outside pacman ({path}); latest release {} was {}",
                            r.tag,
                            released_ago(r, now)
                        )
                    }
                    None => format!(
                        "Installed outside pacman ({path}); latest upstream release unknown"
                    ),
                },
            );
        };
        if self.on_git() {
            let text = match &self.upstream {
                Some(r) => format!(
                    "Following the main branch via {} ({}); latest release {} was {}",
                    self.git_pkg,
                    pkgver(&inst.version),
                    r.tag,
                    released_ago(r, now)
                ),
                None => format!(
                    "Following the main branch via {} ({})",
                    self.git_pkg,
                    pkgver(&inst.version)
                ),
            };
            return st(StatusKind::Git, text);
        }
        if let Some(repo) = &self.repo
            && vercmp(&repo.version, &inst.version).is_gt()
        {
            return st(
                StatusKind::RepoUpdate,
                format!(
                    "Update available in repos: {} → {} ({})",
                    inst.version, repo.version, repo.repo
                ),
            );
        }
        let repo_ver = self
            .repo
            .as_ref()
            .map_or(inst.version.as_str(), |r| r.version.as_str());
        match &self.upstream {
            Some(r) if vercmp(r.version(), pkgver(repo_ver)).is_gt() => {
                let where_ = match self
                    .repo
                    .as_ref()
                    .map(|r| r.repo.as_str())
                    .or(inst.repo.as_deref())
                {
                    Some(name) => format!("{name} repo"),
                    None => "installed package".to_owned(),
                };
                st(
                    StatusKind::BehindUpstream,
                    format!(
                        "Upstream {} {}; {where_} still at {} (repos usually catch up within days)",
                        r.tag,
                        released_ago(r, now),
                        pkgver(repo_ver)
                    ),
                )
            }
            Some(_) => st(StatusKind::UpToDate, "Up to date".into()),
            None => st(
                StatusKind::UpToDate,
                "Up to date with the repos (latest upstream release unknown)".into(),
            ),
        }
    }
}

fn released_ago(r: &Release, now: i64) -> String {
    if r.published_at == 0 {
        "released (date unknown)".into()
    } else {
        format!("released {}", crate::days_ago(now - r.published_at))
    }
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs() as i64)
}

/// The temporary sync database `checkupdates` maintains (`${TMPDIR:-/tmp}/checkup-db-${UID}`).
pub(crate) fn checkupdates_db() -> Option<PathBuf> {
    let uid = std::fs::metadata("/proc/self").ok()?.uid();
    let tmp = std::env::var_os("TMPDIR").map_or_else(|| PathBuf::from("/tmp"), PathBuf::from);
    let db = tmp.join(format!("checkup-db-{uid}"));
    db.join("sync").is_dir().then_some(db)
}

pub(crate) fn repo_updates(tooling: &Tooling) -> Result<Vec<Update>, String> {
    if !tooling.checkupdates {
        return Err("checkupdates not found: install the pacman-contrib package to check repository updates".into());
    }
    let out = cmd::output("checkupdates", ["--nocolor"]).map_err(|e| format!("{e:#}"))?;
    match out.status {
        0 => Ok(parse::parse_updates(&out.stdout)),
        2 => Ok(Vec::new()),
        code => Err(failure("checkupdates", code, &out.stderr)),
    }
}

pub(crate) fn failure(what: &str, code: i32, stderr: &str) -> String {
    let msg = stderr
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if msg.is_empty() {
        format!("{what} failed (exit {code})")
    } else {
        format!("{what} failed: {msg}")
    }
}

/// Installed versions of every package any known project uses.
fn installed_packages() -> Vec<(String, String)> {
    let names = PROJECTS.iter().flat_map(|p| [p.repo_pkg, p.git_pkg]);
    // Missing names are reported on stderr with exit 1; stdout still lists the rest.
    cmd::output("pacman", std::iter::once("-Q").chain(names))
        .map(|o| parse::parse_query(&o.stdout))
        .unwrap_or_default()
}

/// Sync-repo candidates for `names`.
fn sync_info(names: &[&str]) -> Vec<SyncPkg> {
    if names.is_empty() {
        return Vec::new();
    }
    let mut args: Vec<String> = vec!["LC_ALL=C".into(), "pacman".into(), "-Si".into()];
    // Prefer the database checkupdates just refreshed; the system one may be stale.
    if let Some(db) = checkupdates_db() {
        args.push("--dbpath".into());
        args.push(db.to_string_lossy().into_owned());
    }
    args.extend(names.iter().map(|n| (*n).to_owned()));
    cmd::output("env", &args)
        .map(|o| parse::parse_sync_info(&o.stdout))
        .unwrap_or_default()
}

/// How a project was found on this system.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detection {
    /// Installed package name and version (the `-git` package wins if both are listed).
    Package(String, String),
    /// Executable path, when pacman is unavailable.
    Binary(String),
}

/// Whether `project` is present: by installed package when pacman exists, else
/// by executable on `$PATH`.
pub fn detect(
    project: &Project,
    pacman: bool,
    installed: &[(String, String)],
    which: impl Fn(&str) -> Option<String>,
) -> Option<Detection> {
    if pacman {
        [project.git_pkg, project.repo_pkg]
            .into_iter()
            .find_map(|pkg| {
                let (name, version) = installed.iter().find(|(n, _)| n == pkg)?;
                Some(Detection::Package(name.clone(), version.clone()))
            })
    } else {
        project.binary.and_then(&which).map(Detection::Binary)
    }
}

/// Packages that count as important beyond the generic patterns: those of tracked projects.
pub(crate) fn mark_important(updates: &mut [Update], tracked: &[Tracked]) {
    for u in updates {
        u.important = parse::is_important(&u.name)
            || tracked
                .iter()
                .any(|t| u.name == t.repo_pkg || u.name == t.git_pkg);
    }
}

fn tracked(
    project: &Project,
    detection: Detection,
    sync: &[SyncPkg],
    git_in_aur: bool,
    upstream: Result<github::Lookup>,
    now: i64,
) -> Tracked {
    let (installed, binary) = match detection {
        Detection::Package(package, version) => {
            let repo = sync
                .iter()
                .find(|s| s.name == package)
                .map(|s| s.repo.clone());
            (
                Some(Installed {
                    package,
                    version,
                    repo,
                }),
                None,
            )
        }
        Detection::Binary(path) => (None, Some(path)),
    };
    let (upstream, upstream_fetched_at, upstream_error) = match upstream {
        Ok(github::Lookup {
            release: Some(r),
            fetched_at,
            warning,
        }) => (Some(r), Some(fetched_at), warning),
        Ok(github::Lookup {
            release: None,
            fetched_at,
            warning,
        }) => (
            None,
            Some(fetched_at),
            Some(
                warning.unwrap_or_else(|| format!("{} has no published releases", project.github)),
            ),
        ),
        Err(e) => (None, None, Some(format!("{e:#}"))),
    };
    let mut t = Tracked {
        title: project.title.into(),
        github: project.github.into(),
        repo_pkg: project.repo_pkg.into(),
        git_pkg: project.git_pkg.into(),
        git_in_aur,
        installed,
        binary,
        repo: sync.iter().find(|s| s.name == project.repo_pkg).cloned(),
        upstream,
        upstream_fetched_at,
        upstream_error,
        status: Status {
            kind: StatusKind::UpToDate,
            text: String::new(),
        },
    };
    t.status = t.status(now);
    t
}

/// Run a full check. Individual failures are recorded in the report.
pub fn run_check() -> Report {
    let now = now();
    let tooling = Tooling::detect();
    let installed = if tooling.pacman {
        installed_packages()
    } else {
        Vec::new()
    };
    let detected: Vec<(&'static Project, Detection)> = PROJECTS
        .iter()
        .filter_map(|p| Some((p, detect(p, tooling.pacman, &installed, cmd::which)?)))
        .collect();
    let (mut repo, repo_error, aur, aur_error, tracked) = std::thread::scope(|s| {
        let aur = tooling
            .pacman
            .then(|| s.spawn(|| aur::list_updates().map_err(|e| format!("{e:#}"))));
        // Network lookups are independent of pacman; run them in parallel.
        let gh: Vec<_> = detected
            .iter()
            .map(|&(p, _)| s.spawn(move || github::latest_release(p.github, now)))
            .collect();
        let git_pkgs: Vec<&str> = if tooling.pacman {
            detected.iter().map(|(p, _)| p.git_pkg).collect()
        } else {
            Vec::new()
        };
        let in_aur = s.spawn(move || aur::existing(&git_pkgs, now));
        let (repo, repo_error) = if tooling.pacman {
            split(repo_updates(&tooling))
        } else {
            (Vec::new(), None)
        };
        // After checkupdates so its freshly synced database is used.
        let names: Vec<&str> = detected
            .iter()
            .flat_map(|(p, _)| [p.repo_pkg, p.git_pkg])
            .collect();
        let sync = if tooling.pacman {
            sync_info(&names)
        } else {
            Vec::new()
        };
        let (aur, aur_error) = match aur.map(|h| h.join()) {
            Some(Ok(Ok(listing))) => (listing, None),
            Some(Ok(Err(e))) => (aur::Listing::default(), Some(e)),
            Some(Err(_)) => (
                aur::Listing::default(),
                Some("AUR update check panicked".into()),
            ),
            None => (aur::Listing::default(), None),
        };
        let in_aur = in_aur.join().unwrap_or_default();
        let tracked: Vec<Tracked> = detected
            .into_iter()
            .zip(gh)
            .map(|((p, d), h)| {
                let upstream = h
                    .join()
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("GitHub lookup panicked")));
                tracked(p, d, &sync, in_aur.contains(p.git_pkg), upstream, now)
            })
            .collect();
        (repo, repo_error, aur, aur_error, tracked)
    });
    mark_important(&mut repo, &tracked);
    let aur::Listing {
        updates: mut aur,
        not_in_aur,
        ..
    } = aur;
    mark_important(&mut aur, &tracked);
    Report {
        checked_at: now,
        tooling,
        repo,
        repo_error,
        aur,
        aur_error,
        not_in_aur,
        tracked,
    }
}

fn split(r: Result<Vec<Update>, String>) -> (Vec<Update>, Option<String>) {
    match r {
        Ok(v) => (v, None),
        Err(e) => (Vec::new(), Some(e)),
    }
}

fn report_path() -> PathBuf {
    store::state_dir().join("updates-report.json")
}

/// The last saved report; reports from older versions that lack fields fail to
/// parse and are treated as "not checked yet".
pub fn load_report() -> Option<Report> {
    let text = std::fs::read_to_string(report_path()).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_report(report: &Report) -> Result<()> {
    store::write_atomic(&report_path(), &serde_json::to_vec_pretty(report)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(
        installed: (&str, &str, Option<&str>),
        repo: Option<&str>,
        upstream: Option<(&str, i64)>,
    ) -> Tracked {
        Tracked {
            title: "Example Shell".into(),
            github: "example/shell".into(),
            repo_pkg: "example-shell".into(),
            git_pkg: "example-shell-git".into(),
            git_in_aur: true,
            installed: Some(Installed {
                package: installed.0.into(),
                version: installed.1.into(),
                repo: installed.2.map(Into::into),
            }),
            binary: None,
            repo: repo.map(|v| SyncPkg {
                repo: "extra".into(),
                name: "example-shell".into(),
                version: v.into(),
            }),
            upstream: upstream.map(|(tag, published_at)| Release {
                tag: tag.into(),
                name: tag.into(),
                published_at,
                url: String::new(),
                notes: String::new(),
                assets: Vec::new(),
            }),
            upstream_fetched_at: None,
            upstream_error: None,
            status: Status {
                kind: StatusKind::UpToDate,
                text: String::new(),
            },
        }
    }

    const DAY: i64 = 86_400;

    #[test]
    fn behind_upstream_when_repo_lags() {
        let t = sample(
            ("example-shell", "5.2.0-1", Some("extra")),
            Some("5.2.0-1"),
            Some(("v5.2.1", DAY)),
        );
        let s = t.status(4 * DAY + 5);
        assert_eq!(s.kind, StatusKind::BehindUpstream);
        assert_eq!(
            s.text,
            "Upstream v5.2.1 released 3 days ago; extra repo still at 5.2.0 (repos usually catch up within days)"
        );
    }

    #[test]
    fn repo_update_beats_upstream_notice() {
        let t = sample(
            ("example-shell", "5.2.0-1", Some("extra")),
            Some("5.2.1-1"),
            Some(("v5.2.1", 0)),
        );
        let s = t.status(DAY);
        assert_eq!(s.kind, StatusKind::RepoUpdate);
        assert_eq!(
            s.text,
            "Update available in repos: 5.2.0-1 → 5.2.1-1 (extra)"
        );
    }

    #[test]
    fn up_to_date_and_pkgrel_bumps() {
        let t = sample(
            ("example-shell", "5.2.1-1", Some("extra")),
            Some("5.2.1-1"),
            Some(("v5.2.1", 0)),
        );
        assert_eq!(t.status(DAY).kind, StatusKind::UpToDate);
        let t = sample(
            ("example-shell", "5.2.1-1", Some("extra")),
            Some("5.2.1-2"),
            Some(("v5.2.1", 0)),
        );
        assert_eq!(t.status(DAY).kind, StatusKind::RepoUpdate);
    }

    #[test]
    fn git_package_is_reported_as_git() {
        let t = sample(
            ("example-shell-git", "5.2.1.r12.gabc-1", None),
            Some("5.2.1-1"),
            Some(("v5.2.1", 0)),
        );
        let s = t.status(0);
        assert_eq!(s.kind, StatusKind::Git);
        assert!(
            s.text
                .starts_with("Following the main branch via example-shell-git (5.2.1.r12.gabc)"),
            "{}",
            s.text
        );
    }

    #[test]
    fn unmanaged_binary_reports_upstream() {
        let mut t = sample(("x", "1", None), None, Some(("v2.0.0", DAY)));
        t.installed = None;
        t.binary = Some("/usr/local/bin/example-shell".into());
        let s = t.status(3 * DAY);
        assert_eq!(s.kind, StatusKind::Unmanaged);
        assert_eq!(
            s.text,
            "Installed outside pacman (/usr/local/bin/example-shell); latest release v2.0.0 was released 2 days ago"
        );
    }

    const PROJECT: Project = Project {
        title: "Example",
        github: "example/example",
        repo_pkg: "example",
        git_pkg: "example-git",
        binary: Some("example"),
    };

    #[test]
    fn detection_by_package_or_binary() {
        let none = |_: &str| None;
        let found = |b: &str| Some(format!("/usr/bin/{b}"));
        let installed = vec![
            ("example".to_owned(), "1.0-1".to_owned()),
            ("other".to_owned(), "2-1".to_owned()),
        ];
        assert_eq!(
            detect(&PROJECT, true, &installed, none),
            Some(Detection::Package("example".into(), "1.0-1".into()))
        );
        // pacman present: a stray binary does not count, only packages do.
        assert_eq!(detect(&PROJECT, true, &[], found), None);
        let both = vec![
            ("example".to_owned(), "1.0-1".to_owned()),
            ("example-git".to_owned(), "1.1.r3.gabc-1".to_owned()),
        ];
        assert_eq!(
            detect(&PROJECT, true, &both, none),
            Some(Detection::Package(
                "example-git".into(),
                "1.1.r3.gabc-1".into()
            ))
        );
        assert_eq!(
            detect(&PROJECT, false, &installed, found),
            Some(Detection::Binary("/usr/bin/example".into()))
        );
        assert_eq!(detect(&PROJECT, false, &installed, none), None);
    }

    #[test]
    fn tracked_packages_are_important() {
        let t = sample(("example-shell", "1-1", None), None, None);
        let mut u = parse::parse_updates(
            "example-shell 1-1 -> 2-1\nfoo 1-1 -> 2-1\nlinux-zen 6.1-1 -> 6.2-1\n",
        );
        mark_important(&mut u, &[t]);
        assert_eq!(
            u.iter().map(|u| u.important).collect::<Vec<_>>(),
            [true, false, true]
        );
    }

    #[test]
    fn tooling_commands() {
        let paru = Tooling {
            pacman: true,
            checkupdates: true,
            aur_helper: Some("paru".into()),
        };
        assert_eq!(paru.upgrade_command(), "paru -Syu");
        assert_eq!(
            paru.aur_install_command("foo-git").as_deref(),
            Some("paru -S foo-git")
        );
        assert_eq!(paru.repo_install_command("foo"), "paru -S --repo foo");
        let bare = Tooling {
            aur_helper: None,
            ..paru
        };
        assert_eq!(bare.upgrade_command(), "sudo pacman -Syu");
        assert_eq!(bare.aur_install_command("foo-git"), None);
        assert_eq!(bare.repo_install_command("foo"), "sudo pacman -S foo");
    }
}
