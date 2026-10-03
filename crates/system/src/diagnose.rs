//! Sleep/wake diagnosis: journal evidence, GPU driver setup, OpenLinkHub and
//! the connected displays, turned into plain-language recommendations.

use std::fmt::Write as _;

use gtk::glib;
use hyprdeck_core::hypr::ctl;

use crate::journal::{self, Evidence};
use crate::nvidia::{self, Level, NvidiaInfo};
use crate::openlinkhub::{self, OlhInfo};
use crate::settings::{self, RescuePolicy, ResumeGuard};
use crate::shortcut;

/// An enabled HDMI output, as reported by hyprctl.
#[derive(Debug, Clone)]
pub struct HdmiOutput {
    /// Connector name (`HDMI-A-1`).
    pub connector: String,
    /// "make model", or the description when both are empty.
    pub display: String,
    /// Current mode (`3840x2160 @ 120 Hz`).
    pub mode: String,
}

#[derive(Debug, Clone)]
pub struct Diagnosis {
    pub evidence: Result<Evidence, String>,
    /// `None` without the NVIDIA kernel driver.
    pub nvidia: Option<NvidiaInfo>,
    /// Active `/sys/power/mem_sleep` mode (`deep`, `s2idle`).
    pub mem_sleep: Option<String>,
    /// `None` when OpenLinkHub is not installed.
    pub olh: Option<Result<OlhInfo, String>>,
    pub hdmi: Vec<HdmiOutput>,
    pub shortcut: Option<String>,
    pub guard: ResumeGuard,
    pub recommendations: Vec<String>,
}

impl Diagnosis {
    /// OpenLinkHub details when its devices come back later than needed after a wake.
    pub fn olh_slow(&self) -> Option<&OlhInfo> {
        match &self.olh {
            Some(Ok(o)) if o.slow() => Some(o),
            _ => None,
        }
    }
}

/// Active mode from `/sys/power/mem_sleep` (`s2idle [deep]` → `deep`).
pub fn active_mem_sleep(text: &str) -> Option<String> {
    let start = text.find('[')?;
    let end = text[start..].find(']')? + start;
    Some(text[start + 1..end].to_owned())
}

/// Gather everything. Blocking (journal, hyprctl, systemctl).
pub fn run() -> Diagnosis {
    let evidence = journal::collect().map_err(|e| format!("{e:#}"));
    let nvidia = nvidia::read();
    let mem_sleep = std::fs::read_to_string("/sys/power/mem_sleep")
        .ok()
        .and_then(|t| active_mem_sleep(&t));
    let olh = openlinkhub::detect().map(|i| openlinkhub::read(&i).map_err(|e| format!("{e:#}")));
    let hdmi = ctl::monitors()
        .map(|ms| {
            ms.into_iter()
                .filter(|m| !m.disabled && m.name.starts_with("HDMI"))
                .map(|m| {
                    let name = format!("{} {}", m.make, m.model).trim().to_owned();
                    HdmiOutput {
                        display: if name.is_empty() { m.description } else { name },
                        mode: format!("{}x{} @ {:.0} Hz", m.width, m.height, m.refresh_rate),
                        connector: m.name,
                    }
                })
                .collect()
        })
        .unwrap_or_default();
    let shortcut = shortcut::current().ok().flatten();
    let guard = settings::load().map(|s| s.resume).unwrap_or_default();
    let mut d = Diagnosis {
        evidence,
        nvidia,
        mem_sleep,
        olh,
        hdmi,
        shortcut,
        guard,
        recommendations: Vec::new(),
    };
    d.recommendations = recommendations(&d);
    d
}

