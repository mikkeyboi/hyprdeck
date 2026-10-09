//! The update engine: one privileged helper session (`pkexec`) for the
//! repository upgrade and installing AUR build dependencies / built packages,
//! unprivileged `makepkg` builds in between, progress events for the UI/CLI,
//! and the shared run state the dialog and tray observe.

use std::collections::BTreeSet;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use hyprdeck_core::{cmd, notify, rt, store, tray};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::helper::{Event as HelperEvent, Request};
use crate::pkgbuild::{self, Approval};
use crate::progress::{Failure, MakepkgProgress, Op, PacmanProgress, Phase};
use crate::{check, state};

const PACMAN_LOCK: &str = "/var/lib/pacman/db.lck";
/// How long to wait for another package manager before giving up.
const LOCK_WAIT: Duration = Duration::from_secs(10 * 60);
const LOCK_POLL: Duration = Duration::from_secs(2);
/// Saved update logs kept in the state dir.
const KEEP_LOGS: usize = 10;

// ---------------------------------------------------------------------------
// Plan, steps and results

/// An approved AUR package base to build and install.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AurJob {
    pub pkgbase: String,
    /// Installed package names from this base that get updated.
    pub names: Vec<String>,
    /// The reviewed commit; the build refuses to run anything else.
    pub commit: String,
    /// Recorded once the package is installed.
    pub approval: Approval,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub aur: Vec<AurJob>,
    /// AUR packages left out of this run, with the reason.
    pub skipped: Vec<(String, String)>,
    /// Interactive terminal command offered when something can't be done here.
    pub fallback: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StepState {
    Pending,
    Running,
    Done,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Step {
    pub title: String,
    pub state: StepState,
    pub detail: String,
}

/// Progress reported while the engine runs.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// The step list (sent first).
    Steps(Vec<Step>),
    Step {
        index: usize,
        state: StepState,
        detail: Option<String>,
    },
    /// Current activity ("Downloading 3 of 12: mesa").
    Phase(String),
    /// Overall progress 0..=1.
    Fraction(f64),
    Log(String),
    /// The privileged session started; cancelling is no longer possible.
    Privileged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Outcome {
    Success,
    /// The repository upgrade worked but some AUR packages failed or were skipped.
    Partial,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Summary {
    pub outcome: Outcome,
    pub headline: String,
    /// Why it failed and what to do.
    pub message: Option<String>,
    /// Package changes made by pacman (repository and AUR).
    pub changes: Vec<(Op, String)>,
    /// AUR package bases built and installed.
    pub aur_built: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub skipped: Vec<(String, String)>,
    /// `.pacnew`/`.pacsave` files created by this run.
    pub new_pacnew: Vec<String>,
    /// All `.pacnew`/`.pacsave` files waiting for a merge.
    pub pacnew: Vec<String>,
    /// Changed packages that make a restart advisable.
    pub restart: Vec<String>,
    pub log_path: Option<PathBuf>,
    /// Terminal command offered as the fallback.
    pub fallback: Option<String>,
    /// `pacman -Syu` completed.
    pub repo_upgraded: bool,
}

/// Packages whose update is only fully applied after a reboot.
pub fn needs_restart(name: &str) -> bool {
    let kernel = (name == "linux" || name.starts_with("linux-"))
        && !name.ends_with("-headers")
        && !name.ends_with("-docs");
    kernel
        || name.starts_with("nvidia")
        || name.starts_with("systemd")
        || matches!(name, "glibc" | "mesa")
}

// ---------------------------------------------------------------------------
// System access (injected for tests)

/// Why the privileged helper could not be started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartError {
    /// pkexec is not installed.
    Missing,
    /// No polkit authentication agent is running.
    NoAgent,
    /// The password prompt was dismissed or authentication failed.
    Denied,
    /// Cancelled from the app before authenticating.
    Cancelled,
    Failed(String),
}

impl StartError {
    pub fn explanation(&self) -> String {
        match self {
            StartError::Missing => "pkexec (polkit) is not installed, so Hyprdeck can't ask for your password. Install polkit or run the update in a terminal.".into(),
            StartError::NoAgent => "No polkit authentication agent is running, so no password prompt can be shown. Start one (e.g. hyprpolkitagent or polkit-gnome) or run the update in a terminal.".into(),
            StartError::Denied => "Authentication was cancelled or failed; nothing was changed.".into(),
            StartError::Cancelled => "Cancelled before anything was changed.".into(),
            StartError::Failed(e) => format!("The privileged update helper could not start: {e}"),
        }
    }
}

/// A running privileged helper session.
pub trait Helper {
    /// Send a request; output lines go to `line`. Returns pacman's exit status;
    /// `Err` when the helper refused the request or died.
    fn request(&mut self, request: &Request, line: &mut dyn FnMut(&str)) -> Result<i32>;
    fn close(&mut self);
}

/// Everything the engine does outside its own logic.
pub trait System {
    /// Start the helper through pkexec (shows the password prompt). Polls `cancel`.
    fn start_helper(&self, cancel: &AtomicBool) -> Result<Box<dyn Helper>, StartError>;
    /// Run an unprivileged command in `dir`; output lines go to `line`.
    fn run(
        &self,
        dir: &Path,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        line: &mut dyn FnMut(&str),
    ) -> Result<i32>;
    fn lock_held(&self) -> bool;
    fn sleep(&self, d: Duration);
    fn repo_dir(&self, pkgbase: &str) -> PathBuf;
    fn head(&self, dir: &Path) -> Result<String>;
    fn approve(&self, pkgbase: &str, approval: &Approval) -> Result<()>;
    /// Pending `.pacnew`/`.pacsave` files on the system.
    fn pacnew(&self) -> Vec<String>;
    fn save_log(&self, lines: &[String]) -> Option<PathBuf>;
    fn now(&self) -> i64;
}

// ---------------------------------------------------------------------------
// Engine

const AUTH: usize = 0;
const SYNC: usize = 1;
const DOWNLOAD: usize = 2;
const INSTALL: usize = 3;
const HOOKS: usize = 4;
const AUR_FIRST: usize = 5;

struct Engine<'a> {
    sys: &'a dyn System,
    emit: &'a mut dyn FnMut(Event),
    steps: Vec<StepState>,
    log: Vec<String>,
    units: f64,
    done_units: f64,
    fraction: f64,
}

