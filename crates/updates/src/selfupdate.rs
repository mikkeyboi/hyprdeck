//! hyprdeck's own updates, by install mode:
//!
//! - AppImage (`$APPIMAGE`): the `stable` channel follows the latest GitHub
//!   release, `nightly` the rolling `nightly` prerelease; updating replaces the
//!   AppImage in place.
//! - Source checkout (still on disk with `install.sh`): follows the checked-out
//!   branch's upstream; updating fast-forwards it and runs `install.sh` in a
//!   transient systemd unit, outside hyprdeck's cgroup, so the service restart
//!   it performs can't kill the build.
//! - Anything else: version only.
//!
//! Everything here blocks; run it off the GTK thread.

use std::cmp::Ordering;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use hyprdeck_core::{BUILD_COMMIT, VERSION, cmd, store};
use serde::{Deserialize, Serialize};

use crate::check::failure;
use crate::github::{self, Asset, Release};
use crate::parse::{self, CrateUpdate};

/// GitHub repository hyprdeck releases are published to.
pub const REPO: &str = "mikkeyboi/hyprdeck";
pub const RELEASES_URL: &str = "https://github.com/mikkeyboi/hyprdeck/releases";
/// Release asset holding the AppImage, and its optional `sha256sum` file.
pub const APPIMAGE_ASSET: &str = "Hyprdeck-x86_64.AppImage";
pub const CHECKSUM_ASSET: &str = "Hyprdeck-x86_64.AppImage.sha256";
/// Tag of the rolling prerelease CI republishes on every push to main.
pub const NIGHTLY_TAG: &str = "nightly";
const SERVICE: &str = "hyprdeck.service";
/// Transient unit running `install.sh` for source updates.
const JOB_UNIT: &str = "hyprdeck-self-update";
/// Self-update GitHub lookups are reused for this long ("Check now" bypasses it).
const CACHE_TTL: i64 = 30 * 60;
/// Most incoming commits listed for a source checkout.
const MAX_INCOMING: usize = 50;
/// `git log` format parsed by [`parse_log`].
const LOG_FORMAT: &str = "--format=%H%x09%ct%x09%s";
/// Longest a source build may take before we stop waiting for it.
pub const JOB_TIMEOUT: Duration = Duration::from_secs(2 * 3600);

/// What the background checker does when a new version appears.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Policy {
    /// No background self-update checks.
    Off,
    /// Notify; install when the notification's action is clicked.
    #[default]
    Notify,
    /// Install automatically, then restart (deferred while the window is open).
    Auto,
}

impl Policy {
    pub const ALL: [Policy; 3] = [Policy::Off, Policy::Notify, Policy::Auto];

    pub fn label(self) -> &'static str {
        match self {
            Policy::Off => "Off",
            Policy::Notify => "Notify me",
            Policy::Auto => "Install automatically",
        }
    }
}

/// Which AppImage builds to follow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Channel {
    /// Tagged releases.
    #[default]
    Stable,
    /// The `nightly` prerelease, rebuilt from every push to main.
    Nightly,
}

impl Channel {
    pub const ALL: [Channel; 2] = [Channel::Stable, Channel::Nightly];

    pub fn as_str(self) -> &'static str {
        match self {
            Channel::Stable => "stable",
            Channel::Nightly => "nightly",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Channel::Stable => "Stable releases",
            Channel::Nightly => "Nightly builds",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        Channel::ALL.into_iter().find(|c| c.as_str() == s)
    }
}

/// How this hyprdeck was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Running from the AppImage at this path.
    AppImage(PathBuf),
    /// Built from this source checkout (it still exists and has `install.sh`).
    Source(PathBuf),
    /// Anything else (e.g. a distro package).
    Installed,
}

impl Mode {
    pub fn describe(&self) -> String {
        match self {
            Mode::AppImage(p) => format!("AppImage at {}", p.display()),
            Mode::Source(d) => format!("Built from source at {}", d.display()),
            Mode::Installed => "Installed by a package or script".into(),
        }
    }
}

pub fn mode() -> Mode {
    if let Some(path) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
        return Mode::AppImage(PathBuf::from(path));
    }
    source_dir().map_or(Mode::Installed, Mode::Source)
}

/// The workspace this binary was built from, if it is still on disk.
fn source_dir() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("install.sh").is_file() && d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// Semver precedence of two versions (leading `v` and `+build` metadata ignored).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (Vec<u64>, Option<&str>) {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let v = v.split_once('+').map_or(v, |(v, _)| v);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        (
            core.split('.').map(|n| n.parse().unwrap_or(0)).collect(),
            pre,
        )
    }
    let ((ca, pa), (cb, pb)) = (split(a), split(b));
    let len = ca.len().max(cb.len());
    let part = |c: &[u64], i: usize| c.get(i).copied().unwrap_or(0);
    (0..len)
        .map(|i| part(&ca, i).cmp(&part(&cb, i)))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            match (pa, pb) {
                (None, None) => Ordering::Equal,
                // A pre-release sorts before its release.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(pa), Some(pb)) => {
                    let (mut ia, mut ib) = (pa.split('.'), pb.split('.'));
                    loop {
                        match (ia.next(), ib.next()) {
                            (None, None) => break Ordering::Equal,
                            (None, Some(_)) => break Ordering::Less,
                            (Some(_), None) => break Ordering::Greater,
                            (Some(x), Some(y)) => {
                                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                                    (Ok(x), Ok(y)) => x.cmp(&y),
                                    (Ok(_), Err(_)) => Ordering::Less,
                                    (Err(_), Ok(_)) => Ordering::Greater,
                                    (Err(_), Err(_)) => x.cmp(y),
                                };
                                if o.is_ne() {
                                    break o;
                                }
                            }
                        }
                    }
                }
            }
        })
}

fn is_prerelease(version: &str) -> bool {
    let v = version.trim().trim_start_matches(['v', 'V']);
    v.split_once('+').map_or(v, |(v, _)| v).contains('-')
}

