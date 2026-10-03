//! Resume guard: watches logind `PrepareForSleep`, records diagnostics after
//! every wake into `~/.local/state/hyprdeck/resume/<time>.log` and resets the
//! displays when the wake left them dark (per [`RescuePolicy`]).

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::StreamExt;
use gtk::glib;
use hyprdeck_core::hypr::ctl;
use hyprdeck_core::{cmd, notify, rt, store};
use serde::Deserialize;

use crate::settings::{self, RescuePolicy};

/// Number of diagnostics files kept.
const KEEP: usize = 20;

/// Kernel log lines worth keeping in a wake report (lowercased match).
const KERNEL_TERMS: [&str; 12] = [
    "nvidia",
    "amdgpu",
    "i915",
    "drm",
    "frl",
    "xhci",
    "usb",
    "input",
    "hid",
    "pm: ",
    "acpi: pm",
    "openlinkhub",
];
/// User journal lines worth keeping (lowercased match): the compositor, common
/// shells/lockers/session managers and device daemons.
const USER_TERMS: [&str; 9] = [
    "hyprland",
    "hyprdeck",
    "hyprlock",
    "hypridle",
    "uwsm",
    "waybar",
    "noctalia",
    "quickshell",
    "openlinkhub",
];

#[zbus::proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1"
)]
trait Logind {
    fn inhibit(
        &self,
        what: &str,
        who: &str,
        why: &str,
        mode: &str,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;

    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// State captured just before sleep.
#[derive(Debug, Clone)]
pub struct SleepMark {
    /// Unix seconds.
    pub at: i64,
    /// Enabled outputs before sleep (expected back after wake).
    pub outputs: Vec<String>,
}

impl SleepMark {
    /// Blocking (one `hyprctl` call).
    pub fn capture() -> SleepMark {
        let outputs = ctl::monitors()
            .map(|ms| {
                ms.into_iter()
                    .filter(|m| !m.disabled)
                    .map(|m| m.name)
                    .collect()
            })
            .unwrap_or_default();
        SleepMark { at: now(), outputs }
    }
}

pub fn now() -> i64 {
    glib::real_time() / 1_000_000
}

fn local(secs: i64, fmt: &str) -> String {
    glib::DateTime::from_unix_local(secs)
        .and_then(|d| d.format(fmt))
        .map(|s| s.to_string())
        .unwrap_or_else(|_| secs.to_string())
}

pub fn dir() -> PathBuf {
    store::state_dir().join("resume")
}

/// Start the logind watcher (restarts itself on bus errors).
pub fn start() {
    rt::spawn(async {
        loop {
            if let Err(e) = watch().await {
                tracing::warn!("resume guard: {e:#}");
            }
            tokio::time::sleep(Duration::from_secs(30)).await;
        }
    });
}

async fn take_lock(mgr: &LogindProxy<'_>) -> Option<zbus::zvariant::OwnedFd> {
    match mgr
        .inhibit(
            "sleep",
            "hyprdeck",
            "Record the time of sleep for resume diagnostics",
            "delay",
        )
        .await
    {
        Ok(fd) => Some(fd),
        Err(e) => {
            tracing::warn!("resume guard: sleep inhibitor unavailable: {e}");
            None
        }
    }
}

async fn watch() -> Result<()> {
    let conn = zbus::Connection::system()
        .await
        .context("connecting to the system bus")?;
    let mgr = LogindProxy::new(&conn).await?;
    let mut signals = mgr.receive_prepare_for_sleep().await?;
    // A delay lock lets us snapshot the outputs before the system actually sleeps.
    let mut lock = take_lock(&mgr).await;
    let mut mark: Option<SleepMark> = None;
    while let Some(sig) = signals.next().await {
        let start = match sig.args() {
            Ok(a) => a.start,
            Err(e) => {
                tracing::warn!("resume guard: bad PrepareForSleep signal: {e}");
                continue;
            }
        };
        if start {
            mark = Some(tokio::task::spawn_blocking(SleepMark::capture).await?);
            drop(lock.take());
        } else {
            lock = take_lock(&mgr).await;
            let m = mark.take();
            tokio::spawn(async move {
                if let Err(e) = on_wake(m).await {
                    tracing::warn!("resume guard: {e:#}");
                }
            });
        }
    }
    anyhow::bail!("logind signal stream ended")
}

async fn on_wake(mark: Option<SleepMark>) -> Result<()> {
    let s = tokio::task::spawn_blocking(settings::load).await??.resume;
    if !s.enabled {
        return Ok(());
    }
    tokio::time::sleep(Duration::from_secs(u64::from(s.delay_secs))).await;
    let report = tokio::task::spawn_blocking(move || after_wake(mark, s.rescue, false)).await??;
    if s.notify
        && let Some((summary, body)) = report.notification()
    {
        notify::notify(&summary, &body).await?;
    }
    Ok(())
}

/// Minimal monitor view used by problem detection.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MonState {
    pub name: String,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default = "yes")]
    pub dpms_status: bool,
}

