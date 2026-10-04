//! hyprdeck self-update at runtime: the last check, the running update job,
//! restart handling and the background notify/auto policy. Shared by the page,
//! the tray and the scheduler.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::bail;
use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::tray::{self, TrayItem};
use hyprdeck_core::{notify, rt, store, ui};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::check;
use crate::selfupdate::{
    self, Applied, Mode, Policy, Progress, RELEASES_URL, Restart, RestartPlan, SelfCheck,
};
use crate::state;

pub const TRAY_INSTALL: &str = "updates.self.install";
pub const TRAY_RESTART: &str = "updates.self.restart";
/// How often a pending restart looks for the window being hidden.
const RESTART_POLL: Duration = Duration::from_secs(3);
/// Build log lines kept for display.
const LOG_LINES: usize = 40;

/// Result of the last self-update check.
pub struct Checked {
    pub at: i64,
    pub mode: Mode,
    pub result: Result<SelfCheck, String>,
}

#[derive(Debug, Clone, Default)]
pub enum Job {
    #[default]
    Idle,
    /// Current step.
    Running(String),
    /// Installed; waiting to restart into it.
    RestartPending {
        label: String,
        restart: Restart,
    },
    Done(String),
    Failed(String),
}
impl Job {
    pub fn is_running(&self) -> bool {
        matches!(self, Job::Running(_))
    }

    pub fn restart_pending(&self) -> bool {
        matches!(self, Job::RestartPending { .. })
    }

    fn busy(&self) -> bool {
        self.is_running() || self.restart_pending()
    }
}

#[derive(Clone, Default)]
pub struct SelfState {
    pub last: Option<Arc<Checked>>,
    pub checking: bool,
    pub job: Job,
    /// Tail of the source build log (source mode).
    pub log: Option<String>,
}

/// Who asked for an update; decides checksum strictness and result notifications.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Page,
    Tray,
    Notification,
    Auto,
}

static STATE: LazyLock<watch::Sender<SelfState>> =
    LazyLock::new(|| watch::Sender::new(SelfState::default()));

pub fn subscribe() -> watch::Receiver<SelfState> {
    STATE.subscribe()
}

pub fn current() -> SelfState {
    STATE.borrow().clone()
}

fn set_job(job: Job) {
    STATE.send_modify(|s| s.job = job);
    tray::refresh();
}

/// Remembered across restarts in `self-update.json` (state dir).
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
struct Memo {
    /// Last update announced (release tag or commit).
    announced: Option<String>,
    /// Last update an automatic install failed for (failure notified once).
    failed: Option<String>,
    /// A source build unit was started at this time and its result not reported yet.
    source_job: Option<i64>,
}

static MEMO: Mutex<()> = Mutex::new(());
/// An automatic source update waits for the window to be closed.
static AUTO_DEFERRED: AtomicBool = AtomicBool::new(false);

/// Read-modify-write the memo (blocking, tiny file).
fn with_memo<T>(f: impl FnOnce(&mut Memo) -> T) -> T {
    let _guard = MEMO.lock().unwrap_or_else(|e| e.into_inner());
    let path = store::state_dir().join("self-update.json");
    let mut memo: Memo = std::fs::read_to_string(&path)
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let out = f(&mut memo);
    match serde_json::to_vec_pretty(&memo) {
        Ok(json) => {
            if let Err(e) = store::write_atomic(&path, &json) {
                tracing::warn!("saving self-update state: {e:#}");
            }
        }
        Err(e) => tracing::warn!("serializing self-update state: {e}"),
    }
    out
}

/// Check for a hyprdeck update unless a check is running. `force` bypasses the
/// GitHub cache; `background` applies the notify/auto policy to the result.
pub fn check(force: bool, background: bool) {
    if !STATE.send_if_modified(|s| !std::mem::replace(&mut s.checking, true)) {
        return;
    }
    rt::spawn(async move {
        let channel = state::settings().self_update_channel;
        let checked = rt::blocking(move || {
            let (now, mode) = (check::now(), selfupdate::mode());
            let result = SelfCheck::run(&mode, channel, now, force).map_err(|e| format!("{e:#}"));
            Checked {
                at: now,
                mode,
                result,
            }
        })
        .await;
        let checked = Arc::new(checked);
        STATE.send_modify(|s| {
            s.last = Some(checked.clone());
            s.checking = false;
        });
        tray::refresh();
        if background {
            apply_policy(&checked).await;
        }
    });
}