/// Stable channel: whether release `tag` should replace the running `current`
/// version. Only newer versions; pre-release tags only for pre-release builds.
pub fn stable_update(tag: &str, current: &str) -> bool {
    compare_versions(tag, current).is_gt() && (!is_prerelease(tag) || is_prerelease(current))
}

/// Nightly channel: whether the nightly build of `nightly` differs from the
/// running build. A build without a known commit always takes the nightly.
pub fn nightly_update(nightly: &str, build: Option<&str>) -> bool {
    build.is_none_or(|b| !same_commit(b, nightly))
}

/// Whether two commit hashes name the same commit (either may be abbreviated
/// to at least 7 digits).
pub fn same_commit(a: &str, b: &str) -> bool {
    let n = a.len().min(b.len());
    n >= 7 && a.as_bytes()[..n].eq_ignore_ascii_case(&b.as_bytes()[..n])
}

/// 7-digit abbreviation of a commit hash.
pub fn short(sha: &str) -> &str {
    sha.get(..7).unwrap_or(sha)
}

/// The release files an AppImage update needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAssets {
    pub appimage: Asset,
    pub checksum: Option<Asset>,
}

/// The AppImage asset (exact name) and its checksum file if published.
pub fn select_assets(assets: &[Asset]) -> Option<UpdateAssets> {
    let find = |name: &str| assets.iter().find(|a| a.name == name).cloned();
    Some(UpdateAssets {
        appimage: find(APPIMAGE_ASSET)?,
        checksum: find(CHECKSUM_ASSET),
    })
}

/// The hash from a `sha256sum` line (`<64 hex>  <file>`), lowercased.
pub fn parse_sha256(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
}

// ---------------------------------------------------------------------------
// Checking

/// Why an available update can't be applied automatically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    NotGit,
    Detached,
    NoUpstream {
        branch: String,
    },
    UpstreamGone {
        upstream: String,
    },
    Diverged {
        ahead: usize,
        behind: usize,
    },
    Dirty {
        changed: usize,
        untracked: usize,
    },
    NoInstallScript,
    NoCargo,
    /// The release has no AppImage asset.
    NoAsset {
        tag: String,
    },
}

impl Blocker {
    pub fn title(&self) -> &'static str {
        match self {
            Blocker::NotGit => "Not a git checkout",
            Blocker::Detached => "Detached HEAD",
            Blocker::NoUpstream { .. } => "Branch has no upstream",
            Blocker::UpstreamGone { .. } => "Upstream branch is gone",
            Blocker::Diverged { .. } => "Checkout has diverged from upstream",
            Blocker::Dirty { .. } => "Uncommitted changes",
            Blocker::NoInstallScript => "install.sh is not executable",
            Blocker::NoCargo => "cargo not found",
            Blocker::NoAsset { .. } => "No AppImage in the release",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Blocker::NotGit => {
                "The source directory is not a git checkout (or git is not installed), so there is no branch to follow. Rebuild it manually with install.sh.".into()
            }
            Blocker::Detached => {
                "HEAD is detached, so there is no branch to follow. Check out a branch with an upstream (e.g. main) to get updates.".into()
            }
            Blocker::NoUpstream { branch } => format!(
                "Branch {branch} has no upstream, so there is nothing to follow. Source installs update along their branch's upstream; check out main or set one with git branch --set-upstream-to."
            ),
            Blocker::UpstreamGone { upstream } => format!(
                "The upstream branch {upstream} no longer exists on the remote. Check out another branch to keep getting updates."
            ),
            Blocker::Diverged { ahead, behind } => format!(
                "The checkout has {ahead} local commit{} that upstream doesn't, and upstream has {behind} new one{}; it can't fast-forward. Merge or rebase in a terminal.",
                plural(*ahead),
                plural(*behind)
            ),
            Blocker::Dirty { changed, untracked } => {
                let mut parts = Vec::new();
                if *changed > 0 {
                    parts.push(format!("{changed} changed file{}", plural(*changed)));
                }
                if *untracked > 0 {
                    parts.push(format!("{untracked} untracked file{}", plural(*untracked)));
                }
                format!(
                    "The working tree has {}. Commit or stash them before updating.",
                    parts.join(" and ")
                )
            }
            Blocker::NoInstallScript => {
                "install.sh in the source checkout is missing or not executable.".into()
            }
            Blocker::NoCargo => {
                "cargo was not found in a login shell; install Rust (rustup or the rust package) to build updates.".into()
            }
            Blocker::NoAsset { tag } => format!(
                "Release {tag} has no {APPIMAGE_ASSET}; download it from the release page."
            ),
        }
    }
}

pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// AppImage check result for one channel.
#[derive(Debug, Clone)]
pub struct AppImageCheck {
    pub path: PathBuf,
    pub channel: Channel,
    /// Release to compare with; `None`: nothing published on this channel.
    pub release: Option<Release>,
    /// Commit the nightly build was made from (nightly channel).
    pub commit: Option<String>,
    pub fetched_at: i64,
    /// Stale cached data was used because GitHub could not be reached.
    pub warning: Option<String>,
    pub available: bool,
}

impl AppImageCheck {
    /// "0.2.0" or "nightly 1a2b3c4".
    pub fn target_label(&self) -> Option<String> {
        let rel = self.release.as_ref()?;
        Some(match (&self.channel, &self.commit) {
            (Channel::Nightly, Some(c)) => format!("nightly {}", short(c)),
            _ => rel.version().to_owned(),
        })
    }
}

