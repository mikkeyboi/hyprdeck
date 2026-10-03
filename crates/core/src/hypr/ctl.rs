//! Thin, blocking wrappers around `hyprctl` (Hyprland 0.56, Lua config).
//! Call from [`crate::rt::blocking`].

use anyhow::{Result, bail};
use serde::Deserialize;

use crate::cmd;

/// Query `hyprctl -j <args…>` and deserialize the JSON reply.
pub fn query<T: serde::de::DeserializeOwned>(args: &[&str]) -> Result<T> {
    let mut all = vec!["-j"];
    all.extend_from_slice(args);
    cmd::json("hyprctl", all)
}

/// Execute a Lua chunk inside the running compositor (`hyprctl eval`).
pub fn eval(lua: &str) -> Result<()> {
    let out = cmd::output("hyprctl", ["eval", lua])?;
    let text = out.stdout.trim();
    if !out.ok() || text.starts_with("error") {
        bail!(
            "hyprctl eval failed: {}",
            if text.is_empty() {
                out.stderr.trim()
            } else {
                text
            }
        );
    }
    Ok(())
}

/// Evaluate a Lua chunk and return its printed result (`hyprctl repl <code>`).
pub fn repl(lua: &str) -> Result<String> {
    let out = cmd::output("hyprctl", ["repl", lua])?;
    let text = out.stdout.trim();
    if !out.ok() || text.starts_with("error") {
        bail!(
            "hyprctl repl failed: {}",
            if text.is_empty() {
                out.stderr.trim()
            } else {
                text
            }
        );
    }
    Ok(text.to_owned())
}

/// Dispatch a Lua dispatcher expression, e.g. `hl.dsp.dpms({ action = "on" })`.
pub fn dispatch(dsp_expr: &str) -> Result<()> {
    let out = cmd::output("hyprctl", ["dispatch", dsp_expr])?;
    let text = out.stdout.trim();
    if !out.ok() || text != "ok" {
        bail!(
            "hyprctl dispatch failed: {}",
            if text.is_empty() {
                out.stderr.trim()
            } else {
                text
            }
        );
    }
    Ok(())
}

/// Reload the whole config. `config_only` skips the monitor reload (no modeset flicker).
pub fn reload(config_only: bool) -> Result<()> {
    if config_only {
        cmd::run("hyprctl", ["reload", "config-only"])?;
    } else {
        cmd::run("hyprctl", ["reload"])?;
    }
    Ok(())
}

/// Non-empty config parsing errors reported by the compositor.
pub fn config_errors() -> Result<Vec<String>> {
    let errs: Vec<String> = query(&["configerrors"])?;
    Ok(errs.into_iter().filter(|e| !e.trim().is_empty()).collect())
}

/// Re-light every enabled output: DPMS off → on forces a full modeset (and an
/// HDMI FRL link retrain), then a full reload re-applies the monitor rules.
/// Recovers the black screen seen after resume when link training fails.
pub fn rescue_displays() -> Result<()> {
    let outputs: Vec<String> = monitors()?
        .into_iter()
        .filter(|m| !m.disabled)
        .map(|m| m.name)
        .collect();
    for name in &outputs {
        dispatch(&format!(
            "hl.dsp.dpms({{ action = \"off\", monitor = {} }})",
            lua_str(name)
        ))?;
    }
    std::thread::sleep(std::time::Duration::from_millis(1500));
    for name in &outputs {
        dispatch(&format!(
            "hl.dsp.dpms({{ action = \"on\", monitor = {} }})",
            lua_str(name)
        ))?;
    }
    std::thread::sleep(std::time::Duration::from_millis(500));
    reload(false)
}

/// Tail of the compositor log (`hyprctl rollinglog`).
pub fn rolling_log() -> Result<String> {
    cmd::run("hyprctl", ["rollinglog"])
}

/// Quote a Rust string as a Lua string literal.
pub fn lua_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\{}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Monitor {
    pub id: i64,
    pub name: String,
    pub description: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    pub width: i64,
    pub height: i64,
    pub physical_width: i64,
    pub physical_height: i64,
    pub refresh_rate: f64,
    pub x: i64,
    pub y: i64,
    pub scale: f64,
    pub transform: i64,
    pub focused: bool,
    pub dpms_status: bool,
    pub vrr: bool,
    pub disabled: bool,
    pub current_format: String,
    pub mirror_of: String,
    pub available_modes: Vec<String>,
    #[serde(default)]
    pub color_management_preset: String,
    #[serde(default)]
    pub sdr_brightness: Option<f64>,
    #[serde(default)]
    pub sdr_saturation: Option<f64>,
    #[serde(default)]
    pub sdr_min_luminance: Option<f64>,
    #[serde(default)]
    pub sdr_max_luminance: Option<f64>,
}

/// All outputs, including disabled ones.
pub fn monitors() -> Result<Vec<Monitor>> {
    query(&["monitors", "all"])
}

#[derive(Debug, Clone, Deserialize)]
pub struct Keyboard {
    pub name: String,
    pub layout: String,
    pub variant: String,
    pub options: String,
    pub active_keymap: String,
    #[serde(rename = "capsLock")]
    pub caps_lock: bool,
    #[serde(rename = "numLock")]
    pub num_lock: bool,
    pub main: bool,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Pointer {
    pub name: String,
    #[serde(rename = "defaultSpeed")]
    pub default_speed: f64,
    #[serde(rename = "scrollFactor", default)]
    pub scroll_factor: f64,
}

#[derive(Debug, Clone, Deserialize)]
pub struct NamedDevice {
    pub name: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Devices {
    pub keyboards: Vec<Keyboard>,
    pub mice: Vec<Pointer>,
    #[serde(default)]
    pub tablets: Vec<NamedDevice>,
    #[serde(default)]
    pub touch: Vec<NamedDevice>,
    #[serde(default)]
    pub switches: Vec<NamedDevice>,
}

pub fn devices() -> Result<Devices> {
    query(&["devices"])
}

/// A live keybind as reported by `hyprctl -j binds`. Lua-defined actions show up
/// as dispatcher `__lua`; use [`crate::hypr::model`] for their source text.
#[derive(Debug, Clone, Deserialize)]
pub struct LiveBind {
    pub modmask: u32,
    pub key: String,
    pub keycode: i64,
    pub submap: String,
    pub mouse: bool,
    pub locked: bool,
    pub release: bool,
    pub repeat: bool,
    pub description: String,
    pub dispatcher: String,
    pub arg: String,
}

pub fn binds() -> Result<Vec<LiveBind>> {
    query(&["binds"])
}