impl Engine<'_> {
    fn step(&mut self, index: usize, state: StepState, detail: Option<String>) {
        self.steps[index] = state;
        (self.emit)(Event::Step {
            index,
            state,
            detail,
        });
    }

    fn phase(&mut self, text: impl Into<String>) {
        (self.emit)(Event::Phase(text.into()));
    }

    fn log(&mut self, line: &str) {
        self.log.push(line.to_owned());
        (self.emit)(Event::Log(line.to_owned()));
    }

    /// Progress within the current unit of work.
    fn progress(&mut self, within: f64) {
        let f = ((self.done_units + within.clamp(0.0, 1.0)) / self.units).clamp(0.0, 1.0);
        if f > self.fraction + 0.001 || (f >= 1.0 && self.fraction < 1.0) {
            self.fraction = f;
            (self.emit)(Event::Fraction(f));
        }
    }

    fn finish_unit(&mut self) {
        self.done_units += 1.0;
        self.progress(0.0);
    }

    /// Wait while another package manager holds the database lock.
    fn wait_for_lock(&mut self) -> Result<(), Failure> {
        let mut waited = Duration::ZERO;
        while self.sys.lock_held() {
            if waited.is_zero() {
                self.phase("Waiting for another package manager to finish…");
                self.log(&format!(
                    ":: {PACMAN_LOCK} exists; waiting for the other package manager"
                ));
            }
            if waited >= LOCK_WAIT {
                return Err(Failure::Locked);
            }
            self.sys.sleep(LOCK_POLL);
            waited += LOCK_POLL;
        }
        Ok(())
    }

    /// Run one helper request, tracking pacman progress. `on_progress` maps it to steps.
    fn transaction(
        &mut self,
        helper: &mut dyn Helper,
        request: &Request,
        progress: &mut PacmanProgress,
        on_progress: &mut dyn FnMut(&mut Self, &PacmanProgress),
    ) -> Result<(), Failure> {
        self.wait_for_lock()?;
        let start = self.log.len();
        let status = helper.request(request, &mut |line| {
            self.log(line);
            if progress.feed(line) {
                on_progress(self, progress);
            }
        });
        match status {
            Ok(0) => {
                progress.finish();
                on_progress(self, progress);
                Ok(())
            }
            Ok(_) => Err(crate::progress::classify_failure(&self.log[start..])),
            Err(e) => {
                let message = format!("{e:#}");
                self.log(&format!("error: {message}"));
                Err(Failure::Other { message })
            }
        }
    }

    /// Run an unprivileged command, logging its output.
    fn command(
        &mut self,
        dir: &Path,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        mut on_line: impl FnMut(&mut Self, &str),
    ) -> Result<i32> {
        let sys = self.sys;
        sys.run(dir, program, args, env, &mut |line| on_line(self, line))
    }

    /// Output lines of an unprivileged command (not logged).
    fn capture(&mut self, dir: &Path, program: &str, args: &[&str]) -> Result<(i32, Vec<String>)> {
        let mut lines = Vec::new();
        let status = self
            .sys
            .run(dir, program, args, &[("LC_ALL", "C")], &mut |l| {
                lines.push(l.to_owned())
            })?;
        Ok((status, lines))
    }
}

fn repo_step(phase: Phase) -> usize {
    match phase {
        Phase::Sync => SYNC,
        Phase::Resolve | Phase::Download => DOWNLOAD,
        Phase::Check | Phase::Install => INSTALL,
        Phase::Hooks | Phase::Done => HOOKS,
    }
}