fn yes() -> bool {
    true
}

/// Display problems after a wake. `monitors` is `None` when `hyprctl` failed.
pub fn detect(
    kernel_lines: &[&str],
    monitors: Option<&[MonState]>,
    expected: &[String],
) -> Vec<String> {
    let mut problems = Vec::new();
    let frl = kernel_lines
        .iter()
        .filter(|l| l.contains("FRL link training failed"))
        .count();
    if frl > 0 {
        problems.push(format!(
            "HDMI FRL link training failed ({frl}×) — the monitor link did not come back"
        ));
    }
    match monitors {
        None => problems.push("Hyprland did not answer the monitor query".to_owned()),
        Some([]) => problems.push("Hyprland reports no monitors".to_owned()),
        Some(ms) => {
            for m in ms.iter().filter(|m| !m.disabled && !m.dpms_status) {
                problems.push(format!(
                    "{} is enabled but its display power (DPMS) is off",
                    m.name
                ));
            }
            for out in expected {
                if !ms.iter().any(|m| &m.name == out && !m.disabled) {
                    problems.push(format!(
                        "{out} was active before sleep but is missing after wake"
                    ));
                }
            }
        }
    }
    problems
}

fn filter_lines<'a>(text: &'a str, terms: &[&str]) -> Vec<&'a str> {
    text.lines()
        .filter(|l| {
            let low = l.to_ascii_lowercase();
            terms.iter().any(|t| low.contains(t))
        })
        .collect()
}

/// Outcome of one wake check.
#[derive(Debug, Clone)]
pub struct WakeReport {
    pub path: PathBuf,
    pub problems: Vec<String>,
    pub rescue: Rescue,
}

#[derive(Debug, Clone)]
pub enum Rescue {
    NotNeeded,
    Skipped,
    Done,
    Failed(String),
}

impl Rescue {
    fn describe(&self, policy: RescuePolicy) -> String {
        match self {
            Rescue::NotNeeded => "not needed".to_owned(),
            Rescue::Skipped => "skipped (rescue policy: never)".to_owned(),
            Rescue::Done if policy == RescuePolicy::Always => {
                "displays reset (rescue policy: always)".to_owned()
            }
            Rescue::Done => "displays reset".to_owned(),
            Rescue::Failed(e) => format!("failed: {e}"),
        }
    }
}

impl WakeReport {
    /// Desktop notification for this wake, if it warrants one.
    pub fn notification(&self) -> Option<(String, String)> {
        let problems = self.problems.join("\n");
        match &self.rescue {
            Rescue::NotNeeded => None,
            Rescue::Done if self.problems.is_empty() => Some((
                "Displays reset after wake".to_owned(),
                "The resume guard is set to always reset displays.".to_owned(),
            )),
            Rescue::Done => Some((
                "Display problem after wake — fixed".to_owned(),
                format!("{problems}\nDisplays were reset."),
            )),
            Rescue::Skipped => Some((
                "Display problem after wake".to_owned(),
                format!(
                    "{problems}\nIf the screen is black, use the rescue shortcut or run: hyprdeck display rescue"
                ),
            )),
            Rescue::Failed(e) => Some((
                "Display rescue failed".to_owned(),
                format!("{problems}\n{e}"),
            )),
        }
    }
}

