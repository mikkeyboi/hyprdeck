//! Shared check state, settings, the periodic background checker and the tray entry.

use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, SystemTime};

use hyprdeck_core::events::{self, AppEvent};
use hyprdeck_core::tray::{self, TrayItem, TrayProvider};
use hyprdeck_core::{notify, rt, store};
use serde::{Deserialize, Serialize};
use tokio::sync::watch;

use crate::check::{self, Report};
use crate::selfstate::{self, Origin};
use crate::selfupdate::{Channel, Policy};

/// `~/.config/hyprdeck/updates.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    /// Hours between automatic background checks.
    pub interval_hours: u32,
    /// Desktop notification when the number of pending updates grows.
    pub notify: bool,
    /// What background checks do about new hyprdeck versions.
    pub self_update_policy: Policy,
    /// Which AppImage builds to follow (source installs follow their branch).
    pub self_update_channel: Channel,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            interval_hours: 6,
            notify: true,
            self_update_policy: Policy::default(),
            self_update_channel: Channel::default(),
        }
    }
}

const SETTINGS_NAME: &str = "updates";
/// Delay before the first automatic check after startup.
const FIRST_CHECK: i64 = 120;
const TICK: Duration = Duration::from_secs(20);
const PACMAN_LOCAL_DB: &str = "/var/lib/pacman/local";
const PACMAN_LOCK: &str = "/var/lib/pacman/db.lck";

static SETTINGS: LazyLock<Mutex<Settings>> = LazyLock::new(|| {
    Mutex::new(store::load(SETTINGS_NAME).unwrap_or_else(|e| {
        tracing::warn!("updates settings: {e:#}");
        Settings::default()
    }))
});

pub fn settings() -> Settings {
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner())
}

/// Persist settings (blocking file write).
pub fn save_settings(s: Settings) -> anyhow::Result<()> {
    store::save(SETTINGS_NAME, &s)?;
    *SETTINGS.lock().unwrap_or_else(|e| e.into_inner()) = s;
    Ok(())
}

#[derive(Clone, Default)]
pub struct State {
    pub report: Option<Arc<Report>>,
    pub checking: bool,
}

static STATE: LazyLock<watch::Sender<State>> = LazyLock::new(|| {
    watch::Sender::new(State {
        report: check::load_report().map(Arc::new),
        checking: false,
    })
});

pub fn subscribe() -> watch::Receiver<State> {
    STATE.subscribe()
}

pub fn current() -> State {
    STATE.borrow().clone()
}

/// Start a check unless one is already running. `notify` allows a desktop
/// notification when the update count grew. Callable from any thread.
pub fn trigger(notify: bool) {
    let started = STATE.send_if_modified(|s| !std::mem::replace(&mut s.checking, true));
    if !started {
        return;
    }
    rt::spawn(async move {
        let report = rt::blocking(check::run_check).await;
        publish(report, notify);
    });
}

fn publish(report: Report, allow_notify: bool) {
    if let Err(e) = check::save_report(&report) {
        tracing::warn!("saving update report: {e:#}");
    }
    let previous = STATE.borrow().report.as_ref().map(|r| r.total());
    let total = report.total();
    if allow_notify && settings().notify && !report.failed() && total > previous.unwrap_or(0) {
        let (summary, body) = (notification_summary(total), notification_body(&report));
        rt::spawn(async move {
            match notify::notify_action(&summary, &body, "Review & update").await {
                Ok(true) => request_review(),
                Ok(false) => {}
                Err(e) => tracing::warn!("update notification failed: {e:#}"),
            }
        });
    }
    STATE.send_modify(|s| {
        s.report = Some(Arc::new(report));
        s.checking = false;
    });
    tray::refresh();
}

fn notification_summary(total: usize) -> String {
    if total == 1 {
        "1 package update available".into()
    } else {
        format!("{total} package updates available")
    }
}