/// Run `plan`: the repository upgrade, then each approved AUR package.
pub fn run(
    plan: &Plan,
    sys: &dyn System,
    cancel: &AtomicBool,
    emit: &mut dyn FnMut(Event),
) -> Summary {
    let mut titles = vec![
        "Authenticate".to_owned(),
        "Synchronize package databases".to_owned(),
        "Download packages".to_owned(),
        "Install upgrades".to_owned(),
        "Run hooks".to_owned(),
    ];
    for job in &plan.aur {
        titles.push(format!("Build {}", job.pkgbase));
        titles.push(format!("Install {}", job.pkgbase));
    }
    emit(Event::Steps(
        titles
            .iter()
            .map(|t| Step {
                title: t.clone(),
                state: StepState::Pending,
                detail: String::new(),
            })
            .collect(),
    ));
    let mut engine = Engine {
        sys,
        emit,
        steps: vec![StepState::Pending; titles.len()],
        log: Vec::new(),
        units: 1.0 + plan.aur.len() as f64,
        done_units: 0.0,
        fraction: 0.0,
    };
    let mut summary = Summary {
        outcome: Outcome::Success,
        headline: String::new(),
        message: None,
        changes: Vec::new(),
        aur_built: Vec::new(),
        failed: Vec::new(),
        skipped: plan.skipped.clone(),
        new_pacnew: Vec::new(),
        pacnew: Vec::new(),
        restart: Vec::new(),
        log_path: None,
        fallback: None,
        repo_upgraded: false,
    };

    let mut helper = if cancel.load(Ordering::SeqCst) {
        Err(StartError::Cancelled)
    } else {
        engine.step(
            AUTH,
            StepState::Running,
            Some("Waiting for your password".into()),
        );
        engine.phase("Waiting for authentication…");
        sys.start_helper(cancel)
    };
    let helper = match &mut helper {
        Ok(h) => h.as_mut(),
        Err(e) => {
            let e = e.clone();
            let cancelled = matches!(e, StartError::Cancelled | StartError::Denied);
            engine.step(
                AUTH,
                if cancelled {
                    StepState::Skipped
                } else {
                    StepState::Failed
                },
                Some(e.explanation()),
            );
            engine.log(&format!("error: {}", e.explanation()));
            for i in SYNC..titles.len() {
                engine.step(i, StepState::Skipped, None);
            }
            summary.outcome = if cancelled {
                Outcome::Cancelled
            } else {
                Outcome::Failed
            };
            summary.headline = if cancelled {
                "Update cancelled".into()
            } else {
                "Update could not start".into()
            };
            summary.message = Some(e.explanation());
            if !cancelled {
                summary.fallback = Some(plan.fallback.clone());
            }
            summary.log_path = sys.save_log(&engine.log);
            return summary;
        }
    };
    (engine.emit)(Event::Privileged);
    engine.step(AUTH, StepState::Done, Some(String::new()));

    // Repository upgrade.
    engine.step(SYNC, StepState::Running, None);
    engine.phase(Phase::Sync.label());
    let mut progress = PacmanProgress::new();
    let result = engine.transaction(helper, &Request::Upgrade, &mut progress, &mut |e, p| {
        if let Some(phase) = p.phase() {
            let current = repo_step(phase);
            for i in SYNC..current {
                if e.steps[i] != StepState::Done {
                    e.step(i, StepState::Done, None);
                }
            }
            if e.steps[current] != StepState::Running {
                e.step(current, StepState::Running, None);
            }
            e.phase(match p.detail() {
                d if d.is_empty() => phase.label().to_owned(),
                d => d,
            });
        }
        e.progress(p.fraction());
    });
    summary.changes.extend(progress.changes().iter().cloned());
    summary.new_pacnew.extend(progress.pacnew().iter().cloned());
    match result {
        Ok(()) => {
            summary.repo_upgraded = true;
            let nothing = progress.nothing_to_do();
            for i in SYNC..=HOOKS {
                if nothing && i > SYNC {
                    engine.step(i, StepState::Done, Some("Nothing to do".into()));
                } else if engine.steps[i] != StepState::Done {
                    engine.step(i, StepState::Done, None);
                }
            }
            engine.finish_unit();
        }
        Err(failure) => {
            let failed_step = (SYNC..=HOOKS)
                .find(|&i| engine.steps[i] == StepState::Running)
                .unwrap_or(SYNC);
            engine.step(failed_step, StepState::Failed, Some(failure.explanation()));
            for i in failed_step + 1..titles.len() {
                engine.step(i, StepState::Skipped, None);
            }
            for job in &plan.aur {
                summary
                    .skipped
                    .push((job.pkgbase.clone(), "the repository upgrade failed".into()));
            }
            summary.outcome = Outcome::Failed;
            summary.headline = "System update failed".into();
            summary.message = Some(failure.explanation());
            summary.fallback = Some(plan.fallback.clone());
        }
    }

    if summary.repo_upgraded {
        for (i, job) in plan.aur.iter().enumerate() {
            let build = AUR_FIRST + 2 * i;
            match aur_job(&mut engine, helper, job, build) {
                Ok(changes) => {
                    summary.changes.extend(changes);
                    summary.aur_built.push(job.pkgbase.clone());
                }
                Err(AurError::Skipped(reason)) => {
                    summary.skipped.push((job.pkgbase.clone(), reason));
                }
                Err(AurError::Failed(reason)) => {
                    summary.failed.push((job.pkgbase.clone(), reason));
                }
            }
            engine.finish_unit();
        }
    }
    helper.close();

    engine.progress(1.0);
    engine.phase(Phase::Done.label());
    summary.pacnew = sys.pacnew();
    for p in &summary.new_pacnew {
        if !summary.pacnew.contains(p) {
            summary.pacnew.push(p.clone());
        }
    }
    summary.restart = summary
        .changes
        .iter()
        .filter(|(op, name)| *op != Op::Remove && needs_restart(name))
        .map(|(_, name)| name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if summary.outcome == Outcome::Success {
        let left_out = summary.failed.len() + summary.skipped.len();
        if left_out > 0 {
            summary.outcome = Outcome::Partial;
            summary.headline = match left_out {
                1 => "System updated; 1 AUR package was not updated".into(),
                n => format!("System updated; {n} AUR packages were not updated"),
            };
            if summary.fallback.is_none() && !summary.failed.is_empty() {
                summary.fallback = Some(plan.fallback.clone());
            }
        } else if summary.changes.is_empty() {
            summary.headline = "Everything was already up to date".into();
        } else {
            summary.headline = "System updated".into();
        }
    }
    summary.log_path = sys.save_log(&engine.log);
    summary
}

enum AurError {
    Skipped(String),
    Failed(String),
}

fn aur_job(
    engine: &mut Engine<'_>,
    helper: &mut dyn Helper,
    job: &AurJob,
    build: usize,
) -> Result<Vec<(Op, String)>, AurError> {
    let install = build + 1;
    engine.step(build, StepState::Running, None);
    engine.phase(format!("Preparing {}", job.pkgbase));
    engine.log(&format!(":: Building {} (AUR)", job.pkgbase));
    let result = build_and_install(engine, helper, job, build, install);
    match &result {
        Ok(_) => {}
        Err(AurError::Skipped(reason)) => {
            engine.log(&format!(":: Skipping {}: {reason}", job.pkgbase));
            for i in [build, install] {
                if engine.steps[i] != StepState::Done {
                    engine.step(i, StepState::Skipped, Some(reason.clone()));
                }
            }
        }
        Err(AurError::Failed(reason)) => {
            engine.log(&format!("error: {}: {reason}", job.pkgbase));
            let failed = if engine.steps[install] == StepState::Running {
                install
            } else {
                build
            };
            engine.step(failed, StepState::Failed, Some(reason.clone()));
            if failed == build {
                engine.step(install, StepState::Skipped, None);
            }
        }
    }
    result
}

fn build_and_install(
    engine: &mut Engine<'_>,
    helper: &mut dyn Helper,
    job: &AurJob,
    build: usize,
    install: usize,
) -> Result<Vec<(Op, String)>, AurError> {
    let failed = |what: &str, e: anyhow::Error| AurError::Failed(format!("{what}: {e:#}"));
    let dir = engine.sys.repo_dir(&job.pkgbase);
    let head = engine
        .sys
        .head(&dir)
        .map_err(|e| failed("reading the reviewed repository", e))?;
    if head != job.commit {
        return Err(AurError::Failed(format!(
            "the repository changed since you reviewed it ({} → {}); review it again",
            crate::selfupdate::short(&job.commit),
            crate::selfupdate::short(&head)
        )));
    }

    // Only now, after approval, is the PKGBUILD executed (by makepkg).
    let (status, srcinfo) = engine
        .capture(&dir, "makepkg", &["--printsrcinfo"])
        .map_err(|e| failed("makepkg --printsrcinfo", e))?;
    if status != 0 {
        return Err(AurError::Failed(format!(
            "makepkg --printsrcinfo exited with {status}: {}",
            srcinfo.last().map_or("", String::as_str)
        )));
    }
    let srcinfo = pkgbuild::parse_srcinfo(&srcinfo.join("\n"));
    let version = srcinfo.version().unwrap_or_default();
    let deps = srcinfo.build_deps(&job.names, std::env::consts::ARCH);

    let mut repo_deps = Vec::new();
    if !deps.is_empty() {
        let mut args = vec!["-T", "--"];
        args.extend(deps.iter().map(String::as_str));
        let (status, missing) = engine
            .capture(&dir, "pacman", &args)
            .map_err(|e| failed("checking dependencies", e))?;
        if status != 0 && status != 127 {
            return Err(AurError::Failed(format!("pacman -T exited with {status}")));
        }
        let mut aur_only = Vec::new();
        for dep in missing.iter().map(|l| l.trim()).filter(|l| !l.is_empty()) {
            let name = pkgbuild::dep_name(dep);
            let in_repos = pkgbuild::valid_name(name)
                && engine
                    .capture(
                        &dir,
                        "pacman",
                        &["-Sddp", "--print-format", "%n", "--", name],
                    )
                    .is_ok_and(|(s, _)| s == 0);
            if in_repos {
                repo_deps.push(name.to_owned());
            } else {
                aur_only.push(dep.to_owned());
            }
        }
        if !aur_only.is_empty() {
            return Err(AurError::Skipped(format!(
                "needs {} from the AUR (or nowhere); AUR dependencies aren't built automatically yet — use the terminal fallback",
                aur_only.join(", ")
            )));
        }
    }
    if !repo_deps.is_empty() {
        engine.phase(format!("Installing build dependencies of {}", job.pkgbase));
        engine.step(
            build,
            StepState::Running,
            Some(format!("Installing dependencies: {}", repo_deps.join(", "))),
        );
        let mut progress = PacmanProgress::new();
        engine
            .transaction(
                helper,
                &Request::InstallDeps {
                    names: repo_deps.clone(),
                },
                &mut progress,
                &mut |e, p| e.progress(0.1 * p.fraction()),
            )
            .map_err(|f| AurError::Failed(f.explanation()))?;
    }

    let dir_str = dir.to_string_lossy().into_owned();
    let env = [("PKGDEST", dir_str.as_str())];
    let mut makepkg = MakepkgProgress::new();
    engine.phase(format!("Building {}", job.pkgbase));
    let status = engine
        .command(
            &dir,
            "makepkg",
            &["--noconfirm", "--needed", "-f", "--cleanbuild", "--nocolor"],
            &env,
            |e, line| {
                e.log(line);
                if makepkg.feed(line) {
                    if let Some(step) = makepkg.step() {
                        e.step(build, StepState::Running, Some(step.clone()));
                        e.phase(format!("Building {}: {step}", job.pkgbase));
                    }
                    e.progress(0.1 + 0.7 * makepkg.fraction());
                }
            },
        )
        .map_err(|e| failed("makepkg", e))?;
    if status != 0 {
        return Err(AurError::Failed(
            makepkg
                .error()
                .unwrap_or_else(|| format!("makepkg exited with {status}")),
        ));
    }
    let (status, list) = engine
        .capture(&dir, "makepkg", &["--packagelist"])
        .map_err(|e| failed("makepkg --packagelist", e))?;
    let names: Vec<&str> = job.names.iter().map(String::as_str).collect();
    let files: Vec<String> = list
        .into_iter()
        .filter(|f| {
            status == 0
                && pkgbuild::package_of(f, &names, &version).is_some()
                && Path::new(f).is_file()
        })
        .collect();
    if files.is_empty() {
        return Err(AurError::Failed(format!(
            "makepkg built no package for {}",
            job.names.join(", ")
        )));
    }
    engine.step(build, StepState::Done, Some(String::new()));
    engine.step(install, StepState::Running, None);
    engine.phase(format!("Installing {}", job.pkgbase));
    let mut progress = PacmanProgress::new();
    engine
        .transaction(
            helper,
            &Request::InstallBuilt { files },
            &mut progress,
            &mut |e, p| {
                e.progress(0.8 + 0.2 * p.fraction());
            },
        )
        .map_err(|f| AurError::Failed(f.explanation()))?;
    engine.step(install, StepState::Done, None);
    let approval = Approval {
        approved_at: engine.sys.now(),
        ..job.approval.clone()
    };
    if let Err(e) = engine.sys.approve(&job.pkgbase, &approval) {
        engine.log(&format!("warning: recording the approval failed: {e:#}"));
    }
    Ok(progress.changes().to_vec())
}

// ---------------------------------------------------------------------------
// Real system

/// Run `cmd`, passing each stdout/stderr line (lossy UTF-8; for `\r`-updated
/// progress lines only the final state) to `line`. Returns the exit status.
pub fn run_streaming(mut cmd: Command, line: &mut dyn FnMut(&str)) -> Result<i32> {
    let mut child = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("failed to start {:?}", cmd.get_program()))?;
    let (tx, rx) = mpsc::channel::<String>();
    let readers: Vec<_> = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|r| {
        let tx = tx.clone();
        std::thread::spawn(move || forward_lines(r, &tx))
    })
    .collect();
    drop(tx);
    for l in rx {
        line(&l);
    }
    for r in readers {
        let _ = r.join();
    }
    let status = child.wait()?;
    Ok(status.code().unwrap_or(-1))
}