/// Advice after `failures` HDMI FRL link-training failures, naming the HDMI outputs in use.
pub fn hdmi_advice(failures: usize, outputs: &[HdmiOutput]) -> String {
    let logged = format!("{failures} HDMI FRL link-training failure(s) were logged.");
    if outputs.is_empty() {
        return format!(
            "{logged} FRL link training is part of HDMI 2.1; DisplayPort connections don't use it."
        );
    }
    let list = outputs
        .iter()
        .map(|o| {
            let display = if o.display.is_empty() {
                "a display"
            } else {
                o.display.as_str()
            };
            format!("{display} ({}, {})", o.connector, o.mode)
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{logged} Connected over HDMI: {list}. If the display and graphics card both have DisplayPort, a DisplayPort \
         connection avoids HDMI 2.1 FRL link training entirely."
    )
}

fn recommendations(d: &Diagnosis) -> Vec<String> {
    let mut out = Vec::new();
    let guard_on = d.guard.enabled && d.guard.rescue != RescuePolicy::Never;
    if let Ok(ev) = &d.evidence {
        if ev.frl_total > 0 {
            out.push(hdmi_advice(ev.frl_total, &d.hdmi));
            out.push(if guard_on {
                "Keep the resume guard on: it re-trains the display link when it sees the failure after a wake.".to_owned()
            } else {
                "Turn the resume guard on with \"Reset displays: On display problems\" so a failed link is \
                 re-trained automatically after wake."
                    .to_owned()
            });
        }
        let forced = ev.power_after_wake();
        if forced > 0 {
            out.push(match &d.shortcut {
                Some(keys) => format!(
                    "{forced} wake(s) ended with the power button within {} minutes — most likely a black screen. Next time \
                     press {keys} instead: it resets the displays even when the screen is black or locked.",
                    journal::POWER_WINDOW_SECS / 60
                ),
                None => format!(
                    "{forced} wake(s) ended with the power button within {} minutes — most likely a black screen. Add the \
                     rescue keyboard shortcut to recover without cutting power.",
                    journal::POWER_WINDOW_SECS / 60
                ),
            });
        }
        let reinit = ev.wakes.iter().filter(|w| w.xhci_reinit).count();
        if reinit > 0 {
            out.push(format!(
                "The USB controller re-initialises on {reinit} of {} wake(s) (\"xHC error in resume\"), a firmware quirk that \
                 makes every USB device reconnect — keyboards and mice need a moment after wake. It does not cause a \
                 black screen.",
                ev.wakes.len()
            ));
        }
    }
    if let Some(n) = &d.nvidia {
        for (level, text) in n.interpret(d.mem_sleep.as_deref()) {
            if level == Level::Warning {
                out.push(text);
            }
        }
    }
    out
}

/// Plain-language meaning of a `/sys/power/mem_sleep` mode.
pub fn mem_sleep_explain(mode: &str) -> &'static str {
    match mode {
        "deep" => {
            "Suspend to RAM (S3): the GPU and USB lose power, so display links are re-trained and USB devices reconnect on every wake"
        }
        "s2idle" => "Suspend to idle: devices stay partly powered and wake faster",
        "shallow" => "Standby (S1): most devices keep power",
        _ => "",
    }
}

fn date(us: i64) -> String {
    glib::DateTime::from_unix_local(us / 1_000_000)
        .and_then(|d| d.format("%Y-%m-%d %H:%M"))
        .map(|s| s.to_string())
        .unwrap_or_default()
}

