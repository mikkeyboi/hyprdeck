//! Per-game launch settings that any modern Proton understands, translated per runner.
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// Runner families whose launch scripts name the same feature differently.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Family {
    /// GE-Proton and Hyprdeck builds (which start from GE).
    Ge,
    CachyOs,
    Valve,
}

impl Family {
    /// Guesses the family from a tool's folder or display name.
    pub fn detect(name: &str) -> Option<Self> {
        let lower = name.to_ascii_lowercase();
        if lower.contains("cachyos") {
            Some(Self::CachyOs)
        } else if lower.starts_with("ge-proton")
            || lower.starts_with("proton-ge")
            || lower.starts_with("hyprdeck-ge")
        {
            Some(Self::Ge)
        } else if lower.starts_with("proton ") || lower == "proton experimental" {
            Some(Self::Valve)
        } else {
            None
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct GameProfile {
    // Display
    pub hdr: bool,
    pub wayland: bool,
    // Upscaling
    pub dlss_upgrade: bool,
    pub dlss_indicator: bool,
    pub fsr4_upgrade: bool,
    pub xess_upgrade: bool,
    // Latency
    pub vulkan_reflex: bool,
    pub low_latency_layer: bool,
    pub low_latency_dxvk: bool,
    pub low_latency_vkd3d: bool,
    /// DXVK/vkd3d-proton frame limit; 0 is off.
    pub frame_rate_limit: u32,
    // Performance
    pub local_shader_cache: bool,
    // Troubleshooting
    pub disable_ntsync: bool,
    pub proton_log: bool,
}

/// One user-facing switch, for building the UI and checking runner support.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Setting {
    pub key: &'static str,
    pub group: &'static str,
    pub title: &'static str,
    pub subtitle: &'static str,
}

pub const SETTINGS: &[Setting] = &[
    Setting {
        key: "hdr",
        group: "Display",
        title: "HDR",
        subtitle: "Needs HDR enabled for the Gamescope session",
    },
    Setting {
        key: "wayland",
        group: "Display",
        title: "Native Wayland",
        subtitle: "Experimental Wine Wayland driver instead of XWayland",
    },
    Setting {
        key: "dlss_upgrade",
        group: "Upscaling",
        title: "Upgrade DLSS",
        subtitle: "Download and use the newest DLSS DLLs",
    },
    Setting {
        key: "dlss_indicator",
        group: "Upscaling",
        title: "DLSS indicator",
        subtitle: "Show the DLSS overlay to confirm it is active",
    },
    Setting {
        key: "fsr4_upgrade",
        group: "Upscaling",
        title: "Upgrade FSR 3.1 to FSR 4",
        subtitle: "Download the FSR 4 DLL for games with FSR 3.1",
    },
    Setting {
        key: "xess_upgrade",
        group: "Upscaling",
        title: "Upgrade XeSS",
        subtitle: "Download and use the newest XeSS DLLs",
    },
    Setting {
        key: "vulkan_reflex",
        group: "Latency",
        title: "Reflex for Vulkan games",
        subtitle: "dxvk-nvapi's Vulkan Reflex layer (NVIDIA)",
    },
    Setting {
        key: "low_latency_layer",
        group: "Latency",
        title: "Low-latency layer",
        subtitle: "Vendor-neutral frame pacing layer",
    },
    Setting {
        key: "low_latency_dxvk",
        group: "Latency",
        title: "Low-latency DXVK",
        subtitle: "dxvk-low-latency fork for DirectX 8 to 11",
    },
    Setting {
        key: "low_latency_vkd3d",
        group: "Latency",
        title: "Low-latency vkd3d",
        subtitle: "vkd3d-low-latency fork for DirectX 12",
    },
    Setting {
        key: "frame_rate_limit",
        group: "Latency",
        title: "Frame rate limit",
        subtitle: "DXVK and vkd3d-proton limiter; 0 is off",
    },
    Setting {
        key: "local_shader_cache",
        group: "Performance",
        title: "Per-game shader cache",
        subtitle: "Keep a shader cache even with Steam pre-caching off",
    },
    Setting {
        key: "disable_ntsync",
        group: "Troubleshooting",
        title: "Turn off ntsync",
        subtitle: "Fall back to fsync if a game misbehaves",
    },
    Setting {
        key: "proton_log",
        group: "Troubleshooting",
        title: "Proton log",
        subtitle: "Write ~/steam-<appid>.log",
    },
];

/// The environment a profile produces on a runner, plus settings that runner cannot honor.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Resolved {
    pub env: BTreeMap<String, String>,
    pub unsupported: Vec<&'static str>,
}

impl GameProfile {
    pub fn resolve(&self, family: Family) -> Resolved {
        use Family::*;
        let mut out = Resolved::default();
        let mut set = |on: bool, key: &'static str, name: Option<&str>, value: &str| {
            if !on {
                return;
            }
            match name {
                Some(name) => {
                    out.env.insert(name.into(), value.into());
                }
                None => out.unsupported.push(key),
            }
        };
        let modern = |name: &'static str| (family != Valve).then_some(name);