fn forward_lines(r: impl Read, tx: &mpsc::Sender<String>) {
    for chunk in BufReader::new(r).split(b'\n') {
        let Ok(chunk) = chunk else { break };
        let text = String::from_utf8_lossy(&chunk);
        let text = text
            .trim_end_matches('\r')
            .rsplit('\r')
            .next()
            .unwrap_or_default();
        if tx.send(text.to_owned()).is_err() {
            break;
        }
    }
}

/// The program pkexec runs as the helper: the AppImage when running from one,
/// else this executable.
fn self_exe() -> Result<PathBuf> {
    if let Some(appimage) = std::env::var_os("APPIMAGE").map(PathBuf::from)
        && appimage.is_file()
    {
        return Ok(appimage);
    }
    std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("locating the hyprdeck executable")
}

pub struct RealSystem;

impl System for RealSystem {
    fn start_helper(&self, cancel: &AtomicBool) -> Result<Box<dyn Helper>, StartError> {
        let pkexec = cmd::which("pkexec").ok_or(StartError::Missing)?;
        let exe = self_exe().map_err(|e| StartError::Failed(format!("{e:#}")))?;
        let cache = pkgbuild::cache_dir();
        // The helper only accepts a cache (and hyprdeck dir above it) that no
        // other user can write, whatever the umask was when they were created.
        let private = |dir: &Path| {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        };
        std::fs::create_dir_all(&cache)
            .and_then(|()| private(&cache))
            .and_then(|()| cache.parent().map_or(Ok(()), private))
            .and_then(|()| cache.canonicalize())
            .map_err(|e| StartError::Failed(format!("{}: {e}", cache.display())))
            .and_then(|cache| PkexecHelper::start(&pkexec, &exe, &cache, cancel))
            .map(|h| Box::new(h) as Box<dyn Helper>)
    }

    fn run(
        &self,
        dir: &Path,
        program: &str,
        args: &[&str],
        env: &[(&str, &str)],
        line: &mut dyn FnMut(&str),
    ) -> Result<i32> {
        let mut cmd = Command::new(program);
        cmd.args(args)
            .current_dir(dir)
            .envs(env.iter().copied())
            .stdin(Stdio::null());
        run_streaming(cmd, line)
    }

    fn lock_held(&self) -> bool {
        Path::new(PACMAN_LOCK).exists()
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }

    fn repo_dir(&self, pkgbase: &str) -> PathBuf {
        pkgbuild::repo_dir(pkgbase)
    }

