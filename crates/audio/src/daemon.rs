//! Background controller: watches PipeWire (`pactl subscribe` + 15 s safety poll),
//! keeps routing rules applied, restores the simultaneous output when it should be
//! on, and publishes [`Status`] snapshots to the UI and tray.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::Duration;

use anyhow::Result;
use hyprdeck_core::{notify, rt};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{Notify, watch};

use crate::engine::{self, Status};

/// Latest snapshot: `None` until the first read, `Err` when PipeWire is unreachable.
pub type Snapshot = Option<Arc<Result<Status, String>>>;

static SNAPSHOT: LazyLock<watch::Sender<Snapshot>> = LazyLock::new(|| watch::channel(None).0);
static REFRESH: LazyLock<Notify> = LazyLock::new(Notify::new);
static APPLY: AtomicBool = AtomicBool::new(false);
static STARTED: AtomicBool = AtomicBool::new(false);
/// Output set at the last failed restore; retried only when outputs change.
static RESTORE_FAILED_FOR: Mutex<Option<Vec<String>>> = Mutex::new(None);
/// Serializes snapshot production so publishes stay ordered.
static SNAPSHOT_RUN: LazyLock<tokio::sync::Mutex<()>> =
    LazyLock::new(|| tokio::sync::Mutex::new(()));
const DEBOUNCE: Duration = Duration::from_millis(180);
const POLL: Duration = Duration::from_secs(15);

pub fn subscribe() -> watch::Receiver<Snapshot> {
    SNAPSHOT.subscribe()
}

pub fn current() -> Snapshot {
    SNAPSHOT.borrow().clone()
}

/// Debounced refresh; `apply_routes` also moves new streams onto their rules.
pub fn request_refresh(apply_routes: bool) {
    if apply_routes {
        APPLY.store(true, Ordering::Relaxed);
    }
    REFRESH.notify_one();
}

/// Start the watcher once (idempotent). Must be called with no GTK requirement.
pub fn start() {
    if STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    rt::spawn(async {
        // Startup: restore the group if it should be on, sync and apply every rule.
        restore_if_needed().await;
        if let Err(e) = engine::reconcile_routes().await {
            tracing::warn!("routing rules: {e:#}");
        }
        refresh_now().await;
        refresh_loop().await;
    });
    rt::spawn(monitor());
    rt::spawn(async {
        let mut tick = tokio::time::interval(POLL);
        tick.tick().await;
        loop {
            tick.tick().await;
            // Safety net: also re-apply rules in case a move was dropped without an event.
            request_refresh(true);
        }
    });
}

async fn refresh_loop() {
    loop {
        REFRESH.notified().await;
        // Debounce: restart the timer while events keep arriving.
        loop {
            tokio::select! {
                () = REFRESH.notified() => continue,
                () = tokio::time::sleep(DEBOUNCE) => break,
            }
        }
        if APPLY.swap(false, Ordering::Relaxed)
            && let Err(e) = engine::apply_routes().await
        {
            tracing::warn!("applying routing rules: {e:#}");
        }
        restore_if_needed().await;
        refresh_now().await;
    }
}

/// Read and publish a fresh snapshot immediately.
pub async fn refresh_now() {
    let _g = SNAPSHOT_RUN.lock().await;
    let snap = engine::status().await.map_err(|e| format!("{e:#}"));
    if let Err(e) = &snap {
        tracing::warn!("audio status: {e}");
    }
    let snap = Arc::new(snap);
    SNAPSHOT.send_replace(Some(snap));
    crate::tray::changed();
}

async fn restore_if_needed() {
    let Ok(Ok(sinks)) =
        tokio::time::timeout(Duration::from_secs(8), crate::pw::list_sinks("")).await
    else {
        return;
    };
    let key: Vec<String> = sinks
        .iter()
        .filter(|s| !s.is_virtual())
        .map(|s| s.name.clone())
        .collect();
    if RESTORE_FAILED_FOR
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        == Some(&key)
    {
        return;
    }
    match engine::restore().await {
        Ok(Some(msg)) => {
            tracing::info!("{msg}");
            notify_if_enabled(msg).await;
        }
        Ok(None) => {}
        Err(e) => {
            tracing::warn!("restoring simultaneous output: {e:#}");
            *RESTORE_FAILED_FOR.lock().unwrap_or_else(|e| e.into_inner()) = Some(key);
            notify_if_enabled(format!("Could not restore simultaneous output: {e:#}")).await;
        }
    }
}