pub fn check_appimage(
    path: &Path,
    channel: Channel,
    now: i64,
    force: bool,
) -> Result<AppImageCheck> {
    let max_age = if force { 0 } else { CACHE_TTL };
    Ok(match channel {
        Channel::Stable => {
            let l = github::latest_release_within(REPO, now, max_age)?;
            AppImageCheck {
                path: path.to_path_buf(),
                channel,
                available: l
                    .release
                    .as_ref()
                    .is_some_and(|r| stable_update(&r.tag, VERSION)),
                release: l.release,
                commit: None,
                fetched_at: l.fetched_at,
                warning: l.warning,
            }
        }
        Channel::Nightly => {
            let l = github::tagged_release(REPO, NIGHTLY_TAG, now, max_age)?;
            let (release, commit) = l.release.map(|t| (t.release, t.commit)).unzip();
            AppImageCheck {
                path: path.to_path_buf(),
                channel,
                available: commit
                    .as_deref()
                    .is_some_and(|c| nightly_update(c, BUILD_COMMIT)),
                release,
                commit,
                fetched_at: l.fetched_at,
                warning: l.warning,
            }
        }
    })
}

/// `git status --porcelain=v2 --branch`, parsed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GitStatus {
    /// Full hash of HEAD; `None` before the first commit.
    pub head: Option<String>,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Configured upstream, e.g. `origin/main`.
    pub upstream: Option<String>,
    /// Commits (ahead, behind) the upstream; `None` when it is gone or unset.
    pub ahead_behind: Option<(usize, usize)>,
    /// Modified, staged, renamed or conflicted tracked paths.
    pub changed: usize,
    pub untracked: usize,
}

impl GitStatus {
    pub fn dirty(&self) -> usize {
        self.changed + self.untracked
    }
}

pub fn parse_git_status(text: &str) -> GitStatus {
    let mut s = GitStatus::default();
    for line in text.lines() {
        if let Some(header) = line.strip_prefix("# ") {
            let (key, value) = header.split_once(' ').unwrap_or((header, ""));
            let value = value.trim();
            match key {
                "branch.oid" => s.head = (value != "(initial)").then(|| value.to_owned()),
                "branch.head" => s.branch = (value != "(detached)").then(|| value.to_owned()),
                "branch.upstream" => s.upstream = Some(value.to_owned()),
                "branch.ab" => {
                    let mut it = value.split_whitespace();
                    let ahead = it.next().and_then(|v| v.strip_prefix('+')?.parse().ok());
                    let behind = it.next().and_then(|v| v.strip_prefix('-')?.parse().ok());
                    s.ahead_behind = ahead.zip(behind);
                }
                _ => {}
            }
        } else if line.starts_with("? ") {
            s.untracked += 1;
        } else if matches!(line.split(' ').next(), Some("1" | "2" | "u")) {
            s.changed += 1;
        }
    }
    s
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    /// Full hash.
    pub hash: String,
    /// Commit time, Unix seconds.
    pub time: i64,
    pub subject: String,
}

/// Lines of `git log --format=%H%x09%ct%x09%s`.
pub fn parse_log(text: &str) -> Vec<Commit> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.splitn(3, '\t');
            Some(Commit {
                hash: it.next()?.to_owned(),
                time: it.next()?.parse().ok()?,
                subject: it.next().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// What updating a source checkout would do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePlan {
    /// Upstream commits the checkout lacks.
    pub behind: usize,
    /// Local commits upstream lacks.
    pub ahead: usize,
    /// The checkout's HEAD differs from the commit the running binary was built from.
    pub rebuild: bool,
    pub blocker: Option<Blocker>,
}

impl SourcePlan {
    pub fn available(&self) -> bool {
        self.behind > 0 || self.rebuild
    }

    pub fn ready(&self) -> bool {
        self.available() && self.blocker.is_none()
    }
}

/// Decide from the checkout's status and the commit the running binary was
/// built from (`None`: unknown, then only upstream commits count).
pub fn plan_source(git: &GitStatus, build: Option<&str>) -> SourcePlan {
    let rebuild = matches!((git.head.as_deref(), build), (Some(h), Some(b)) if !same_commit(h, b));
    let (ahead, behind) = git.ahead_behind.unwrap_or((0, 0));
    let blocker = match (&git.branch, &git.upstream, git.ahead_behind) {
        (None, _, _) => Some(Blocker::Detached),
        (Some(branch), None, _) => Some(Blocker::NoUpstream {
            branch: branch.clone(),
        }),
        (_, Some(upstream), None) => Some(Blocker::UpstreamGone {
            upstream: upstream.clone(),
        }),
        _ if ahead > 0 && behind > 0 => Some(Blocker::Diverged { ahead, behind }),
        _ if (behind > 0 || rebuild) && git.dirty() > 0 => Some(Blocker::Dirty {
            changed: git.changed,
            untracked: git.untracked,
        }),
        _ => None,
    };
    SourcePlan {
        behind,
        ahead,
        rebuild,
        blocker,
    }
}

/// Source checkout check result.
#[derive(Debug, Clone)]
pub struct SourceCheck {
    pub dir: PathBuf,
    /// `None`: not a git checkout, or git is not installed.
    pub git: Option<GitStatus>,
    pub head: Option<Commit>,
    /// Upstream commits not in the checkout, newest first.
    pub incoming: Vec<Commit>,
    /// `git fetch` failed; the counts are from the last successful fetch.
    pub fetch_error: Option<String>,
    pub plan: SourcePlan,
}

/// Inspect `dir`: `git fetch` (when `fetch` and an upstream is set), then
/// compare with the upstream and with `build`, the running binary's commit.
pub fn check_source(dir: &Path, fetch: bool, build: Option<&str>) -> Result<SourceCheck> {
    if !dir.join(".git").exists() || cmd::which("git").is_none() {
        return Ok(SourceCheck {
            dir: dir.to_path_buf(),
            git: None,
            head: None,
            incoming: Vec::new(),
            fetch_error: None,
            plan: SourcePlan {
                behind: 0,
                ahead: 0,
                rebuild: false,
                blocker: Some(Blocker::NotGit),
            },
        });
    }
    let mut status = git_status(dir)?;
    let mut fetch_error = None;
    if fetch && status.upstream.is_some() {
        match git(dir, &["fetch", "--quiet"]) {
            Ok(out) if out.ok() => status = git_status(dir)?,
            Ok(out) => fetch_error = Some(failure("git fetch", out.status, &out.stderr)),
            Err(e) => fetch_error = Some(format!("{e:#}")),
        }
    }
    let head = git_stdout(dir, &["log", "-1", LOG_FORMAT])
        .ok()
        .and_then(|t| parse_log(&t).into_iter().next());
    let mut plan = plan_source(&status, build);
    let incoming = if plan.behind > 0 {
        let limit = MAX_INCOMING.to_string();
        git_stdout(dir, &["log", LOG_FORMAT, "-n", &limit, "HEAD..@{u}"])
            .map(|t| parse_log(&t))
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if plan.ready() {
        if !is_executable(&dir.join("install.sh")) {
            plan.blocker = Some(Blocker::NoInstallScript);
        } else if !cargo_available() {
            plan.blocker = Some(Blocker::NoCargo);
        }
    }
    Ok(SourceCheck {
        dir: dir.to_path_buf(),
        git: Some(status),
        head,
        incoming,
        fetch_error,
        plan,
    })
}

fn git_status(dir: &Path) -> Result<GitStatus> {
    git_stdout(dir, &["status", "--porcelain=v2", "--branch"]).map(|t| parse_git_status(&t))
}

/// Run git in `dir` without ever prompting for credentials.
fn git(dir: &Path, args: &[&str]) -> Result<cmd::Output> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .context("failed to spawn git")?;
    Ok(cmd::Output {
        status: out.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    })
}

fn git_stdout(dir: &Path, args: &[&str]) -> Result<String> {
    let out = git(dir, args)?;
    if !out.ok() {
        bail!(
            "{}",
            failure(&format!("git {}", args[0]), out.status, &out.stderr)
        );
    }
    Ok(out.stdout)
}

/// cargo as `install.sh` will see it (login shell: rustup's PATH included).
fn cargo_available() -> bool {
    cmd::output("bash", ["-lc", "command -v cargo"]).is_ok_and(|o| o.ok())
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}

/// The check for whichever way hyprdeck is installed.
#[derive(Debug, Clone)]
pub enum SelfCheck {
    AppImage(AppImageCheck),
    Source(SourceCheck),
    Installed,
}

/// An available update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// Release tag or commit, so each new version is announced only once.
    pub id: String,
    /// "Hyprdeck 0.2.0 is available".
    pub title: String,
    /// One line about what is new.
    pub detail: String,
    /// Tray/button label: "Update Hyprdeck to 0.2.0".
    pub action: String,
    /// Why it can't be installed without the user's help.
    pub blocker: Option<Blocker>,
}