/// Collect diagnostics, detect problems, apply the rescue policy and write the
/// report. Blocking; call a few seconds after the wake.
pub fn after_wake(
    mark: Option<SleepMark>,
    policy: RescuePolicy,
    simulated: bool,
) -> Result<WakeReport> {
    let woke = now();
    let (slept, expected) = match mark {
        Some(m) => (Some(m.at), m.outputs),
        None => (None, Vec::new()),
    };
    // Without a sleep mark (watcher started mid-sleep) look back two minutes.
    let since = format!("@{}", slept.unwrap_or(woke - 120));

    let monitors_raw = cmd::output("hyprctl", ["-j", "monitors", "all"]);
    let monitors: Option<Vec<MonState>> = monitors_raw
        .as_ref()
        .ok()
        .filter(|o| o.ok())
        .and_then(|o| serde_json::from_str(&o.stdout).ok());
    let rolling = ctl::rolling_log().unwrap_or_else(|e| format!("(unavailable: {e:#})"));
    let kernel = journal_text(&["-k", "--since", &since]);
    let user = journal_text(&["--user", "--since", &since]);
    let kernel_lines = section_lines(&kernel, &KERNEL_TERMS);
    let user_lines = section_lines(&user, &USER_TERMS);

    let problems = detect(&kernel_lines, monitors.as_deref(), &expected);
    let rescue = match policy {
        RescuePolicy::Never if !problems.is_empty() => Rescue::Skipped,
        RescuePolicy::Always => run_rescue(),
        RescuePolicy::OnProblem if !problems.is_empty() => run_rescue(),
        _ => Rescue::NotNeeded,
    };

    let mut text = String::from("hyprdeck resume diagnostics\n");
    if let Some(s) = slept {
        text.push_str(&format!("slept: {}\n", local(s, "%Y-%m-%d %H:%M:%S")));
    }
    text.push_str(&format!("woke: {}\n", local(woke, "%Y-%m-%d %H:%M:%S")));
    if simulated {
        text.push_str("simulated: yes\n");
    }
    for p in &problems {
        text.push_str(&format!("problem: {p}\n"));
    }
    text.push_str(&format!("rescue: {}\n", rescue.describe(policy)));
    text.push_str(&format!(
        "expected outputs: {}\n",
        if expected.is_empty() {
            "(unknown)".to_owned()
        } else {
            expected.join(", ")
        }
    ));

    text.push_str("\n=== hyprctl -j monitors all ===\n");
    match &monitors_raw {
        Ok(o) => text.push_str(if o.stdout.trim().is_empty() {
            o.stderr.trim()
        } else {
            o.stdout.trim()
        }),
        Err(e) => text.push_str(&format!("(failed: {e:#})")),
    }
    text.push_str("\n\n=== kernel log since sleep (filtered) ===\n");
    push_lines(&mut text, &kernel_lines);
    text.push_str("\n=== user journal since sleep (filtered) ===\n");
    push_lines(&mut text, &user_lines);
    text.push_str("\n=== hyprctl rollinglog ===\n");
    text.push_str(rolling.trim_end());
    text.push('\n');

    let dir = dir();
    let path = dir.join(format!("{}.log", local(woke, "%Y%m%d-%H%M%S")));
    store::write_atomic(&path, text.as_bytes())?;
    prune(&dir)?;
    Ok(WakeReport {
        path,
        problems,
        rescue,
    })
}

fn run_rescue() -> Rescue {
    match ctl::rescue_displays() {
        Ok(()) => Rescue::Done,
        Err(e) => Rescue::Failed(format!("{e:#}")),
    }
}

/// `journalctl <args>` output, or the failure as text.
fn journal_text(args: &[&str]) -> Result<String, String> {
    let mut all = args.to_vec();
    all.extend(["-o", "short-iso", "--no-pager", "-q"]);
    match cmd::output("journalctl", &all) {
        Ok(o) if !o.ok() && o.stdout.is_empty() => {
            Err(format!("(journalctl failed: {})", o.stderr.trim()))
        }
        Ok(o) => Ok(o.stdout),
        Err(e) => Err(format!("(journalctl failed: {e:#})")),
    }
}

fn section_lines<'a>(text: &'a Result<String, String>, terms: &[&str]) -> Vec<&'a str> {
    match text {
        Ok(t) => filter_lines(t, terms),
        Err(e) => vec![e.as_str()],
    }
}