        set(
            self.hdr,
            "hdr",
            Some(match family {
                Ge => "PROTON_ENABLE_HDR",
                CachyOs | Valve => "DXVK_HDR",
            }),
            "1",
        );
        set(
            self.wayland,
            "wayland",
            modern("PROTON_ENABLE_WAYLAND"),
            "1",
        );
        set(
            self.dlss_upgrade,
            "dlss_upgrade",
            modern("PROTON_DLSS_UPGRADE"),
            "1",
        );
        set(
            self.dlss_indicator,
            "dlss_indicator",
            modern("PROTON_DLSS_INDICATOR"),
            "1",
        );
        set(
            self.fsr4_upgrade,
            "fsr4_upgrade",
            modern("PROTON_FSR4_UPGRADE"),
            "1",
        );
        set(
            self.xess_upgrade,
            "xess_upgrade",
            modern("PROTON_XESS_UPGRADE"),
            "1",
        );
        set(
            self.vulkan_reflex,
            "vulkan_reflex",
            modern("DXVK_NVAPI_VKREFLEX"),
            "1",
        );
        set(
            self.low_latency_layer,
            "low_latency_layer",
            modern("LOW_LATENCY_LAYER"),
            "1",
        );
        // GE has no switch for these forks; Hyprdeck variants carry them as DLL overlays instead.
        set(
            self.low_latency_dxvk,
            "low_latency_dxvk",
            (family == CachyOs).then_some("PROTON_DXVK_LOWLATENCY"),
            "1",
        );
        set(
            self.low_latency_vkd3d,
            "low_latency_vkd3d",
            (family == CachyOs).then_some("PROTON_VKD3D_LOWLATENCY"),
            "1",
        );
        if self.frame_rate_limit > 0 {
            let fps = self.frame_rate_limit.to_string();
            set(true, "frame_rate_limit", Some("DXVK_FRAME_RATE"), &fps);
            set(true, "frame_rate_limit", Some("VKD3D_FRAME_RATE"), &fps);
        }
        set(
            self.local_shader_cache,
            "local_shader_cache",
            modern("PROTON_LOCAL_SHADER_CACHE"),
            "1",
        );
        set(
            self.disable_ntsync,
            "disable_ntsync",
            (family == Ge).then_some("PROTON_NO_NTSYNC"),
            "1",
        );
        set(self.proton_log, "proton_log", Some("PROTON_LOG"), "1");
        out
    }

    fn flag_mut(&mut self, key: &str) -> Option<&mut bool> {
        Some(match key {
            "hdr" => &mut self.hdr,
            "wayland" => &mut self.wayland,
            "dlss_upgrade" => &mut self.dlss_upgrade,
            "dlss_indicator" => &mut self.dlss_indicator,
            "fsr4_upgrade" => &mut self.fsr4_upgrade,
            "xess_upgrade" => &mut self.xess_upgrade,
            "vulkan_reflex" => &mut self.vulkan_reflex,
            "low_latency_layer" => &mut self.low_latency_layer,
            "low_latency_dxvk" => &mut self.low_latency_dxvk,
            "low_latency_vkd3d" => &mut self.low_latency_vkd3d,
            "local_shader_cache" => &mut self.local_shader_cache,
            "disable_ntsync" => &mut self.disable_ntsync,
            "proton_log" => &mut self.proton_log,
            _ => return None,
        })
    }

    /// The on/off value of a [`SETTINGS`] key; `None` for numeric settings.
    pub fn flag(&self, key: &str) -> Option<bool> {
        self.clone().flag_mut(key).map(|v| *v)
    }

    /// Sets an on/off [`SETTINGS`] key; returns false for unknown or numeric keys.
    pub fn set_flag(&mut self, key: &str, on: bool) -> bool {
        match self.flag_mut(key) {
            Some(value) => {
                *value = on;
                true
            }
            None => false,
        }
    }