async fn apply_policy(checked: &Checked) {
    let policy = state::settings().self_update_policy;
    let Some(offer) = checked.result.as_ref().ok().and_then(SelfCheck::offer) else {
        return;
    };
    if policy == Policy::Off || current().job.busy() {
        return;
    }
    let id = offer.id.clone();
    let fresh = rt::blocking(move || {
        with_memo(|m| m.announced.replace(id.clone()).is_none_or(|old| old != id))
    })
    .await;
    match (policy, offer.blocker) {
        // Retried at every periodic check; failures are announced once.
        // install.sh restarts the service, so a source build waits for the window to close.
        (Policy::Auto, None) if matches!(checked.mode, Mode::Source(_)) && ui::window_visible() => {
            if !AUTO_DEFERRED.swap(true, Ordering::SeqCst) {
                rt::spawn(async {
                    while ui::window_visible() {
                        tokio::time::sleep(RESTART_POLL).await;
                    }
                    AUTO_DEFERRED.store(false, Ordering::SeqCst);
                    update(Origin::Auto);
                });
            }
        }
        (Policy::Auto, None) => update(Origin::Auto),
        _ if !fresh => {}
        (_, Some(blocker)) => {
            let body = format!("{}\n{}", offer.detail, blocker.message());
            rt::spawn(async move {
                if let Ok(true) = notify::notify_action(&offer.title, &body, "Open Hyprdeck").await
                {
                    events::send(AppEvent::ShowPage("updates".into()));
                }
            });
        }
        (_, None) => {
            rt::spawn(async move {
                match notify::notify_action(&offer.title, &offer.detail, "Update now").await {
                    Ok(true) => update(Origin::Notification),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("update notification failed: {e:#}"),
                }
            });
        }
    }
}

struct StateProgress;

impl Progress for StateProgress {
    fn step(&self, msg: &str) {
        set_job(Job::Running(msg.to_owned()));
    }

    fn log(&self, text: &str) {
        STATE.send_modify(|s| {
            let log = s.log.get_or_insert_default();
            log.push_str(text);
            let lines = log.lines().count();
            if lines > LOG_LINES {
                *log = log
                    .lines()
                    .skip(lines - LOG_LINES)
                    .collect::<Vec<_>>()
                    .join("\n");
                log.push('\n');
            }
        });
    }

    fn job_started(&self) {
        with_memo(|m| m.source_job = Some(check::now()));
    }
}

/// Download/build and install the available update, then restart per
/// [`selfupdate::restart_plan`]. No-op while another update runs or awaits restart.
pub fn update(origin: Origin) {
    let started = STATE.send_if_modified(|s| {
        if s.job.busy() {
            return false;
        }
        s.job = Job::Running("Starting…".into());
        s.log = None;
        true
    });
    if started {
        tray::refresh();
        rt::spawn(run(origin));
    }
}

async fn run(origin: Origin) {
    let channel = state::settings().self_update_channel;
    let (result, source, report) = rt::blocking(move || {
        let progress = StateProgress;
        let mode = selfupdate::mode();
        let source = matches!(mode, Mode::Source(_));
        let result = selfupdate::lock().and_then(|_lock| match &mode {
            Mode::AppImage(path) => {
                progress.step("Checking for updates…");
                let c = selfupdate::check_appimage(path, channel, check::now(), false)?;
                selfupdate::apply_appimage(&c, origin == Origin::Auto, &progress)
            }
            Mode::Source(dir) => selfupdate::apply_source(dir, &progress),
            Mode::Installed => {
                bail!("this installation can't update itself; get new versions from {RELEASES_URL}")
            }
        });
        // A restarted hyprdeck may already have reported the build's result.
        let report = !source || with_memo(|m| m.source_job.take()).is_some() || result.is_err();
        (result, source, report)
    })
    .await;
    if source {
        let log = rt::blocking(|| selfupdate::log_tail(LOG_LINES)).await;
        STATE.send_modify(|s| s.log = log);
    }
    let notify = report && origin != Origin::Page;
    match result {
        Ok(applied) => installed(applied, notify),
        Err(e) => {
            let msg = format!("{e:#}");
            tracing::warn!("hyprdeck update failed: {msg}");
            set_job(Job::Failed(msg.clone()));
            let announce = notify
                && (origin != Origin::Auto || {
                    let id = current()
                        .last
                        .as_ref()
                        .and_then(|c| c.result.as_ref().ok()?.offer())
                        .map(|o| o.id);
                    rt::blocking(move || {
                        with_memo(|m| std::mem::replace(&mut m.failed, id.clone()) != id)
                    })
                    .await
                });
            if announce {
                notify::notify_bg("Hyprdeck update failed", msg);
            }
        }
    }
}

