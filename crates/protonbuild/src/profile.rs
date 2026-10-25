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

/// `~/.config/hyprdeck/games`, honoring `XDG_CONFIG_HOME`.
pub fn profiles_dir() -> Result<PathBuf> {
    let config = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .context("HOME is not set")?;
    Ok(config.join("hyprdeck/games"))
}

pub fn load(dir: &Path, app: u32) -> Result<GameProfile> {
    let path = dir.join(format!("{app}.json"));
    match fs::read_to_string(&path) {
        Ok(text) => {
            serde_json::from_str(&text).with_context(|| format!("Reading {}", path.display()))
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(GameProfile::default()),
        Err(err) => Err(err).with_context(|| format!("Reading {}", path.display())),
    }
}

/// Saves atomically; an all-default profile removes the file.
pub fn save(dir: &Path, app: u32, profile: &GameProfile) -> Result<()> {
    let path = dir.join(format!("{app}.json"));
    if profile.is_empty() {
        return match fs::remove_file(&path) {
            Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err.into()),
            _ => Ok(()),
        };
    }
    fs::create_dir_all(dir)?;
    let staging = dir.join(format!(".{app}.json.tmp"));
    fs::write(&staging, serde_json::to_string_pretty(profile)? + "\n")?;
    fs::rename(&staging, &path)?;
    Ok(())
}

/// Every saved profile, keyed by Steam app id.
pub fn load_all(dir: &Path) -> Result<BTreeMap<u32, GameProfile>> {
    let mut all = BTreeMap::new();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(all),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let name = entry?.file_name();
        let Some(app) = name
            .to_str()
            .and_then(|n| n.strip_suffix(".json"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        all.insert(app, load(dir, app)?);
    }
    Ok(all)
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
    fn profiles_round_trip_and_defaults_delete_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let profile = GameProfile {
            hdr: true,
            ..Default::default()
        };
        save(dir.path(), 1091500, &profile).unwrap();
        assert_eq!(load(dir.path(), 1091500).unwrap(), profile);
        assert_eq!(load_all(dir.path()).unwrap().len(), 1);
        save(dir.path(), 1091500, &GameProfile::default()).unwrap();
        assert!(!dir.path().join("1091500.json").exists());
        assert_eq!(load(dir.path(), 42).unwrap(), GameProfile::default());
    }
}
