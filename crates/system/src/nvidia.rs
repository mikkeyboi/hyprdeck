//! NVIDIA suspend/resume configuration readout and interpretation.

use std::collections::BTreeMap;

use hyprdeck_core::cmd;

const PROC_DIR: &str = "/proc/driver/nvidia";
const PARAMS: &str = "/proc/driver/nvidia/params";
const VERSION: &str = "/proc/driver/nvidia/version";
pub const SERVICES: [&str; 4] = [
    "nvidia-suspend.service",
    "nvidia-resume.service",
    "nvidia-hibernate.service",
    "nvidia-suspend-then-hibernate.service",
];

#[derive(Debug, Clone, Default)]
pub struct NvidiaInfo {
    /// `NVRM version:` line, trimmed to the driver description.
    pub version: Option<String>,
    pub params: BTreeMap<String, String>,
    /// Unit → `systemctl is-enabled` answer.
    pub services: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Info,
    Warning,
}

impl NvidiaInfo {
    pub fn param(&self, key: &str) -> Option<&str> {
        self.params.get(key).map(String::as_str)
    }

    fn services_enabled(&self) -> Vec<&str> {
        self.services
            .iter()
            .filter(|(_, s)| s == "enabled")
            .map(|(u, _)| u.as_str())
            .collect()
    }

    /// Plain-language reading of the configuration; `mem_sleep` is the active
    /// `/sys/power/mem_sleep` mode.
    pub fn interpret(&self, mem_sleep: Option<&str>) -> Vec<(Level, String)> {
        let mut out = Vec::new();
        if self.params.is_empty() {
            return out;
        }
        let notifiers = self.param("UseKernelSuspendNotifiers") == Some("1");
        let preserve = self
            .param("PreserveVideoMemoryAllocations")
            .is_some_and(|v| v != "0");
        let enabled = self.services_enabled();
        match (notifiers, enabled.is_empty()) {
            (true, true) => out.push((
                Level::Ok,
                "The driver saves and restores video memory itself (kernel suspend notifiers), so the \
                 nvidia-suspend/resume services are correctly disabled."
                    .to_owned(),
            )),
            (true, false) => out.push((
                Level::Warning,
                format!(
                    "Kernel suspend notifiers are on but {} also enabled — video memory is handled twice. \
                     Disable those services.",
                    enabled.join(", ")
                ),
            )),
            (false, true) if preserve => out.push((
                Level::Warning,
                "Video memory preservation is on but nothing saves it: enable nvidia-suspend, nvidia-resume and \
                 nvidia-hibernate (or set NVreg_UseKernelSuspendNotifiers=1)."
                    .to_owned(),
            )),
            (false, false) => out.push((
                Level::Ok,
                format!("Video memory is saved by the systemd services ({}).", enabled.join(", ")),
            )),
            (false, true) => {}
        }
        if !preserve {
            out.push((
                Level::Warning,
                "Video memory is not preserved across sleep; apps can show corrupted or black windows after wake."
                    .to_owned(),
            ));
        }
        if mem_sleep == Some("s2idle") {
            out.push((
                Level::Info,
                "Sleep mode is s2idle: NVIDIA needs NVreg_EnableS0ixPowerManagement=1 for the GPU to save power in it."
                    .to_owned(),
            ));
        }
        out
    }
}

/// Parse `/proc/driver/nvidia/params` (`Key: value` per line).
pub fn parse_params(text: &str) -> BTreeMap<String, String> {
    text.lines()
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().trim_matches('"').to_owned()))
        .collect()
}

/// Read everything; `None` without the NVIDIA kernel driver. Blocking.
pub fn read() -> Option<NvidiaInfo> {
    if !std::path::Path::new(PROC_DIR).exists() {
        return None;
    }
    let params = std::fs::read_to_string(PARAMS)
        .map(|t| parse_params(&t))
        .unwrap_or_default();
    let version = std::fs::read_to_string(VERSION).ok().and_then(|t| {
        let line = t
            .lines()
            .next()?
            .strip_prefix("NVRM version:")?
            .trim()
            .to_owned();
        // "NVIDIA UNIX Open Kernel Module for x86_64  615.71.09  Release Build  (…)"
        Some(
            line.split("  Release Build")
                .next()
                .unwrap_or(&line)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" "),
        )
    });
    let services = SERVICES
        .iter()
        .map(|u| {
            let state = cmd::output("systemctl", ["is-enabled", u])
                .map(|o| o.stdout.trim().to_owned())
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "not installed".to_owned());
            ((*u).to_owned(), state)
        })
        .collect();
    Some(NvidiaInfo {
        version,
        params,
        services,
    })
}
