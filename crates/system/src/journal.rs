//! Sleep/wake evidence from the system journal across recent boots: wakes,
//! HDMI FRL link-training failures, xHCI re-inits and power-button presses
//! shortly after a wake (a forced shutdown on a black screen).

use anyhow::{Context, Result};
use hyprdeck_core::cmd;
use serde::Deserialize;

/// `journalctl -g` pattern selecting every message [`classify`] understands.
const PATTERN: &str =
    "PM: suspend (entry|exit)|FRL link training failed|xHC error in resume|Power key pressed";
/// An FRL failure this soon after a wake is attributed to that wake.
pub const FRL_WINDOW_SECS: i64 = 120;
/// A power-button press this soon after a wake suggests the screen stayed black.
pub const POWER_WINDOW_SECS: i64 = 300;
/// How many boots of history to examine.
pub const MAX_BOOTS: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    SleepEntry,
    Resume,
    FrlFail,
    XhciReinit,
    PowerKey,
}

pub fn classify(msg: &str) -> Option<Kind> {
    if msg.contains("PM: suspend entry") {
        Some(Kind::SleepEntry)
    } else if msg.contains("PM: suspend exit") {
        Some(Kind::Resume)
    } else if msg.contains("FRL link training failed") {
        Some(Kind::FrlFail)
    } else if msg.contains("xHC error in resume") {
        Some(Kind::XhciReinit)
    } else if msg.starts_with("Power key pressed") {
        Some(Kind::PowerKey)
    } else {
        None
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    pub boot: String,
    /// Wall-clock time, microseconds since the epoch.
    pub at_us: i64,
    pub kind: Kind,
}

#[derive(Deserialize)]
struct RawEntry {
    #[serde(rename = "MESSAGE", default)]
    message: serde_json::Value,
    #[serde(rename = "_BOOT_ID", default)]
    boot: String,
    #[serde(rename = "__REALTIME_TIMESTAMP", default)]
    realtime: String,
}

/// Parse `journalctl -o json` output (one object per line) into classified events.
pub fn parse_events(json_lines: &str) -> Vec<Event> {
    json_lines
        .lines()
        .filter_map(|line| serde_json::from_str::<RawEntry>(line).ok())
        .filter_map(|e| {
            let kind = match &e.message {
                serde_json::Value::String(s) => classify(s),
                // Non-UTF-8 messages are exported as byte arrays.
                serde_json::Value::Array(bytes) => {
                    let raw: Vec<u8> = bytes
                        .iter()
                        .filter_map(|b| b.as_u64().map(|b| b as u8))
                        .collect();
                    classify(&String::from_utf8_lossy(&raw))
                }
                _ => None,
            }?;
            Some(Event {
                boot: e.boot,
                at_us: e.realtime.parse().ok()?,
                kind,
            })
        })
        .collect()
}

/// One wake from sleep and what followed it.
#[derive(Debug, Clone, PartialEq)]
pub struct Wake {
    pub at_us: i64,
    /// USB controller had to be re-initialised during this resume.
    pub xhci_reinit: bool,
    /// Seconds after the wake of the first FRL failure (within [`FRL_WINDOW_SECS`]).
    pub frl_after: Option<i64>,
    /// Seconds after the wake the power button was pressed (within [`POWER_WINDOW_SECS`]).
    pub power_key_after: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Evidence {
    pub boots: usize,
    /// Start of the examined history (µs since epoch).
    pub since_us: Option<i64>,
    pub sleeps: usize,
    /// Chronological.
    pub wakes: Vec<Wake>,
    pub frl_total: usize,
    pub xhci_total: usize,
}

impl Evidence {
    pub fn frl_after_wake(&self) -> usize {
        self.wakes.iter().filter(|w| w.frl_after.is_some()).count()
    }

    pub fn power_after_wake(&self) -> usize {
        self.wakes
            .iter()
            .filter(|w| w.power_key_after.is_some())
            .count()
    }
}

/// Fold chronological events into per-wake evidence. Events from different
/// boots never attach to each other.
pub fn summarize(events: &[Event]) -> Evidence {
    let mut ev = Evidence::default();
    let mut boot: Option<&str> = None;
    // Index into `ev.wakes` of the last wake in the current boot.
    let mut last_wake: Option<usize> = None;
    let mut pending_xhci = false;
    for e in events {
        if boot != Some(e.boot.as_str()) {
            boot = Some(&e.boot);
            last_wake = None;
            pending_xhci = false;
        }
        let since_wake = |wakes: &[Wake], idx: Option<usize>| {
            idx.map(|i| (i, (e.at_us - wakes[i].at_us) / 1_000_000))
        };
        match e.kind {
            Kind::SleepEntry => {
                ev.sleeps += 1;
                pending_xhci = false;
            }
            Kind::XhciReinit => {
                ev.xhci_total += 1;
                pending_xhci = true;
            }
            Kind::Resume => {
                ev.wakes.push(Wake {
                    at_us: e.at_us,
                    xhci_reinit: pending_xhci,
                    frl_after: None,
                    power_key_after: None,
                });
                last_wake = Some(ev.wakes.len() - 1);
                pending_xhci = false;
            }
            Kind::FrlFail => {
                ev.frl_total += 1;
                if let Some((i, secs)) = since_wake(&ev.wakes, last_wake)
                    && secs <= FRL_WINDOW_SECS
                    && ev.wakes[i].frl_after.is_none()
                {
                    ev.wakes[i].frl_after = Some(secs);
                }
            }
            Kind::PowerKey => {
                if let Some((i, secs)) = since_wake(&ev.wakes, last_wake)
                    && secs <= POWER_WINDOW_SECS
                    && ev.wakes[i].power_key_after.is_none()
                {
                    ev.wakes[i].power_key_after = Some(secs);
                }
            }
        }
    }
    ev
}

#[derive(Deserialize)]
struct BootRow {
    first_entry: i64,
}

/// Query the journal for the last [`MAX_BOOTS`] boots. Blocking.
pub fn collect() -> Result<Evidence> {
    let boots: Vec<BootRow> = cmd::json("journalctl", ["--list-boots", "-o", "json", "--no-pager"])
        .context("listing boots")?;
    let recent = &boots[boots.len().saturating_sub(MAX_BOOTS)..];
    let Some(first) = recent.first() else {
        return Ok(Evidence::default());
    };
    let since = format!("@{}", first.first_entry / 1_000_000);
    let out = cmd::output(
        "journalctl",
        [
            "-S",
            &since,
            "-o",
            "json",
            "--output-fields=MESSAGE,_BOOT_ID",
            "-g",
            PATTERN,
            "--no-pager",
        ],
    )?;
    // Exit status 1 with empty output means "no matches".
    if !out.ok() && !out.stderr.trim().is_empty() && out.stdout.is_empty() {
        anyhow::bail!("journalctl: {}", out.stderr.trim());
    }
    let mut ev = summarize(&parse_events(&out.stdout));
    ev.boots = recent.len();
    ev.since_us = Some(first.first_entry);
    Ok(ev)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn line(boot: &str, secs: i64, msg: &str) -> String {
        serde_json::json!({ "_BOOT_ID": boot, "__REALTIME_TIMESTAMP": (secs * 1_000_000).to_string(), "MESSAGE": msg })
            .to_string()
    }

    #[test]
    fn classifies_messages() {
        assert_eq!(classify("PM: suspend entry (deep)"), Some(Kind::SleepEntry));
        assert_eq!(classify("PM: suspend exit"), Some(Kind::Resume));
        assert_eq!(
            classify("nvidia-modeset: WARNING: GPU:0: HDMI FRL link training failed."),
            Some(Kind::FrlFail)
        );
        assert_eq!(
            classify("xhci_hcd 0000:00:14.0: xHC error in resume, USBSTS 0x411, Reinit"),
            Some(Kind::XhciReinit)
        );
        assert_eq!(classify("Power key pressed short."), Some(Kind::PowerKey));
        assert_eq!(classify("PM: hibernation: Registered nosave memory"), None);
    }

    #[test]
    fn attributes_events_to_wakes_within_boot() {
        let text = [
            line(
                "a",
                1000,
                "nvidia-modeset: WARNING: GPU:0: HDMI FRL link training failed.",
            ),
            line("a", 2000, "PM: suspend entry (deep)"),
            line(
                "a",
                3000,
                "xhci_hcd 0000:00:14.0: xHC error in resume, USBSTS 0x411, Reinit",
            ),
            line("a", 3003, "PM: suspend exit"),
            line(
                "a",
                3035,
                "nvidia-modeset: WARNING: GPU:0: HDMI FRL link training failed.",
            ),
            line("a", 4000, "PM: suspend entry (deep)"),
            line("a", 5000, "PM: suspend exit"),
            line("a", 5150, "Power key pressed short."),
            line("b", 5200, "Power key pressed short."),
            "not json".to_owned(),
            line("b", 9000, "unrelated"),
        ]
        .join("\n");
        let ev = summarize(&parse_events(&text));
        assert_eq!(ev.sleeps, 2);
        assert_eq!(ev.frl_total, 2);
        assert_eq!(ev.xhci_total, 1);
        assert_eq!(ev.wakes.len(), 2);
        assert_eq!(
            ev.wakes[0],
            Wake {
                at_us: 3003 * 1_000_000,
                xhci_reinit: true,
                frl_after: Some(32),
                power_key_after: None
            }
        );
        assert_eq!(ev.wakes[1].frl_after, None);
        assert!(!ev.wakes[1].xhci_reinit);
        assert_eq!(ev.wakes[1].power_key_after, Some(150));
        assert_eq!(ev.frl_after_wake(), 1);
        assert_eq!(ev.power_after_wake(), 1);
    }

    #[test]
    fn late_events_are_not_attributed() {
        let text = [
            line("a", 0, "PM: suspend exit"),
            line("a", FRL_WINDOW_SECS + 1, "HDMI FRL link training failed."),
            line("a", POWER_WINDOW_SECS + 1, "Power key pressed short."),
        ]
        .join("\n");
        let ev = summarize(&parse_events(&text));
        assert_eq!(ev.frl_total, 1);
        assert_eq!(ev.frl_after_wake(), 0);
        assert_eq!(ev.power_after_wake(), 0);
    }

    #[test]
    fn decodes_byte_array_messages() {
        let bytes: Vec<u8> = b"PM: suspend exit\xff".to_vec();
        let l =
            serde_json::json!({ "_BOOT_ID": "a", "__REALTIME_TIMESTAMP": "5", "MESSAGE": bytes })
                .to_string();
        assert_eq!(
            parse_events(&l),
            vec![Event {
                boot: "a".into(),
                at_us: 5,
                kind: Kind::Resume
            }]
        );
    }
}