    fn head(&self, dir: &Path) -> Result<String> {
        pkgbuild::head(dir)
    }

    fn approve(&self, pkgbase: &str, approval: &Approval) -> Result<()> {
        pkgbuild::approve(pkgbase, approval.clone())
    }

    fn pacnew(&self) -> Vec<String> {
        if cmd::which("pacdiff").is_none() {
            return Vec::new();
        }
        cmd::output("pacdiff", ["--output"])
            .map(|o| {
                o.stdout
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_log(&self, lines: &[String]) -> Option<PathBuf> {
        let dir = store::state_dir();
        let path = dir.join(format!("system-update-{}.log", check::now()));
        let mut text = lines.join("\n");
        text.push('\n');
        if let Err(e) = store::write_atomic(&path, text.as_bytes()) {
            tracing::warn!("saving the update log: {e:#}");
            return None;
        }
        prune_logs(&dir);
        Some(path)
    }

    fn now(&self) -> i64 {
        check::now()
    }
}

fn prune_logs(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut logs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("system-update-") && n.ends_with(".log"))
        })
        .collect();
    logs.sort();
    let excess = logs.len().saturating_sub(KEEP_LOGS);
    for old in &logs[..excess] {
        let _ = std::fs::remove_file(old);
    }
}

/// `pkexec <self> updates root-helper …` with its stdin/stdout pipes.
struct PkexecHelper {
    child: Child,
    stdin: Option<ChildStdin>,
    events: Receiver<String>,
    stderr: Arc<Mutex<String>>,
}

impl PkexecHelper {
    fn start(
        pkexec: &str,
        exe: &Path,
        cache: &Path,
        cancel: &AtomicBool,
    ) -> Result<Self, StartError> {
        // SAFETY: getuid has no preconditions.
        let uid = unsafe { libc::getuid() };
        let mut child = Command::new(pkexec)
            .arg(exe)
            .args([
                "updates",
                "root-helper",
                "--uid",
                &uid.to_string(),
                "--cache",
            ])
            .arg(cache)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| StartError::Failed(format!("starting pkexec: {e}")))?;
        let (tx, events) = mpsc::channel();
        let stdout = child.stdout.take().expect("piped stdout");
        std::thread::spawn(move || forward_lines(stdout, &tx));
        let stderr = Arc::new(Mutex::new(String::new()));
        let err_buf = stderr.clone();
        let err_pipe = child.stderr.take().expect("piped stderr");
        std::thread::spawn(move || {
            for line in BufReader::new(err_pipe).lines() {
                let Ok(line) = line else { break };
                let mut buf = err_buf.lock().unwrap_or_else(|e| e.into_inner());
                buf.push_str(&line);
                buf.push('\n');
            }
        });
        let stdin = child.stdin.take();
        let mut helper = PkexecHelper {
            child,
            stdin,
            events,
            stderr,
        };
        loop {
            if cancel.load(Ordering::SeqCst) {
                let _ = helper.child.kill();
                let _ = helper.child.wait();
                return Err(StartError::Cancelled);
            }
            match helper.events.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    if let Ok(HelperEvent::Ready { .. }) = serde_json::from_str(&line) {
                        return Ok(helper);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => {
                    let status = helper.child.wait().ok().and_then(|s| s.code());
                    // Let the stderr reader drain.
                    std::thread::sleep(Duration::from_millis(50));
                    let stderr = helper.stderr_text();
                    return Err(start_error(status, &stderr));
                }
            }
        }
    }

    fn stderr_text(&self) -> String {
        self.stderr
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .trim()
            .to_owned()
    }
}

/// Map pkexec's exit status and stderr to a reason.
fn start_error(status: Option<i32>, stderr: &str) -> StartError {
    if stderr.contains("No authentication agent found") {
        return StartError::NoAgent;
    }
    match status {
        Some(126) => StartError::Denied,
        Some(127) if stderr.contains("Not authorized") || stderr.is_empty() => StartError::Denied,
        Some(code) => StartError::Failed(if stderr.is_empty() {
            format!("pkexec exited with {code}")
        } else {
            stderr.trim_start_matches("Error: ").to_owned()
        }),
        None => StartError::Failed("the helper was killed".into()),
    }
}

impl Helper for PkexecHelper {
    fn request(&mut self, request: &Request, line: &mut dyn FnMut(&str)) -> Result<i32> {
        let stdin = self.stdin.as_mut().context("helper session closed")?;
        let mut json = serde_json::to_string(request)?;
        json.push('\n');
        stdin
            .write_all(json.as_bytes())
            .and_then(|()| stdin.flush())
            .context("the update helper exited")?;
        for raw in self.events.iter() {
            match serde_json::from_str::<HelperEvent>(&raw) {
                Ok(HelperEvent::Line { text }) => line(&text),
                Ok(HelperEvent::Done { status }) => return Ok(status),
                Ok(HelperEvent::Rejected { error }) => bail!("the update helper refused: {error}"),
                Ok(HelperEvent::Ready { .. }) => {}
                // Not protocol (e.g. a log line); show it rather than drop it.
                Err(_) => line(&raw),
            }
        }
        Err(anyhow!(
            "the update helper exited unexpectedly: {}",
            self.stderr_text()
        ))
    }

    fn close(&mut self) {
        if let Some(mut stdin) = self.stdin.take() {
            let mut json = serde_json::to_string(&Request::Exit).unwrap_or_default();
            json.push('\n');
            let _ = stdin.write_all(json.as_bytes());
        }
        let _ = self.child.wait();
    }
}

impl Drop for PkexecHelper {
    fn drop(&mut self) {
        self.close();
    }
}

// ---------------------------------------------------------------------------
// Bookkeeping shared by the UI and the CLI

#[derive(Debug, Default, Serialize, Deserialize)]
struct History {
    /// Unix seconds of the last completed repository upgrade.
    last_success: Option<i64>,
}

fn history_path() -> PathBuf {
    store::state_dir().join("system-update.json")
}

/// When the last hyprdeck system update completed. Blocking.
pub fn last_success() -> Option<i64> {
    std::fs::read_to_string(history_path())
        .ok()
        .and_then(|t| serde_json::from_str::<History>(&t).ok())
        .and_then(|h| h.last_success)
}

/// Record a completed run (blocking).
pub fn record(summary: &Summary, now: i64) {
    if !summary.repo_upgraded {
        return;
    }
    let history = History {
        last_success: Some(now),
    };
    let saved = serde_json::to_vec_pretty(&history)
        .map_err(anyhow::Error::from)
        .and_then(|json| store::write_atomic(&history_path(), &json));
    if let Err(e) = saved {
        tracing::warn!("recording the system update: {e:#}");
    }
}