    /// Whether `family` can honor `key`, judged by turning on only that setting.
    pub fn supported(key: &str, family: Family) -> bool {
        let mut probe = Self::default();
        if key == "frame_rate_limit" {
            probe.frame_rate_limit = 60;
        } else if !probe.set_flag(key, true) {
            return false;
        }
        probe.resolve(family).unsupported.is_empty()
    }

    pub fn is_empty(&self) -> bool {
        self == &Self::default()
    }
}

/// A game's departures from the global defaults; settings it leaves out follow them.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Overrides {
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub flags: BTreeMap<String, bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frame_rate_limit: Option<u32>,
}

impl Overrides {
    /// The settings a game runs with: the global defaults with this game's overrides on top.
    pub fn apply(&self, global: &GameProfile) -> GameProfile {
        let mut profile = global.clone();
        for (key, on) in &self.flags {
            profile.set_flag(key, *on);
        }
        if let Some(fps) = self.frame_rate_limit {
            profile.frame_rate_limit = fps;
        }
        profile
    }

    /// Overrides an on/off key, or follows the global default again with `None`.
    pub fn set_flag(&mut self, key: &str, value: Option<bool>) {
        if GameProfile::default().flag(key).is_none() {
            return;
        }
        match value {
            Some(on) => self.flags.insert(key.to_owned(), on),
            None => self.flags.remove(key),
        };
    }

    pub fn len(&self) -> usize {
        self.flags.len() + usize::from(self.frame_rate_limit.is_some())
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl GameProfile {
    /// Titles of the settings that are on, for summaries such as "HDR, Upgrade DLSS".
    pub fn enabled_titles(&self) -> Vec<String> {
        SETTINGS
            .iter()
            .filter_map(|s| match self.flag(s.key) {
                Some(true) => Some(s.title.to_owned()),
                None if s.key == "frame_rate_limit" && self.frame_rate_limit > 0 => {
                    Some(format!("{} FPS limit", self.frame_rate_limit))
                }
                _ => None,
            })
            .collect()
    }
}

/// Global defaults and every game's overrides, as saved under [`profiles_dir`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Profiles {
    pub global: GameProfile,
    pub games: BTreeMap<u32, Overrides>,
}

impl Profiles {
    pub fn effective(&self, app: u32) -> GameProfile {
        match self.games.get(&app) {
            Some(overrides) => overrides.apply(&self.global),
            None => self.global.clone(),
        }
    }

    /// Reads `defaults.json` and every `<appid>.json` in `dir`; missing files are defaults.
    pub fn load(dir: &Path) -> Result<Self> {
        let mut profiles = Self {
            global: read_json(&dir.join(GLOBAL))?.unwrap_or_default(),
            games: BTreeMap::new(),
        };
        let entries = match fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(profiles),
            Err(err) => return Err(err.into()),
        };
        for entry in entries {
            let path = entry?.path();
            let Some(app) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".json"))
                .and_then(|n| n.parse::<u32>().ok())
            else {
                continue;
            };
            if let Some(overrides) = read_json::<Overrides>(&path)?
                && !overrides.is_empty()
            {
                profiles.games.insert(app, overrides);
            }
        }
        Ok(profiles)
    }

    pub fn save_global(dir: &Path, global: &GameProfile) -> Result<()> {
        write_json(dir, GLOBAL, global, global.is_empty())
    }

    /// Saves one game's overrides; a game with none left has its file removed.
    pub fn save_game(dir: &Path, app: u32, overrides: &Overrides) -> Result<()> {
        write_json(dir, &format!("{app}.json"), overrides, overrides.is_empty())
    }
}

const GLOBAL: &str = "defaults.json";

/// `~/.config/hyprdeck/games`, honoring `XDG_CONFIG_HOME`.
pub fn profiles_dir() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME is not set")?;
    Ok(config.join("hyprdeck/games"))
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text)
            .map(Some)
            .with_context(|| format!("Reading {}", path.display())),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err).with_context(|| format!("Reading {}", path.display())),
    }
}