fn notification_body(report: &Report) -> String {
    let mut names: Vec<&str> = report.important().map(|u| u.name.as_str()).collect();
    names.extend(
        report
            .repo
            .iter()
            .chain(&report.aur)
            .filter(|u| !u.important)
            .map(|u| u.name.as_str()),
    );
    let shown = names.len().min(4);
    let mut body = names[..shown].join(", ");
    if names.len() > shown {
        body.push_str(&format!(" and {} more", names.len() - shown));
    }
    body
}

fn pacman_db_stamp() -> Option<SystemTime> {
    std::fs::metadata(PACMAN_LOCAL_DB)
        .and_then(|m| m.modified())
        .ok()
}

/// Periodic checks plus a re-check whenever a pacman transaction finishes
/// (e.g. after "Update everything" in the terminal; without pacman the stamp never changes).
async fn scheduler() {
    let start = check::now();
    let mut db_stamp = pacman_db_stamp();
    loop {
        tokio::time::sleep(TICK).await;
        let state = current();
        if state.checking {
            continue;
        }
        // An in-app update re-checks by itself when it finishes.
        if crate::apply::active() {
            db_stamp = pacman_db_stamp();
            continue;
        }
        let stamp = pacman_db_stamp();
        if stamp != db_stamp && !std::path::Path::new(PACMAN_LOCK).exists() {
            db_stamp = stamp;
            trigger(false);
            continue;
        }
        let now = check::now();
        let due = match state
            .report
            .as_ref()
            .map(|r| r.checked_at)
            .filter(|&t| t >= start)
        {
            Some(last) => last + i64::from(settings().interval_hours.max(1)) * 3600,
            None => start + FIRST_CHECK,
        };
        if now >= due {
            trigger(true);
            if settings().self_update_policy != Policy::Off {
                selfstate::check(false, true);
            }
        }
    }
}

struct UpdatesTray;

impl TrayProvider for UpdatesTray {
    fn items(&self) -> Vec<TrayItem> {
        let state = STATE.borrow();
        let label = match (&state.report, state.checking) {
            _ if crate::apply::active() => crate::apply::tray_label().unwrap_or_default(),
            (Some(r), _) if r.total() > 0 => format!("Updates: {} available", r.total()),
            (Some(r), _) if r.failed() => "Updates: check failed".into(),
            (Some(r), _) if !r.tooling.pacman => "Updates: upstream releases".into(),
            (Some(_), _) => "System up to date".into(),
            (None, true) => "Updates: checking…".into(),
            (None, false) => "Updates: not checked yet".into(),
        };
        let pending = state.report.as_ref().is_some_and(|r| r.total() > 0);
        let id = if pending || crate::apply::active() {
            TRAY_REVIEW
        } else {
            "updates.show"
        };
        let mut items = vec![TrayItem::action(label, id)];
        items.extend(selfstate::tray_item());
        items
    }

    fn activate(&self, id: &str) {
        match id {
            "updates.show" => events::send(AppEvent::ShowPage("updates".into())),
            TRAY_REVIEW => request_review(),
            selfstate::TRAY_INSTALL => selfstate::update(Origin::Tray),
            selfstate::TRAY_RESTART => selfstate::restart_now(),
            _ => {}
        }
    }
}

/// Tray entry that opens the update review (or the running update).
const TRAY_REVIEW: &str = "updates.review";

/// `true` while a request to open the update dialog is pending.
static REVIEW: LazyLock<watch::Sender<bool>> = LazyLock::new(|| watch::Sender::new(false));

/// Show the updates page and open the update dialog (from the tray or a
/// notification). Callable from any thread.
pub fn request_review() {
    REVIEW.send_replace(true);
    events::send(AppEvent::ShowPage("updates".into()));
}

pub fn review_requests() -> watch::Receiver<bool> {
    REVIEW.subscribe()
}

/// Consume a pending review request.
pub fn take_review_request() -> bool {
    REVIEW.send_if_modified(|pending| std::mem::replace(pending, false))
}

pub fn start() {
    tray::register(Arc::new(UpdatesTray));
    rt::spawn(scheduler());
    selfstate::start();
}