/// Excludes concurrent system updates (app and CLI).
pub struct RunLock {
    _file: std::fs::File,
}

pub fn lock() -> Result<RunLock> {
    let dir = store::state_dir();
    std::fs::create_dir_all(&dir).context("creating the state directory")?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(dir.join("system-update.lock"))
        .context("opening the update lock")?;
    match file.try_lock() {
        Ok(()) => Ok(RunLock { _file: file }),
        Err(std::fs::TryLockError::WouldBlock) => bail!("a system update is already running"),
        Err(std::fs::TryLockError::Error(e)) => Err(anyhow!(e).context("locking the update")),
    }
}

// ---------------------------------------------------------------------------
// Shared run state (app)

#[derive(Clone, Default)]
pub struct RunState {
    pub active: bool,
    pub steps: Vec<Step>,
    pub phase: String,
    pub fraction: f64,
    pub log: Vec<String>,
    /// Cancelling is possible (before the privileged session starts).
    pub cancellable: bool,
    pub summary: Option<Arc<Summary>>,
}

static RUN: LazyLock<watch::Sender<RunState>> =
    LazyLock::new(|| watch::Sender::new(RunState::default()));
static CANCEL: AtomicBool = AtomicBool::new(false);

pub fn subscribe() -> watch::Receiver<RunState> {
    RUN.subscribe()
}

pub fn active() -> bool {
    RUN.borrow().active
}

/// Cancel the run if it hasn't reached the privileged session yet.
pub fn cancel() {
    if RUN.borrow().cancellable {
        CANCEL.store(true, Ordering::SeqCst);
    }
}

/// Forget a finished run (the dialog shows the review again).
pub fn reset() {
    RUN.send_if_modified(|s| {
        if s.active {
            return false;
        }
        *s = RunState::default();
        true
    });
}

fn apply_event(s: &mut RunState, event: Event) {
    match event {
        Event::Steps(steps) => s.steps = steps,
        Event::Step {
            index,
            state,
            detail,
        } => {
            if let Some(step) = s.steps.get_mut(index) {
                step.state = state;
                if let Some(d) = detail {
                    step.detail = d;
                }
            }
        }
        Event::Phase(p) => s.phase = p,
        Event::Fraction(f) => s.fraction = f,
        Event::Log(l) => s.log.push(l),
        Event::Privileged => s.cancellable = false,
    }
}

/// Start `plan` in the background; progress is published to [`subscribe`].
pub fn start(plan: Plan) -> Result<()> {
    let lock = lock()?;
    let started = RUN.send_if_modified(|s| {
        if s.active {
            return false;
        }
        *s = RunState {
            active: true,
            cancellable: true,
            phase: "Starting…".into(),
            ..RunState::default()
        };
        true
    });
    if !started {
        bail!("a system update is already running");
    }
    CANCEL.store(false, Ordering::SeqCst);
    tray::refresh();
    rt::spawn(async move {
        let summary = rt::blocking(move || {
            let _lock = lock;
            let summary = run(&plan, &RealSystem, &CANCEL, &mut |e| {
                RUN.send_modify(|s| apply_event(s, e));
            });
            record(&summary, check::now());
            summary
        })
        .await;
        let summary = Arc::new(summary);
        RUN.send_modify(|s| {
            s.active = false;
            s.cancellable = false;
            s.fraction = 1.0;
            s.summary = Some(summary.clone());
        });
        tray::refresh();
        if summary.outcome != Outcome::Cancelled {
            state::trigger(false);
        }
        if summary.outcome != Outcome::Cancelled {
            let body = summary_line(&summary);
            let severity = if summary.outcome == Outcome::Success {
                notify::Severity::Normal
            } else {
                notify::Severity::Error
            };
            if let Ok(true) = notify::notify_action(
                notify::Category::PackageUpdates,
                severity,
                &summary.headline,
                &body,
                "Show details",
            )
            .await
            {
                state::request_review();
            }
        }
    });
    Ok(())
}

/// One-line description of a finished run (notifications, CLI).
pub fn summary_line(s: &Summary) -> String {
    let mut parts = Vec::new();
    let changed = s.changes.iter().filter(|(op, _)| *op != Op::Remove).count();
    if changed > 0 {
        parts.push(format!("{changed} packages updated"));
    }
    if !s.failed.is_empty() {
        parts.push(format!("{} failed", s.failed.len()));
    }
    if !s.skipped.is_empty() {
        parts.push(format!("{} skipped", s.skipped.len()));
    }
    if !s.restart.is_empty() {
        parts.push("restart recommended".into());
    }
    if let Some(m) = &s.message {
        parts.push(m.clone());
    }
    parts.join(" · ")
}