/// Writes atomically, or removes the file when `empty`.
fn write_json(dir: &Path, name: &str, value: &impl Serialize, empty: bool) -> Result<()> {
    let path = dir.join(name);
    if empty {
        return match fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        };
    }
    fs::create_dir_all(dir)?;
    let staging = dir.join(format!(".{name}.tmp"));
    fs::write(&staging, serde_json::to_string_pretty(value)? + "\n")?;
    fs::rename(&staging, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn families_are_detected_from_tool_names() {
        assert_eq!(Family::detect("GE-Proton11-1"), Some(Family::Ge));
        assert_eq!(
            Family::detect("hyprdeck-ge-proton11-1-v3"),
            Some(Family::Ge)
        );
        assert_eq!(
            Family::detect("proton-cachyos-11.0-20260702-slr-x86_64_v3"),
            Some(Family::CachyOs)
        );
        assert_eq!(Family::detect("Proton Experimental"), Some(Family::Valve));
        assert_eq!(Family::detect("XLM"), None);
    }

    #[test]
    fn same_profile_maps_to_each_runners_names() {
        let profile = GameProfile {
            hdr: true,
            dlss_upgrade: true,
            low_latency_dxvk: true,
            disable_ntsync: true,
            frame_rate_limit: 117,
            ..Default::default()
        };
        let ge = profile.resolve(Family::Ge);
        assert_eq!(ge.env["PROTON_ENABLE_HDR"], "1");
        assert_eq!(ge.env["PROTON_NO_NTSYNC"], "1");
        assert_eq!(ge.env["DXVK_FRAME_RATE"], "117");
        assert_eq!(ge.unsupported, ["low_latency_dxvk"]);

        let cachy = profile.resolve(Family::CachyOs);
        assert_eq!(cachy.env["DXVK_HDR"], "1");
        assert_eq!(cachy.env["PROTON_DXVK_LOWLATENCY"], "1");
        assert_eq!(cachy.unsupported, ["disable_ntsync"]);

        let valve = profile.resolve(Family::Valve);
        assert!(valve.unsupported.contains(&"dlss_upgrade"));
        assert_eq!(valve.env["VKD3D_FRAME_RATE"], "117");
    }

    #[test]
    fn every_profile_field_has_a_ui_setting() {
        let json = serde_json::to_value(GameProfile::default()).unwrap();
        let keys: Vec<_> = json.as_object().unwrap().keys().cloned().collect();
        let mut settings: Vec<_> = SETTINGS.iter().map(|s| s.key.to_owned()).collect();
        settings.sort();
        let mut keys = keys;
        keys.sort();
        assert_eq!(keys, settings);
    }

    #[test]
    fn flags_are_addressable_by_setting_key() {
        let mut profile = GameProfile::default();
        for setting in SETTINGS {
            if setting.key == "frame_rate_limit" {
                assert_eq!(profile.flag(setting.key), None);
                continue;
            }
            assert!(profile.set_flag(setting.key, true), "{}", setting.key);
            assert_eq!(profile.flag(setting.key), Some(true));
        }
        assert!(!profile.set_flag("nope", true));
        assert!(GameProfile::supported("frame_rate_limit", Family::Valve));
        assert!(!GameProfile::supported("low_latency_dxvk", Family::Ge));
        assert!(GameProfile::supported("low_latency_dxvk", Family::CachyOs));
    }

    #[test]
    fn games_follow_global_defaults_unless_overridden() {
        let dir = tempfile::tempdir().unwrap();
        let global = GameProfile {
            hdr: true,
            dlss_upgrade: true,
            frame_rate_limit: 117,
            ..Default::default()
        };
        let mut overrides = Overrides::default();
        overrides.set_flag("hdr", Some(false));
        overrides.set_flag("proton_log", Some(true));
        overrides.set_flag("frame_rate_limit", Some(true));
        assert_eq!(overrides.len(), 2);
        Profiles::save_global(dir.path(), &global).unwrap();
        Profiles::save_game(dir.path(), 1091500, &overrides).unwrap();

        let profiles = Profiles::load(dir.path()).unwrap();
        assert_eq!(profiles.global, global);
        let game = profiles.effective(1091500);
        assert!(!game.hdr && game.dlss_upgrade && game.proton_log);
        assert_eq!(game.frame_rate_limit, 117);
        assert_eq!(profiles.effective(42), global);
        assert_eq!(
            global.enabled_titles(),
            ["HDR", "Upgrade DLSS", "117 FPS limit"]
        );

        overrides.set_flag("hdr", None);
        overrides.set_flag("proton_log", None);
        Profiles::save_game(dir.path(), 1091500, &overrides).unwrap();
        assert!(!dir.path().join("1091500.json").exists());
        Profiles::save_global(dir.path(), &GameProfile::default()).unwrap();
        assert_eq!(Profiles::load(dir.path()).unwrap(), Profiles::default());
    }
}