fn installed(applied: Applied, notify: bool) {
    let Applied {
        label,
        message,
        restart,
    } = applied;
    tracing::info!("hyprdeck update: {message}");
    set_job(Job::RestartPending {
        label: label.clone(),
        restart,
    });
    match selfupdate::restart_plan(ui::window_visible()) {
        RestartPlan::Now => {
            if notify {
                notify::notify_bg(
                    format!("Hyprdeck {label} installed"),
                    "Restarting Hyprdeck…",
                );
            }
            restart_now();
        }
        RestartPlan::WhenHidden => {
            rt::spawn(async move {
                let ask = notify::notify_action(
                    &format!("Hyprdeck {label} is installed"),
                    "Restart Hyprdeck to finish updating. It restarts by itself once the window is closed.",
                    "Restart now",
                )
                .await;
                if let Ok(true) = ask {
                    restart_now();
                }
            });
            rt::spawn(async {
                loop {
                    tokio::time::sleep(RESTART_POLL).await;
                    if !matches!(current().job, Job::RestartPending { .. }) {
                        break;
                    }
                    if selfupdate::restart_plan(ui::window_visible()) == RestartPlan::Now {
                        restart_now();
                        break;
                    }
                }
            });
        }
    }
}

/// Restart into an installed update (no-op unless one is pending).
pub fn restart_now() {
    let mut target = None;
    STATE.send_if_modified(|s| match &s.job {
        Job::RestartPending { restart, .. } => {
            target = Some(restart.clone());
            s.job = Job::Running("Restarting Hyprdeck…".into());
            true
        }
        _ => false,
    });
    let Some(restart) = target else { return };
    tray::refresh();
    rt::spawn(async move {
        // Only returns on failure or when systemd is about to replace this process.
        if let Err(e) = rt::blocking(move || selfupdate::restart(&restart)).await {
            let msg = format!("Restart failed: {e:#}");
            tracing::warn!("{msg}");
            set_job(Job::Failed(msg.clone()));
            notify::notify_bg("Hyprdeck update", msg);
        }
    });
}

/// The tray entry for hyprdeck itself, when there is something to do.
pub fn tray_item() -> Option<TrayItem> {
    let s = STATE.borrow();
    match &s.job {
        Job::Running(_) => Some(TrayItem::Action {
            label: "Updating Hyprdeck…".into(),
            id: TRAY_INSTALL.into(),
            enabled: false,
        }),
        Job::RestartPending { .. } => Some(TrayItem::action(
            "Restart to finish updating Hyprdeck",
            TRAY_RESTART,
        )),
        Job::Idle | Job::Done(_) | Job::Failed(_) => {
            if state::settings().self_update_policy == Policy::Off {
                return None;
            }
            let offer = s.last.as_ref()?.result.as_ref().ok()?.offer()?;
            offer
                .blocker
                .is_none()
                .then(|| TrayItem::action(offer.action, TRAY_INSTALL))
        }
    }
}

/// Report a source build that outlived the process that started it (its
/// `install.sh` restarts the service).
pub fn start() {
    rt::spawn(async {
        if rt::blocking(|| with_memo(|m| m.source_job)).await.is_none() {
            return;
        }
        STATE.send_modify(|s| {
            s.job = Job::Running("Finishing the update build…".into());
            s.log = None;
        });
        let result =
            rt::blocking(|| selfupdate::wait_job(&StateProgress, selfupdate::JOB_TIMEOUT)).await;
        let (report, log) = rt::blocking(|| {
            (
                with_memo(|m| m.source_job.take()).is_some(),
                selfupdate::log_tail(LOG_LINES),
            )
        })
        .await;
        STATE.send_modify(|s| s.log = log);
        let job = match result {
            Ok(true) => Job::Done(format!("Updated to {}", hyprdeck_core::version_string())),
            Ok(false) => Job::Failed(format!(
                "install.sh failed; see {}",
                selfupdate::log_path().display()
            )),
            Err(e) => Job::Failed(format!("{e:#}")),
        };
        if report {
            match &job {
                Job::Done(msg) => notify::notify_bg("Hyprdeck updated", msg.clone()),
                Job::Failed(msg) => notify::notify_bg("Hyprdeck update failed", msg.clone()),
                _ => {}
            }
        }
        set_job(job);
    });
}