impl SelfCheck {
    /// Check `mode`; `force` bypasses the GitHub cache (source checks always fetch).
    pub fn run(mode: &Mode, channel: Channel, now: i64, force: bool) -> Result<SelfCheck> {
        Ok(match mode {
            Mode::AppImage(path) => SelfCheck::AppImage(check_appimage(path, channel, now, force)?),
            Mode::Source(dir) => SelfCheck::Source(check_source(dir, true, BUILD_COMMIT)?),
            Mode::Installed => SelfCheck::Installed,
        })
    }

    pub fn offer(&self) -> Option<Offer> {
        match self {
            SelfCheck::AppImage(c) if c.available => {
                let rel = c.release.as_ref()?;
                let label = c.target_label()?;
                let blocker = select_assets(&rel.assets)
                    .is_none()
                    .then(|| Blocker::NoAsset {
                        tag: rel.tag.clone(),
                    });
                Some(Offer {
                    id: c.commit.clone().unwrap_or_else(|| rel.tag.clone()),
                    title: format!("Hyprdeck {label} is available"),
                    detail: match &c.commit {
                        Some(commit) => format!("Nightly build of commit {}", short(commit)),
                        None => format!("You have {VERSION}; {} is out", rel.name),
                    },
                    action: format!("Update Hyprdeck to {label}"),
                    blocker,
                })
            }
            SelfCheck::Source(c) if c.plan.available() => {
                let upstream = c
                    .git
                    .as_ref()
                    .and_then(|g| g.upstream.as_deref())
                    .unwrap_or("upstream");
                let (id, title, detail, action) = if c.plan.behind > 0 {
                    let n = c.plan.behind;
                    let newest = c.incoming.first();
                    (
                        newest.map_or_else(|| format!("behind-{n}"), |c| c.hash.clone()),
                        format!("{n} new Hyprdeck commit{} available", plural(n)),
                        match newest {
                            Some(c) => format!("On {upstream}: {}", c.subject),
                            None => format!("On {upstream}"),
                        },
                        format!("Update Hyprdeck ({n} new commit{})", plural(n)),
                    )
                } else {
                    let head = c.head.as_ref().map_or("", |h| h.hash.as_str());
                    (
                        head.to_owned(),
                        "A newer Hyprdeck build is ready to install".to_owned(),
                        format!(
                            "The checkout is at {}, newer than the running build",
                            short(head)
                        ),
                        "Rebuild Hyprdeck".to_owned(),
                    )
                };
                Some(Offer {
                    id,
                    title,
                    detail,
                    action,
                    blocker: c.plan.blocker.clone(),
                })
            }
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Applying

/// Progress reporting for [`apply_appimage`] / [`apply_source`].
pub trait Progress {
    fn step(&self, msg: &str);
    /// New output appended to the build log (source mode).
    fn log(&self, _text: &str) {}
    /// The transient build unit was started (source mode).
    fn job_started(&self) {}
}

/// How to switch to the installed update.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Restart {
    /// Program to re-exec this process into.
    pub exec: PathBuf,
    /// Also restart `hyprdeck.service` (AppImage: nothing else restarts it).
    pub service: bool,
}

#[derive(Debug, Clone)]
pub struct Applied {
    /// What is now installed: "0.2.0", "nightly 1a2b3c4", "1a2b3c4".
    pub label: String,
    pub message: String,
    pub restart: Restart,
}

/// When to restart after an update: never pull the window from under the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPlan {
    Now,
    /// Ask ("Restart to finish") and restart once the window is hidden.
    WhenHidden,
}

pub fn restart_plan(window_visible: bool) -> RestartPlan {
    if window_visible {
        RestartPlan::WhenHidden
    } else {
        RestartPlan::Now
    }
}

/// Download and install the AppImage `c` offers. With `require_checksum`
/// (automatic installs) releases without a published checksum are refused.
pub fn apply_appimage(
    c: &AppImageCheck,
    require_checksum: bool,
    progress: &dyn Progress,
) -> Result<Applied> {
    let (Some(rel), Some(label)) = (&c.release, c.target_label()) else {
        bail!("nothing is published on the {} channel", c.channel.as_str());
    };
    if !c.available {
        bail!("already up to date ({label})");
    }
    let assets = select_assets(&rel.assets).ok_or_else(|| {
        anyhow!(
            "{}",
            Blocker::NoAsset {
                tag: rel.tag.clone()
            }
            .message()
        )
    })?;
    if require_checksum && assets.checksum.is_none() {
        bail!(
            "{} has no published checksum; not installing it automatically",
            rel.tag
        );
    }
    progress.step(&format!(
        "Downloading {label} ({:.1} MB)…",
        assets.appimage.size as f64 / 1e6
    ));
    let verified = install_appimage(&c.path, &assets)?;
    Ok(Applied {
        message: format!(
            "Installed {label} to {}{}",
            c.path.display(),
            if verified {
                " (checksum verified)"
            } else {
                " (no checksum published)"
            }
        ),
        label,
        restart: Restart {
            exec: c.path.clone(),
            service: true,
        },
    })
}

fn curl_args<'a>(extra: &[&'a str], url: &'a str) -> Vec<&'a str> {
    let mut args = vec!["-fsSL", "--retry", "2", "-A", "hyprdeck"];
    args.extend_from_slice(extra);
    args.push(url);
    args
}