impl Diagnosis {
    /// Plain-text report for the CLI.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(s, "Sleep & wake history");
        match &self.evidence {
            Ok(ev) => {
                let _ = writeln!(
                    s,
                    "  {} boot(s) since {}: {} wake(s)",
                    ev.boots,
                    ev.since_us.map(date).unwrap_or_default(),
                    ev.wakes.len()
                );
                let _ = writeln!(
                    s,
                    "  HDMI FRL link-training failures: {} ({} within {} s of a wake)",
                    ev.frl_total,
                    ev.frl_after_wake(),
                    journal::FRL_WINDOW_SECS
                );
                let _ = writeln!(s, "  USB controller re-inits on wake: {}", ev.xhci_total);
                let _ = writeln!(
                    s,
                    "  Power button within {} min of a wake: {}",
                    journal::POWER_WINDOW_SECS / 60,
                    ev.power_after_wake()
                );
                for w in ev.wakes.iter().rev().take(10) {
                    let _ = writeln!(s, "    {}  {}", date(w.at_us), wake_flags(w));
                }
            }
            Err(e) => {
                let _ = writeln!(s, "  journal unavailable: {e}");
            }
        }
        if let Some(m) = &self.mem_sleep {
            let _ = writeln!(s, "  mem_sleep: {m}");
        }
        if let Some(n) = &self.nvidia {
            let _ = writeln!(s, "\nNVIDIA");
            if let Some(v) = &n.version {
                let _ = writeln!(s, "  {v}");
            }
            for key in [
                "UseKernelSuspendNotifiers",
                "PreserveVideoMemoryAllocations",
                "TemporaryFilePath",
            ] {
                if let Some(v) = n.param(key) {
                    let _ = writeln!(s, "  {key}: {v}");
                }
            }
            for (unit, state) in &n.services {
                let _ = writeln!(s, "  {unit}: {state}");
            }
            for (level, text) in n.interpret(self.mem_sleep.as_deref()) {
                let _ = writeln!(s, "  [{level:?}] {text}");
            }
        }
        if let Some(olh) = &self.olh {
            let _ = writeln!(s, "\nOpenLinkHub");
            match olh {
                Ok(o) => {
                    let _ = writeln!(
                        s,
                        "  {} ({}): resumeDelay {} ms, RestartSec {} s, {}",
                        o.config.display(),
                        o.scope.label(),
                        o.resume_delay_ms,
                        o.restart_sec,
                        o.active
                    );
                    let _ = writeln!(
                        s,
                        "  devices back ≈ {:.1} s after wake (plus device start-up){}",
                        o.wake_delay(),
                        if o.slow() {
                            " — can be shortened on the Tweaks page"
                        } else {
                            ""
                        }
                    );
                }
                Err(e) => {
                    let _ = writeln!(s, "  {e}");
                }
            }
        }
        let _ = writeln!(
            s,
            "\nResume guard: {} · rescue: {} · rescue shortcut: {}",
            if self.guard.enabled { "on" } else { "off" },
            self.guard.rescue.label(),
            self.shortcut.as_deref().unwrap_or("not set")
        );
        if !self.recommendations.is_empty() {
            let _ = writeln!(s, "\nRecommendations");
            for r in &self.recommendations {
                let _ = writeln!(s, "  • {r}");
            }
        }
        s
    }
}

pub fn wake_flags(w: &journal::Wake) -> String {
    let mut f = Vec::new();
    if let Some(s) = w.frl_after {
        f.push(format!("FRL link failure +{s}s"));
    }
    if let Some(s) = w.power_key_after {
        f.push(format!("power button +{s}s"));
    }
    if w.xhci_reinit {
        f.push("USB re-init".to_owned());
    }
    if f.is_empty() {
        "ok".to_owned()
    } else {
        f.join(", ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_sleep_mode() {
        assert_eq!(active_mem_sleep("s2idle [deep]\n").as_deref(), Some("deep"));
        assert_eq!(active_mem_sleep("[s2idle]\n").as_deref(), Some("s2idle"));
        assert_eq!(active_mem_sleep(""), None);
    }

    #[test]
    fn hdmi_advice_uses_live_outputs() {
        let out = HdmiOutput {
            connector: "HDMI-A-1".into(),
            display: "Example Monitor".into(),
            mode: "2560x1440 @ 144 Hz".into(),
        };
        let advice = hdmi_advice(2, std::slice::from_ref(&out));
        assert!(advice.starts_with(
            "2 HDMI FRL link-training failure(s) were logged. Connected over HDMI: Example Monitor (HDMI-A-1, \
             2560x1440 @ 144 Hz). "
        ));
        assert!(hdmi_advice(1, &[]).starts_with("1 HDMI FRL"));
    }
}