/// Tray label while an in-app update runs.
pub fn tray_label() -> Option<String> {
    let s = RUN.borrow();
    s.active
        .then(|| "Updating system… (show progress)".to_owned())
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::rc::Rc;

    use super::*;

    /// Scripted helper responses: output lines and exit status per request.
    type Script = Rc<RefCell<VecDeque<(Vec<String>, i32)>>>;

    const UPGRADE: &[&str] = &[
        ":: Synchronizing package databases...",
        " core downloading...",
        " extra downloading...",
        ":: Starting full system upgrade...",
        "resolving dependencies...",
        "looking for conflicting packages...",
        "",
        "Packages (2) linux-7.2.8-2  mesa-1:26.2.4-1",
        "",
        "Total Download Size:   120.00 MiB",
        "Total Installed Size:  300.00 MiB",
        "Net Upgrade Size:        1.00 MiB",
        "",
        ":: Proceed with installation? [Y/n] ",
        ":: Retrieving packages...",
        " linux-7.2.8-2-x86_64.pkg.tar.zst downloading...",
        " mesa-1:26.2.4-1-x86_64.pkg.tar.zst downloading...",
        "checking keyring...",
        "checking package integrity...",
        "loading package files...",
        "checking for file conflicts...",
        "checking available disk space...",
        ":: Processing package changes...",
        "upgrading linux...",
        "upgrading mesa...",
        ":: Running post-transaction hooks...",
        "(1/2) Updating linux initcpios...",
        "(2/2) Arming ConditionNeedsUpdate...",
    ];

    struct FakeHelper {
        script: Script,
        requests: Rc<RefCell<Vec<Request>>>,
    }
    impl Helper for FakeHelper {
        fn request(&mut self, request: &Request, line: &mut dyn FnMut(&str)) -> Result<i32> {
            self.requests.borrow_mut().push(request.clone());
            let (lines, status) = self
                .script
                .borrow_mut()
                .pop_front()
                .expect("unexpected request");
            for l in lines {
                line(&l);
            }
            Ok(status)
        }

        fn close(&mut self) {}
    }

    #[derive(Default)]
    struct Fake {
        start: Option<StartError>,
        head: String,
        /// Responses to helper requests, in order.
        script: Script,
        requests: Rc<RefCell<Vec<Request>>>,
        commands: RefCell<Vec<String>>,
        /// `pacman -T` output.
        missing: Vec<&'static str>,
        /// Names `pacman -Sddp` resolves.
        repo: Vec<&'static str>,
        build_status: i32,
        approved: RefCell<Vec<(String, Approval)>>,
        lock_polls: RefCell<u32>,
        dir: PathBuf,
    }

    impl System for Fake {
        fn start_helper(&self, _: &AtomicBool) -> Result<Box<dyn Helper>, StartError> {
            match &self.start {
                Some(e) => Err(e.clone()),
                None => Ok(Box::new(FakeHelper {
                    script: self.script.clone(),
                    requests: self.requests.clone(),
                })),
            }
        }

        fn run(
            &self,
            _dir: &Path,
            program: &str,
            args: &[&str],
            env: &[(&str, &str)],
            line: &mut dyn FnMut(&str),
        ) -> Result<i32> {
            let cmdline = format!("{program} {}", args.join(" "));
            self.commands.borrow_mut().push(cmdline.clone());
            let out = |lines: &[&str], line: &mut dyn FnMut(&str)| {
                for l in lines {
                    line(l);
                }
            };
            Ok(match (program, args) {
                ("makepkg", ["--printsrcinfo"]) => {
                    out(
                        &[
                            "pkgbase = foo",
                            "\tpkgver = 2.0",
                            "\tpkgrel = 1",
                            "\tmakedepends = cmake",
                            "\tmakedepends = aur-thing>=1",
                            "\tdepends = glibc",
                            "pkgname = foo",
                            "pkgname = foo-extra",
                        ],
                        line,
                    );
                    0
                }
                ("pacman", ["-T", ..]) => {
                    out(&self.missing, line);
                    if self.missing.is_empty() { 0 } else { 127 }
                }
                ("pacman", ["-Sddp", .., name]) => {
                    if self.repo.contains(name) {
                        out(&[name], line);
                        0
                    } else {
                        1
                    }
                }
                ("makepkg", ["--packagelist"]) => {
                    let dir = self.dir.display();
                    for f in ["foo", "foo-extra"] {
                        line(&format!("{dir}/{f}-2.0-1-x86_64.pkg.tar.zst"));
                    }
                    0
                }
                ("makepkg", _) => {
                    assert_eq!(env, [("PKGDEST", self.dir.to_str().unwrap())]);
                    assert!(args.contains(&"--cleanbuild") && !args.contains(&"-s"));
                    out(
                        &[
                            "==> Making package: foo 2.0-1",
                            "==> Starting build()...",
                            "==> Finished making: foo 2.0-1",
                        ],
                        line,
                    );
                    self.build_status
                }
                _ => panic!("unexpected command {cmdline}"),
            })
        }

        fn lock_held(&self) -> bool {
            let mut polls = self.lock_polls.borrow_mut();
            if *polls > 0 {
                *polls -= 1;
                true
            } else {
                false
            }
        }

        fn sleep(&self, _: Duration) {}

        fn repo_dir(&self, _: &str) -> PathBuf {
            self.dir.clone()
        }

        fn head(&self, _: &Path) -> Result<String> {
            Ok(self.head.clone())
        }

        fn approve(&self, pkgbase: &str, approval: &Approval) -> Result<()> {
            self.approved
                .borrow_mut()
                .push((pkgbase.to_owned(), approval.clone()));
            Ok(())
        }

        fn pacnew(&self) -> Vec<String> {
            vec!["/etc/pacman.conf.pacnew".into()]
        }

        fn save_log(&self, _: &[String]) -> Option<PathBuf> {
            None
        }

        fn now(&self) -> i64 {
            1000
        }
    }

    fn lines(l: &[&str]) -> Vec<String> {
        l.iter().map(|s| (*s).to_owned()).collect()
    }

    fn job() -> AurJob {
        AurJob {
            pkgbase: "foo".into(),
            names: vec!["foo".into()],
            commit: "abc".into(),
            approval: Approval {
                commit: "abc".into(),
                maintainer: Some("m".into()),
                source_hosts: vec!["example.org".into()],
                approved_at: 0,
                findings: Vec::new(),
            },
        }
    }

    fn fake(dir: &str) -> Fake {
        let dir = std::env::temp_dir().join(format!("hd-apply-{dir}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for f in ["foo", "foo-extra"] {
            std::fs::write(dir.join(format!("{f}-2.0-1-x86_64.pkg.tar.zst")), "").unwrap();
        }
        Fake {
            head: "abc".into(),
            repo: vec!["cmake"],
            dir,
            ..Fake::default()
        }
    }

    fn execute(plan: &Plan, sys: &Fake) -> (Summary, Vec<Event>) {
        let mut events = Vec::new();
        let summary = run(plan, sys, &AtomicBool::new(false), &mut |e| events.push(e));
        let _ = std::fs::remove_dir_all(&sys.dir);
        (summary, events)
    }

    fn final_states(events: &[Event]) -> Vec<StepState> {
        let mut state = RunState::default();
        for e in events {
            apply_event(&mut state, e.clone());
        }
        state.steps.iter().map(|s| s.state).collect()
    }

    #[test]
    fn upgrade_then_build_and_install_aur_package() {
        let sys = fake("ok");
        let sys = Fake {
            missing: vec!["cmake"],
            lock_polls: RefCell::new(2),
            ..sys
        };
        sys.script.borrow_mut().extend([
            (lines(UPGRADE), 0),
            (lines(&["installing cmake..."]), 0),
            (
                lines(&[
                    "loading packages...",
                    "resolving dependencies...",
                    "looking for conflicting packages...",
                    "",
                    "Packages (1) foo-2.0-1",
                    "",
                    ":: Proceed with installation? [Y/n] ",
                    "checking keyring...",
                    "checking package integrity...",
                    "loading package files...",
                    "checking for file conflicts...",
                    ":: Processing package changes...",
                    "upgrading foo...",
                ]),
                0,
            ),
        ]);
        let plan = Plan {
            aur: vec![job()],
            skipped: vec![("bar".into(), "not approved".into())],
            fallback: "paru -Syu".into(),
        };
        let (s, events) = execute(&plan, &sys);
        assert_eq!(s.outcome, Outcome::Partial, "{s:?}");
        assert!(s.repo_upgraded);
        assert_eq!(s.aur_built, ["foo"]);
        assert_eq!(s.skipped, [("bar".to_owned(), "not approved".to_owned())]);
        assert_eq!(s.restart, ["linux", "mesa"]);
        assert_eq!(s.pacnew, ["/etc/pacman.conf.pacnew"]);
        assert!(s.changes.contains(&(Op::Upgrade, "foo".into())));
        let requests = sys.requests.borrow();
        assert_eq!(requests[0], Request::Upgrade);
        assert_eq!(
            requests[1],
            Request::InstallDeps {
                names: vec!["cmake".into()]
            }
        );
        // Only the installed package of the split base is installed.
        let Request::InstallBuilt { files } = &requests[2] else {
            panic!("{:?}", requests[2])
        };
        assert_eq!(files.len(), 1);
        assert!(files[0].ends_with("/foo-2.0-1-x86_64.pkg.tar.zst"));
        assert_eq!(sys.approved.borrow()[0].1.approved_at, 1000);
        assert_eq!(final_states(&events), [StepState::Done; 7]);
        let fractions: Vec<f64> = events
            .iter()
            .filter_map(|e| match e {
                Event::Fraction(f) => Some(*f),
                _ => None,
            })
            .collect();
        assert!(fractions.windows(2).all(|w| w[0] <= w[1]));
        assert_eq!(fractions.last(), Some(&1.0));
        assert!(events.contains(&Event::Privileged));
        assert!(events.iter().any(
            |e| matches!(e, Event::Phase(p) if p == "Waiting for another package manager to finish…")
        ));
    }

    #[test]
    fn aur_only_dependencies_skip_the_package() {
        let sys = Fake {
            missing: vec!["cmake", "aur-thing>=1"],
            ..fake("aurdep")
        };
        sys.script.borrow_mut().push_back((lines(UPGRADE), 0));
        let plan = Plan {
            aur: vec![job()],
            fallback: "paru -Syu".into(),
            ..Plan::default()
        };
        let (s, events) = execute(&plan, &sys);
        assert_eq!(s.outcome, Outcome::Partial);
        assert_eq!(s.skipped.len(), 1);
        assert!(s.skipped[0].1.contains("aur-thing>=1"), "{:?}", s.skipped);
        assert_eq!(sys.requests.borrow().len(), 1, "nothing installed");
        assert!(sys.approved.borrow().is_empty());
        assert!(
            !sys.commands
                .borrow()
                .iter()
                .any(|c| c.contains("--cleanbuild"))
        );
        let states = final_states(&events);
        assert_eq!(states[5..], [StepState::Skipped, StepState::Skipped]);
    }

    #[test]
    fn changed_repository_is_not_built() {
        let sys = Fake {
            head: "def".into(),
            ..fake("changed")
        };
        sys.script.borrow_mut().push_back((lines(UPGRADE), 0));
        let plan = Plan {
            aur: vec![job()],
            ..Plan::default()
        };
        let (s, _) = execute(&plan, &sys);
        assert_eq!(s.failed.len(), 1);
        assert!(s.failed[0].1.contains("changed since you reviewed"));
        assert!(sys.commands.borrow().is_empty(), "no makepkg ran");
    }

    #[test]
    fn failed_build_is_reported() {
        let sys = Fake {
            build_status: 4,
            ..fake("buildfail")
        };
        sys.script.borrow_mut().push_back((lines(UPGRADE), 0));
        let plan = Plan {
            aur: vec![job()],
            fallback: "paru -Syu".into(),
            ..Plan::default()
        };
        let (s, events) = execute(&plan, &sys);
        assert_eq!(s.failed.len(), 1);
        assert_eq!(s.fallback.as_deref(), Some("paru -Syu"));
        assert_eq!(
            final_states(&events)[5..],
            [StepState::Failed, StepState::Skipped]
        );
    }

    #[test]
    fn repo_failure_stops_the_run() {
        let sys = fake("repofail");
        sys.script.borrow_mut().push_back((
            lines(&[
                ":: Synchronizing package databases...",
                ":: Starting full system upgrade...",
                "resolving dependencies...",
                "looking for conflicting packages...",
                ":: foo-ng and foo are in conflict. Remove foo? [y/N] ",
                "error: unresolvable package conflicts detected",
                "error: failed to prepare transaction (conflicting dependencies)",
                ":: foo-ng and foo are in conflict",
            ]),
            1,
        ));
        let plan = Plan {
            aur: vec![job()],
            fallback: "paru -Syu".into(),
            ..Plan::default()
        };
        let (s, events) = execute(&plan, &sys);
        assert_eq!(s.outcome, Outcome::Failed);
        assert!(!s.repo_upgraded);
        assert_eq!(s.fallback.as_deref(), Some("paru -Syu"));
        assert!(
            s.message.as_deref().unwrap().contains("Remove foo?"),
            "{s:?}"
        );
        assert_eq!(sys.requests.borrow().len(), 1);
        let states = final_states(&events);
        assert!(states.contains(&StepState::Failed));
        assert_eq!(states[5..], [StepState::Skipped, StepState::Skipped]);
    }

    #[test]
    fn helper_start_failures() {
        for (err, outcome) in [
            (StartError::Denied, Outcome::Cancelled),
            (StartError::NoAgent, Outcome::Failed),
            (StartError::Missing, Outcome::Failed),
        ] {
            let sys = Fake {
                start: Some(err.clone()),
                ..fake("start")
            };
            let plan = Plan {
                fallback: "sudo pacman -Syu".into(),
                ..Plan::default()
            };
            let (s, _) = execute(&plan, &sys);
            assert_eq!(s.outcome, outcome, "{err:?}");
            assert_eq!(s.message, Some(err.explanation()));
            assert_eq!(s.fallback.is_some(), outcome == Outcome::Failed);
        }
        assert_eq!(
            start_error(
                Some(127),
                "Error executing command as another user: No authentication agent found.\n"
            ),
            StartError::NoAgent
        );
        assert_eq!(start_error(Some(126), ""), StartError::Denied);
        assert_eq!(
            start_error(Some(1), "Error: the update helper must run as root"),
            StartError::Failed("the update helper must run as root".into())
        );
    }

    #[test]
    fn restart_packages() {
        for n in [
            "linux",
            "linux-zen",
            "linux-firmware",
            "nvidia-utils",
            "systemd",
            "systemd-libs",
            "glibc",
            "mesa",
        ] {
            assert!(needs_restart(n), "{n}");
        }
        for n in [
            "linux-headers",
            "linux-api-headers",
            "mesa-utils",
            "kitty",
            "lib32-glibc",
        ] {
            assert!(!needs_restart(n), "{n}");
        }
    }
}