/// Download the release AppImage next to `appimage`, verify its checksum when
/// one is published, make it executable and atomically replace `appimage`.
/// Returns whether the checksum was verified.
pub fn install_appimage(appimage: &Path, assets: &UpdateAssets) -> Result<bool> {
    // Replace the real file even if $APPIMAGE is a symlink to it.
    let target = std::fs::canonicalize(appimage).unwrap_or_else(|_| appimage.to_path_buf());
    let dir = target
        .parent()
        .context("AppImage path has no parent directory")?;
    let tmp = dir.join(format!(".{APPIMAGE_ASSET}.{}.part", std::process::id()));
    let result = download_verified(&tmp, assets).and_then(|verified| {
        let mode = std::fs::metadata(&target).map_or(0o755, |m| m.mode() & 0o7777) | 0o755;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .context("making the download executable")?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("replacing {}", target.display()))?;
        Ok(verified)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn download_verified(tmp: &Path, assets: &UpdateAssets) -> Result<bool> {
    let tmp_str = tmp.to_str().context("non-UTF-8 AppImage path")?;
    let out = cmd::output(
        "curl",
        curl_args(&["--max-time", "1800", "-o", tmp_str], &assets.appimage.url),
    )?;
    if !out.ok() {
        bail!(
            "{}",
            failure("Downloading the AppImage", out.status, &out.stderr)
        );
    }
    if assets.appimage.size > 0 {
        let got = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
        if got != assets.appimage.size {
            bail!(
                "download incomplete: got {got} bytes, expected {}",
                assets.appimage.size
            );
        }
    }
    let Some(checksum) = &assets.checksum else {
        return Ok(false);
    };
    let out = cmd::output("curl", curl_args(&["--max-time", "60"], &checksum.url))?;
    if !out.ok() {
        bail!(
            "{}",
            failure("Downloading the checksum", out.status, &out.stderr)
        );
    }
    let want = parse_sha256(&out.stdout)
        .ok_or_else(|| anyhow!("{CHECKSUM_ASSET} does not contain a SHA-256 hash"))?;
    let got = cmd::run("sha256sum", [tmp_str])?;
    let got = parse_sha256(&got).ok_or_else(|| anyhow!("unexpected sha256sum output"))?;
    if got != want {
        bail!("checksum mismatch: expected {want}, downloaded file has {got}");
    }
    Ok(true)
}

/// Fast-forward the checkout at `dir` and run `install.sh` in the transient
/// unit, waiting for it. `install.sh` restarts `hyprdeck.service` when it is
/// active, so when this process is the service it usually ends before this returns.
pub fn apply_source(dir: &Path, progress: &dyn Progress) -> Result<Applied> {
    if job_running() {
        bail!("a Hyprdeck update is already building (unit {JOB_UNIT})");
    }
    progress.step("Fetching from upstream…");
    let c = check_source(dir, true, BUILD_COMMIT)?;
    if let Some(e) = &c.fetch_error {
        bail!("{e}");
    }
    if let Some(b) = &c.plan.blocker {
        bail!("{}", b.message());
    }
    if !c.plan.available() {
        bail!("already up to date");
    }
    if c.plan.behind > 0 {
        progress.step(&format!(
            "Pulling {} new commit{}…",
            c.plan.behind,
            plural(c.plan.behind)
        ));
        git_stdout(dir, &["pull", "--ff-only", "--quiet"])?;
    }
    let head = git_stdout(dir, &["rev-parse", "HEAD"])?.trim().to_owned();
    progress.step(&format!(
        "Building and installing {} (install.sh)…",
        short(&head)
    ));
    start_job(dir)?;
    progress.job_started();
    if !wait_job(progress, JOB_TIMEOUT)? {
        bail!("install.sh failed; see {}", log_path().display());
    }
    Ok(Applied {
        label: short(&head).to_owned(),
        message: format!("Built and installed {}", short(&head)),
        restart: Restart {
            exec: store::home().join(".local/bin/hyprdeck"),
            service: false,
        },
    })
}

/// Build output of the last source update.
pub fn log_path() -> PathBuf {
    store::state_dir().join("self-update.log")
}

/// Exit status of the last source update's `install.sh`, written when it ends.
fn status_path() -> PathBuf {
    store::state_dir().join("self-update.status")
}

/// The last `lines` lines of the build log.
pub fn log_tail(lines: usize) -> Option<String> {
    let text = std::fs::read_to_string(log_path()).ok()?;
    let all: Vec<&str> = text.lines().collect();
    Some(all[all.len().saturating_sub(lines)..].join("\n"))
}

fn job_state() -> String {
    cmd::systemctl_user([
        "show",
        "-p",
        "ActiveState",
        "--value",
        &format!("{JOB_UNIT}.service"),
    ])
    .map(|s| s.trim().to_owned())
    .unwrap_or_default()
}

fn is_running_state(state: &str) -> bool {
    matches!(
        state,
        "activating" | "active" | "deactivating" | "reloading" | "refreshing"
    )
}

/// The transient build unit is running (possibly started by another process).
pub fn job_running() -> bool {
    is_running_state(&job_state())
}

fn start_job(dir: &Path) -> Result<()> {
    let (log, status) = (log_path(), status_path());
    std::fs::create_dir_all(store::state_dir()).context("creating the state directory")?;
    let _ = std::fs::remove_file(&status);
    std::fs::write(
        &log,
        format!(
            "== Hyprdeck self-update: {} (running {}) ==\n",
            dir.display(),
            hyprdeck_core::version_string()
        ),
    )
    .with_context(|| format!("writing {}", log.display()))?;
    let q = |p: &Path| cmd::shell_quote(&p.to_string_lossy());
    // No `$` in the command: systemd expands environment variables in it.
    let script = format!(
        "{{ cd {d} && ./install.sh; }} >>{l} 2>&1 && echo 0 >{s} || echo 1 >{s}",
        d = q(dir),
        l = q(&log),
        s = q(&status)
    );
    let unit = format!("--unit={JOB_UNIT}");
    let out = cmd::output(
        "systemd-run",
        [
            "--user",
            unit.as_str(),
            "--collect",
            "--no-block",
            "--property=Type=oneshot",
            "--description=Hyprdeck self-update",
            "--",
            "bash",
            "-lc",
            script.as_str(),
        ],
    )?;
    if !out.ok() {
        bail!("{}", failure("systemd-run", out.status, &out.stderr));
    }
    Ok(())
}

/// Poll the transient unit until it ends, streaming new log output to
/// `progress`. Returns whether `install.sh` succeeded.
pub fn wait_job(progress: &dyn Progress, timeout: Duration) -> Result<bool> {
    let start = Instant::now();
    let mut offset = 0u64;
    let mut seen_running = false;
    loop {
        if let Ok(mut f) = File::open(log_path())
            && f.seek(SeekFrom::Start(offset)).is_ok()
        {
            let mut buf = Vec::new();
            if f.read_to_end(&mut buf).is_ok() && !buf.is_empty() {
                offset += buf.len() as u64;
                progress.log(&String::from_utf8_lossy(&buf));
            }
        }
        let running = job_running();
        seen_running |= running;
        if !running {
            if let Ok(text) = std::fs::read_to_string(status_path())
                && let Ok(code) = text.trim().parse::<i32>()
            {
                return Ok(code == 0);
            }
            // Give a just-queued unit time to start before calling it lost.
            if seen_running || start.elapsed() > Duration::from_secs(30) {
                bail!(
                    "the update job ended without a result; see {}",
                    log_path().display()
                );
            }
        }
        if start.elapsed() > timeout {
            bail!(
                "gave up waiting for the update job after {}",
                crate::duration(timeout.as_secs() as i64)
            );
        }
        std::thread::sleep(Duration::from_secs(2));
    }
}

/// Held while an update runs; also excludes updates from other hyprdeck processes.
pub struct UpdateLock {
    _file: File,
}

pub fn lock() -> Result<UpdateLock> {
    let dir = store::state_dir();
    std::fs::create_dir_all(&dir).context("creating the state directory")?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("self-update.lock"))
        .context("opening the update lock")?;
    match file.try_lock() {
        Ok(()) => Ok(UpdateLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => {
            bail!("another Hyprdeck update is already running")
        }
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow!(e).context("locking the update")),
    }
}

/// Restart `hyprdeck.service` if it is active. Returns `Some(is_this_process)`
/// when it was restarted.
pub fn restart_service() -> Result<Option<bool>> {
    let active =
        cmd::output("systemctl", ["--user", "is-active", "--quiet", SERVICE]).is_ok_and(|o| o.ok());
    if !active {
        return Ok(None);
    }
    let main_pid = cmd::systemctl_user(["show", "-p", "MainPID", "--value", SERVICE])
        .ok()
        .and_then(|s| s.trim().parse::<u32>().ok());
    // --no-block: when we are the service, systemd stops us mid-call.
    cmd::systemctl_user(["--no-block", "restart", SERVICE])?;
    Ok(Some(main_pid == Some(std::process::id())))
}

/// Switch this process to the update. Only returns on failure, or when
/// systemd is about to replace this process (it is the service).
pub fn restart(r: &Restart) -> Result<()> {
    use std::os::unix::process::CommandExt;
    if r.service && restart_service()? == Some(true) {
        return Ok(());
    }
    let err = Command::new(&r.exec)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(anyhow!(err).context(format!("restarting {}", r.exec.display())))
}

/// Semver-compatible crate updates for hyprdeck's lockfile (`cargo update --dry-run`).
pub fn crate_updates(dir: &Path) -> Result<Vec<CrateUpdate>> {
    if cmd::which("cargo").is_none() {
        bail!("cargo not found on PATH");
    }
    let manifest = dir.join("Cargo.toml");
    let out = cmd::output(
        "cargo",
        [
            "update".as_ref(),
            "--dry-run".as_ref(),
            "--color".as_ref(),
            "never".as_ref(),
            "--manifest-path".as_ref(),
            manifest.as_os_str(),
        ],
    )?;
    if !out.ok() {
        bail!(
            "{}",
            failure("cargo update --dry-run", out.status, &out.stderr)
        );
    }
    Ok(parse::parse_cargo_dry_run(&out.stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_precedence() {
        use Ordering::*;
        for (a, b, want) in [
            ("v0.2.0", "0.1.9", Greater),
            ("0.10.0", "0.9.0", Greater),
            ("1.0.0", "v1.0.0", Equal),
            ("1.0", "1.0.0", Equal),
            ("1.0.1", "1.0.0+build.5", Greater),
            ("1.0.0+a", "1.0.0+b", Equal),
            ("1.0.0-rc.1", "1.0.0", Less),
            ("1.0.0-alpha", "1.0.0-alpha.1", Less),
            ("1.0.0-alpha.1", "1.0.0-alpha.beta", Less),
            ("1.0.0-beta.2", "1.0.0-beta.11", Less),
            ("1.0.0-rc.1", "1.0.0-beta.11", Greater),
            ("2.0.0", "10.0.0", Less),
        ] {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    #[test]
    fn stable_channel_decision() {
        assert!(stable_update("v0.2.0", "0.1.0"), "newer");
        assert!(!stable_update("v0.1.0", "0.1.0"), "equal");
        assert!(!stable_update("v0.0.9", "0.1.0"), "older");
        // Pre-releases: never offered to a release build, but a pre-release build
        // moves on to newer pre-releases and to the final release.
        assert!(!stable_update("v0.2.0-rc.1", "0.1.0"));
        assert!(stable_update("v0.2.0-rc.2", "0.2.0-rc.1"));
        assert!(stable_update("v0.2.0", "0.2.0-rc.1"));
        assert!(!stable_update("v0.2.0-rc.1", "0.2.0"));
        // The rolling nightly tag is not a version.
        assert!(!stable_update("nightly", "0.1.0"));
    }

    #[test]
    fn nightly_channel_decision() {
        let a = "533e43f2274a4c4df7b0282c0ae4d876625d12de";
        let b = "42ff02b0000000000000000000000000000000aa";
        assert!(!nightly_update(a, Some(a)), "same commit");
        assert!(!nightly_update(a, Some(&a.to_ascii_uppercase())));
        assert!(!nightly_update(a, Some("533e43f")), "abbreviated");
        assert!(nightly_update(a, Some(b)), "different commit");
        assert!(nightly_update(a, None), "unknown build commit");
        assert!(!same_commit("533e4", "533e4"), "too short to trust");
        assert_eq!(short(a), "533e43f");
        assert_eq!(short("abc"), "abc");
    }

    #[test]
    fn restart_waits_for_hidden_window() {
        assert_eq!(restart_plan(false), RestartPlan::Now);
        assert_eq!(restart_plan(true), RestartPlan::WhenHidden);
    }

    const OID: &str = "533e43f2274a4c4df7b0282c0ae4d876625d12de";

    fn status(branch_ab: &str, entries: &str) -> GitStatus {
        parse_git_status(&format!(
            "# branch.oid {OID}\n# branch.head main\n{branch_ab}{entries}"
        ))
    }

    #[test]
    fn git_status_parsing() {
        let up = "# branch.upstream origin/main\n";
        let s = status(&format!("{up}# branch.ab +0 -3\n"), "");
        assert_eq!(
            s,
            GitStatus {
                head: Some(OID.into()),
                branch: Some("main".into()),
                upstream: Some("origin/main".into()),
                ahead_behind: Some((0, 3)),
                changed: 0,
                untracked: 0,
            }
        );
        let s = status(
            &format!("{up}# branch.ab +2 -1\n"),
            "1 .M N... 100644 100644 100644 aa bb src/a.rs\n\
             2 R. N... 100644 100644 100644 aa bb R100 new.rs\told.rs\n\
             u UU N... 100644 100644 100644 100644 aa bb cc conflict.rs\n\
             ? notes.txt\n",
        );
        assert_eq!(s.ahead_behind, Some((2, 1)));
        assert_eq!((s.changed, s.untracked, s.dirty()), (3, 1, 4));

        let s = status("", "");
        assert_eq!((s.upstream, s.ahead_behind), (None, None));
        let s = status(up, "");
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!(s.ahead_behind, None, "upstream branch deleted");

        let s = parse_git_status("# branch.oid (initial)\n# branch.head (detached)\n");
        assert_eq!((s.head, s.branch), (None, None));
    }

    #[test]
    fn source_plan_decisions() {
        let up = "# branch.upstream origin/main\n";
        let plan = |ab: &str, entries: &str, build: Option<&str>| {
            plan_source(&status(&format!("{up}{ab}"), entries), build)
        };
        let dirty = "1 .M N... 100644 100644 100644 aa bb src/a.rs\n? x\n";

        let p = plan("# branch.ab +0 -0\n", "", Some(OID));
        assert!(!p.available() && p.blocker.is_none(), "{p:?}");
        // Local edits don't matter while there is nothing to install.
        assert_eq!(plan("# branch.ab +0 -0\n", dirty, Some(OID)).blocker, None);
        // Unpushed local commits alone are not an update.
        assert!(!plan("# branch.ab +4 -0\n", "", Some(OID)).available());

        let p = plan("# branch.ab +0 -3\n", "", Some(OID));
        assert_eq!((p.behind, p.ready()), (3, true));
        assert_eq!(
            plan("# branch.ab +0 -3\n", dirty, Some(OID)).blocker,
            Some(Blocker::Dirty {
                changed: 1,
                untracked: 1
            })
        );
        assert_eq!(
            plan("# branch.ab +2 -3\n", "", Some(OID)).blocker,
            Some(Blocker::Diverged {
                ahead: 2,
                behind: 3
            })
        );
        // Pulled (or committed) but not rebuilt yet.
        let p = plan("# branch.ab +0 -0\n", "", Some("42ff02b"));
        assert!(p.rebuild && p.ready(), "{p:?}");
        assert!(!plan("# branch.ab +0 -0\n", "", None).available());

        assert_eq!(
            plan_source(&status("", ""), Some(OID)).blocker,
            Some(Blocker::NoUpstream {
                branch: "main".into()
            })
        );
        assert_eq!(
            plan_source(&status(up, ""), Some(OID)).blocker,
            Some(Blocker::UpstreamGone {
                upstream: "origin/main".into()
            })
        );
        let detached = parse_git_status(&format!("# branch.oid {OID}\n# branch.head (detached)\n"));
        assert_eq!(
            plan_source(&detached, Some(OID)).blocker,
            Some(Blocker::Detached)
        );
    }

    /// Real git: a bare "remote", a clone behind it, then diverged and dirty.
    #[test]
    fn source_check_against_real_git() {
        if cmd::which("git").is_none() {
            return;
        }
        let root = std::env::temp_dir().join(format!("hd-selfupdate-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let run = |dir: &Path, args: &[&str]| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args([
                    "-c",
                    "user.name=t",
                    "-c",
                    "user.email=t@t",
                    "-c",
                    "commit.gpgsign=false",
                ])
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        };
        let (remote, work, clone) = (
            root.join("remote.git"),
            root.join("work"),
            root.join("clone"),
        );
        run(
            &root,
            &[
                "init",
                "-q",
                "--bare",
                "-b",
                "main",
                remote.to_str().unwrap(),
            ],
        );
        run(
            &root,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                work.to_str().unwrap(),
            ],
        );
        let commit = |dir: &Path, msg: &str| {
            std::fs::write(dir.join("f"), msg).unwrap();
            run(dir, &["add", "f"]);
            run(dir, &["commit", "-q", "-m", msg]);
        };
        commit(&work, "one");
        run(&work, &["push", "-q", "origin", "main"]);
        run(
            &root,
            &[
                "clone",
                "-q",
                remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );

        let c = check_source(&clone, true, None).unwrap();
        assert!(!c.plan.available(), "{:?}", c.plan);
        assert_eq!(c.head.as_ref().map(|h| h.subject.as_str()), Some("one"));

        commit(&work, "two");
        commit(&work, "three");
        run(&work, &["push", "-q", "origin", "main"]);
        let c = check_source(&clone, true, None).unwrap();
        assert_eq!(c.fetch_error, None);
        assert_eq!(c.plan.behind, 2);
        let subjects: Vec<&str> = c.incoming.iter().map(|c| c.subject.as_str()).collect();
        assert_eq!(subjects, ["three", "two"]);

        std::fs::write(clone.join("scratch"), "x").unwrap();
        let c = check_source(&clone, false, None).unwrap();
        assert_eq!(
            c.plan.blocker,
            Some(Blocker::Dirty {
                changed: 0,
                untracked: 1
            })
        );
        std::fs::remove_file(clone.join("scratch")).unwrap();

        commit(&clone, "local");
        let c = check_source(&clone, false, None).unwrap();
        assert_eq!(
            c.plan.blocker,
            Some(Blocker::Diverged {
                ahead: 1,
                behind: 2
            })
        );

        run(&clone, &["checkout", "-q", "-b", "topic"]);
        let c = check_source(&clone, true, None).unwrap();
        assert_eq!(
            c.plan.blocker,
            Some(Blocker::NoUpstream {
                branch: "topic".into()
            })
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            url: format!("https://example.invalid/{name}"),
            size: 1,
        }
    }

    #[test]
    fn asset_selection_is_exact() {
        let all = [
            asset("Hyprdeck-aarch64.AppImage"),
            asset("Hyprdeck-x86_64.AppImage.zsync"),
            asset(CHECKSUM_ASSET),
            asset(APPIMAGE_ASSET),
        ];
        let sel = select_assets(&all).unwrap();
        assert_eq!(sel.appimage.name, APPIMAGE_ASSET);
        assert_eq!(
            sel.checksum.as_ref().map(|a| a.name.as_str()),
            Some(CHECKSUM_ASSET)
        );

        let sel = select_assets(&[asset(APPIMAGE_ASSET)]).unwrap();
        assert_eq!(sel.checksum, None);
        assert_eq!(
            select_assets(&[asset("hyprdeck-x86_64.appimage"), asset(CHECKSUM_ASSET)]),
            None
        );
        assert_eq!(select_assets(&[]), None);
    }

    #[test]
    fn sha256_lines() {
        let h = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        assert_eq!(
            parse_sha256(&format!("{h}  Hyprdeck-x86_64.AppImage\n")),
            Some(h.to_ascii_lowercase())
        );
        assert_eq!(parse_sha256(h), Some(h.to_ascii_lowercase()));
        assert_eq!(parse_sha256("abc  file"), None);
        assert_eq!(parse_sha256(""), None);
    }

    /// End-to-end replace via `file://` URLs (curl + sha256sum, no network).
    #[test]
    fn install_replaces_appimage_and_checks_hash() {
        let dir = std::env::temp_dir().join(format!("hd-selfupdate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(APPIMAGE_ASSET);
        let new = dir.join("new.AppImage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&new, b"new build").unwrap();
        let hash = cmd::run("sha256sum", [&new]).unwrap();
        let good = dir.join("good.sha256");
        let bad = dir.join("bad.sha256");
        std::fs::write(&good, &hash).unwrap();
        std::fs::write(&bad, format!("{}  x\n", "0".repeat(64))).unwrap();
        let url = |p: &Path| Asset {
            name: String::new(),
            url: format!("file://{}", p.display()),
            size: 0,
        };
        let assets = |sum: Option<&Path>| UpdateAssets {
            appimage: Asset {
                size: 9,
                ..url(&new)
            },
            checksum: sum.map(url),
        };

        let err = install_appimage(&target, &assets(Some(&bad))).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");

        assert!(install_appimage(&target, &assets(Some(&good))).unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"new build");
        assert_ne!(std::fs::metadata(&target).unwrap().mode() & 0o111, 0);
        assert!(!install_appimage(&target, &assets(None)).unwrap());

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.ends_with(".part")), "{names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
