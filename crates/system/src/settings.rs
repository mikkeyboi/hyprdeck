//! Persisted settings for this crate: `~/.config/hyprdeck/system.toml`.

use anyhow::Result;
use hyprdeck_core::store;
use serde::{Deserialize, Serialize};

const STORE_NAME: &str = "system";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub resume: ResumeGuard,
}

/// What the resume guard does after every wake from sleep.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ResumeGuard {
    /// Record diagnostics after each wake (and run the rescue policy).
    pub enabled: bool,
    /// Seconds to wait after wake before checking the displays.
    pub delay_secs: u32,
    pub rescue: RescuePolicy,
}

impl Default for ResumeGuard {
    fn default() -> Self {
        ResumeGuard {
            enabled: true,
            delay_secs: 3,
            rescue: RescuePolicy::OnProblem,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RescuePolicy {
    Never,
    #[default]
    OnProblem,
    Always,
}

impl RescuePolicy {
    pub const ALL: [RescuePolicy; 3] = [
        RescuePolicy::Never,
        RescuePolicy::OnProblem,
        RescuePolicy::Always,
    ];

    pub fn label(self) -> &'static str {
        match self {
            RescuePolicy::Never => "Never",
            RescuePolicy::OnProblem => "On display problems",
            RescuePolicy::Always => "Always",
        }
    }

    /// CLI spelling (`never`, `problem`, `always`).
    pub fn parse(s: &str) -> Option<RescuePolicy> {
        match s {
            "never" => Some(RescuePolicy::Never),
            "problem" | "on-problem" => Some(RescuePolicy::OnProblem),
            "always" => Some(RescuePolicy::Always),
            _ => None,
        }
    }
}

pub fn load() -> Result<Settings> {
    store::load(STORE_NAME)
}

/// Load, modify and save.
pub fn update(f: impl FnOnce(&mut Settings)) -> Result<Settings> {
    let mut s = load()?;
    f(&mut s);
    store::save(STORE_NAME, &s)?;
    Ok(s)
}