async fn notify_if_enabled(body: String) {
    let enabled = tokio::task::spawn_blocking(crate::config::load)
        .await
        .ok()
        .and_then(Result::ok)
        .is_none_or(|c| c.notifications);
    if enabled {
        notify::notify_bg("Audio", body);
    }
}

/// Classify a `pactl subscribe` line: `Some(apply_routes)` when a refresh is needed.
pub fn classify_event(line: &str) -> Option<bool> {
    if line.contains("sink-input") {
        Some(line.contains("'new'") || line.contains("'change'"))
    } else if line.contains("sink") || line.contains("server") || line.contains("card") {
        Some(false)
    } else {
        None
    }
}

async fn monitor() {
    let mut missing_logged = false;
    loop {
        let mut cmd = Command::new("pactl");
        cmd.arg("subscribe")
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .kill_on_drop(true);
        // SAFETY: prctl is async-signal-safe; ties the watcher's lifetime to ours.
        unsafe {
            cmd.pre_exec(|| {
                libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM);
                Ok(())
            });
        }
        match cmd.spawn() {
            Ok(mut child) => {
                if let Some(out) = child.stdout.take() {
                    let mut lines = BufReader::new(out).lines();
                    while let Ok(Some(line)) = lines.next_line().await {
                        if let Some(apply) = classify_event(&line) {
                            request_refresh(apply);
                        }
                    }
                }
                let _ = child.kill().await;
                tracing::info!("pactl subscribe ended; restarting");
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No pipewire-pulse/pactl: snapshots report it; check again now and then.
                if !std::mem::replace(&mut missing_logged, true) {
                    tracing::warn!("pactl not found; audio features need pipewire-pulse");
                }
                request_refresh(false);
                tokio::time::sleep(Duration::from_secs(30)).await;
                continue;
            }
            Err(e) => tracing::warn!("pactl subscribe: {e}"),
        }
        missing_logged = false;
        // Server restart: refresh (restores the group) and retry shortly.
        *RESTORE_FAILED_FOR.lock().unwrap_or_else(|e| e.into_inner()) = None;
        request_refresh(true);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// Run an engine operation from the UI: returns its message after a fresh snapshot is published.
pub async fn ui_action<F>(fut: F) -> Result<String>
where
    F: Future<Output = Result<String>> + Send + 'static,
{
    rt::run(async move {
        let r = fut.await;
        refresh_now().await;
        r
    })
    .await
}

/// Run an engine operation from the tray (non-GTK thread); reports via desktop notification.
pub fn tray_action<F>(fut: F)
where
    F: Future<Output = Result<String>> + Send + 'static,
{
    rt::spawn(async move {
        let r = fut.await;
        refresh_now().await;
        match r {
            Ok(msg) if !msg.is_empty() => notify_if_enabled(msg).await,
            Ok(_) => {}
            Err(e) => {
                tracing::warn!("{e:#}");
                notify_if_enabled(format!("{e:#}")).await;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::classify_event;

    #[test]
    fn subscribe_lines_are_classified() {
        assert_eq!(classify_event("Event 'new' on sink-input #12"), Some(true));
        assert_eq!(
            classify_event("Event 'change' on sink-input #12"),
            Some(true)
        );
        assert_eq!(
            classify_event("Event 'remove' on sink-input #12"),
            Some(false)
        );
        assert_eq!(classify_event("Event 'change' on sink #66"), Some(false));
        assert_eq!(
            classify_event("Event 'change' on server #4294967295"),
            Some(false)
        );
        assert_eq!(classify_event("Event 'change' on card #48"), Some(false));
        assert_eq!(classify_event("Event 'new' on source-output #9"), None);
        assert_eq!(classify_event("Event 'change' on client #3"), None);
    }
}