fn push_lines(out: &mut String, lines: &[&str]) {
    if lines.is_empty() {
        out.push_str("(none)\n");
    }
    for l in lines {
        out.push_str(l);
        out.push('\n');
    }
}

fn log_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok().map(|e| e.path()))
                .filter(|p| p.extension().is_some_and(|x| x == "log"))
                .collect()
        })
        .unwrap_or_default();
    // Names are `%Y%m%d-%H%M%S`, so lexical order is chronological.
    files.sort();
    files
}

fn prune(dir: &Path) -> Result<()> {
    let files = log_files(dir);
    for old in &files[..files.len().saturating_sub(KEEP)] {
        std::fs::remove_file(old).with_context(|| format!("removing {}", old.display()))?;
    }
    Ok(())
}

/// Header of a saved wake report.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WakeLog {
    pub path: PathBuf,
    pub slept: Option<String>,
    pub woke: String,
    pub simulated: bool,
    pub problems: Vec<String>,
    pub rescue: String,
}

/// Parse the `key: value` header (up to the first blank line).
pub fn parse_header(path: PathBuf, text: &str) -> WakeLog {
    let mut log = WakeLog {
        path,
        ..Default::default()
    };
    for line in text.lines().skip(1).take_while(|l| !l.trim().is_empty()) {
        let Some((k, v)) = line.split_once(": ") else {
            continue;
        };
        match k {
            "slept" => log.slept = Some(v.to_owned()),
            "woke" => log.woke = v.to_owned(),
            "simulated" => log.simulated = v == "yes",
            "problem" => log.problems.push(v.to_owned()),
            "rescue" => log.rescue = v.to_owned(),
            _ => {}
        }
    }
    log
}

/// Saved wake reports, newest first. Blocking.
pub fn recent() -> Vec<WakeLog> {
    log_files(&dir())
        .into_iter()
        .rev()
        .filter_map(|p| {
            let text = std::fs::read_to_string(&p).ok()?;
            Some(parse_header(p, &text))
        })
        .collect()
}

/// Simulate a sleep/wake cycle without suspending (dev/test path).
pub fn simulate(policy: RescuePolicy, delay_secs: u64) -> Result<WakeReport> {
    let mark = SleepMark::capture();
    std::thread::sleep(Duration::from_secs(delay_secs));
    after_wake(Some(mark), policy, true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mon(name: &str, disabled: bool, dpms: bool) -> MonState {
        MonState {
            name: name.into(),
            disabled,
            dpms_status: dpms,
        }
    }

    #[test]
    fn healthy_wake_has_no_problems() {
        let ms = [mon("HDMI-A-1", false, true), mon("DP-2", true, false)];
        assert!(
            detect(
                &["xhci_hcd: xHC error in resume"],
                Some(&ms),
                &["HDMI-A-1".into()]
            )
            .is_empty()
        );
    }

    #[test]
    fn detects_each_problem() {
        let k = ["nvidia-modeset: WARNING: GPU:0: HDMI FRL link training failed."];
        let ms = [mon("HDMI-A-1", false, false)];
        let p = detect(&k, Some(&ms), &["HDMI-A-1".into(), "DP-1".into()]);
        assert_eq!(p.len(), 3, "{p:?}");
        assert!(p[0].contains("FRL"));
        assert!(p[1].contains("DPMS"));
        assert!(p[2].starts_with("DP-1"));
        assert_eq!(detect(&[], Some(&[]), &[]).len(), 1);
        assert_eq!(detect(&[], None, &[]).len(), 1);
    }

    #[test]
    fn parses_report_header() {
        let text = "hyprdeck resume diagnostics\nslept: 2026-10-03 09:00:00\nwoke: 2026-10-03 09:10:00\nsimulated: yes\n\
                    problem: a: b\nproblem: c\nrescue: displays reset\n\n=== x ===\nproblem: ignored\n";
        let log = parse_header(PathBuf::from("/x.log"), text);
        assert_eq!(log.slept.as_deref(), Some("2026-10-03 09:00:00"));
        assert_eq!(log.woke, "2026-10-03 09:10:00");
        assert!(log.simulated);
        assert_eq!(log.problems, ["a: b", "c"]);
        assert_eq!(log.rescue, "displays reset");
    }
}
