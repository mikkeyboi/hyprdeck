//! Persisted launcher choices; automatic display values are resolved at launch.

use anyhow::Result;
use hyprdeck_core::store;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Toggle {
    Auto,
    On,
    Off,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    Fullscreen,
    Borderless,
    Windowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Filter {
    Linear,
    Nearest,
    Fsr,
    Nis,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub output: Option<String>,
    pub auto_display: bool,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: f64,
    pub render_width: u32,
    pub render_height: u32,
    pub hdr: Toggle,
    pub adaptive_sync: Toggle,
    pub window_mode: WindowMode,
    pub filter: Filter,
    pub framerate_limit: u32,
    pub grab_keyboard: bool,
    pub force_grab_cursor: bool,
    pub hide_cursor_delay: u32,
    pub mouse_sensitivity: f64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            output: None,
            auto_display: true,
            width: 1920,
            height: 1080,
            refresh_rate: 60.0,
            render_width: 0,
            render_height: 0,
            hdr: Toggle::Auto,
            adaptive_sync: Toggle::Auto,
            window_mode: WindowMode::Fullscreen,
            filter: Filter::Linear,
            framerate_limit: 0,
            grab_keyboard: false,
            force_grab_cursor: false,
            hide_cursor_delay: 1000,
            mouse_sensitivity: 1.0,
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self> {
        store::load("gamescope")
    }

    pub fn save(&self) -> Result<()> {
        store::save("gamescope", self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn older_profiles_keep_automatic_mouse_capture_and_switching_defaults() {
        let settings: Settings = serde_json::from_value(serde_json::json!({
            "grab_keyboard": true,
            "framerate_limit": 60
        }))
        .unwrap();
        assert!(settings.grab_keyboard);
        assert_eq!(settings.framerate_limit, 60);
        assert!(!settings.force_grab_cursor);
        assert_eq!(settings.hide_cursor_delay, 1000);
        assert_eq!(settings.mouse_sensitivity, 1.0);
    }
}
