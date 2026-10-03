//! DDC/CI monitor controls through `ddcutil`. Every call takes ~0.1–1 s and
//! the I²C bus tolerates only one transaction at a time, so calls are
//! serialized per bus. All functions block.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, LazyLock, Mutex};

use anyhow::{Context, Result, bail};
use hyprdeck_core::cmd;

pub const BRIGHTNESS: u8 = 0x10;
pub const CONTRAST: u8 = 0x12;
pub const INPUT_SOURCE: u8 = 0x60;
pub const POWER_MODE: u8 = 0xD6;

/// A display found by `ddcutil detect --brief`.
#[derive(Debug, Clone, PartialEq)]
pub struct DdcDisplay {
    pub bus: u32,
    /// DRM connector without the card prefix (`HDMI-A-1`).
    pub connector: Option<String>,
    /// `MFG:MODEL:SERIAL` as reported by ddcutil.
    pub monitor: String,
}

/// VCP features advertised by `ddcutil capabilities`, with value names for
/// non-continuous ones (`0x60` → `0x0f: DisplayPort-1`, …).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Capabilities {
    pub features: BTreeMap<u8, Vec<(u8, String)>>,
}

impl Capabilities {
    pub fn has(&self, code: u8) -> bool {
        self.features.contains_key(&code)
    }

    pub fn values(&self, code: u8) -> &[(u8, String)] {
        self.features.get(&code).map_or(&[], Vec::as_slice)
    }
}

/// A VCP reading: continuous (`current`/`max`) or a non-continuous value byte.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Vcp {
    Continuous { current: u16, max: u16 },
    Value(u8),
}

static BUS_LOCKS: LazyLock<Mutex<HashMap<u32, Arc<Mutex<()>>>>> = LazyLock::new(Default::default);

fn bus_lock(bus: u32) -> Arc<Mutex<()>> {
    let mut map = BUS_LOCKS.lock().unwrap_or_else(|e| e.into_inner());
    map.entry(bus).or_default().clone()
}

fn ddcutil(bus: u32, args: &[&str]) -> Result<String> {
    let lock = bus_lock(bus);
    let _guard = lock.lock().unwrap_or_else(|e| e.into_inner());
    let bus = bus.to_string();
    let mut all = vec!["--bus", bus.as_str()];
    all.extend_from_slice(args);
    let out = cmd::output("ddcutil", &all)?;
    if !out.ok() {
        let msg = if out.stderr.trim().is_empty() {
            out.stdout.trim()
        } else {
            out.stderr.trim()
        };
        bail!("ddcutil {}: {msg}", args.join(" "));
    }
    Ok(out.stdout)
}

/// Whether DDC/CI can be used at all on this machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    /// ddcutil is installed and an I²C device can be opened.
    Ready,
    /// ddcutil is not installed: monitor controls are simply absent.
    NoDdcutil,
    /// ddcutil is installed but no `/dev/i2c-*` exists (`i2c-dev` not loaded).
    NoDevices,
    /// `/dev/i2c-*` exist but can't be opened; group owning them, if not root.
    Denied(Option<String>),
}

impl Access {
    /// One-line explanation for the page, `None` when there is nothing to fix.
    pub fn hint(&self) -> Option<String> {
        match self {
            Access::Ready | Access::NoDdcutil => None,
            Access::NoDevices => Some(
                "Monitor brightness and input controls need the i2c-dev kernel module: run “sudo modprobe i2c-dev” and add i2c-dev to a file in /etc/modules-load.d/ to load it at boot."
                    .into(),
            ),
            Access::Denied(Some(group)) => Some(format!(
                "Monitor brightness and input controls need access to /dev/i2c-*: add yourself to the {group} group (“sudo usermod -aG {group} $USER”) and log in again."
            )),
            Access::Denied(None) => Some(
                "Monitor brightness and input controls need access to /dev/i2c-*: install ddcutil's udev rule or give an i2c group read/write access, then log in again."
                    .into(),
            ),
        }
    }
}

/// Check ddcutil and I²C device access (blocking, cheap).
pub fn access() -> Access {
    use std::os::unix::fs::MetadataExt;
    if cmd::which("ddcutil").is_none() {
        return Access::NoDdcutil;
    }
    let devices: Vec<std::path::PathBuf> = std::fs::read_dir("/dev")
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().starts_with("i2c-"))
        .map(|e| e.path())
        .collect();
    if devices.is_empty() {
        return Access::NoDevices;
    }
    if devices.iter().any(|p| {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(p)
            .is_ok()
    }) {
        return Access::Ready;
    }
    let gid = devices
        .iter()
        .find_map(|p| p.metadata().ok())
        .map(|m| m.gid());
    let group = gid.filter(|&g| g != 0).and_then(|gid| {
        let groups = std::fs::read_to_string("/etc/group").ok()?;
        group_name(&groups, gid)
    });
    Access::Denied(group)
}

/// Name of `gid` in `/etc/group` text.
fn group_name(groups: &str, gid: u32) -> Option<String> {
    groups.lines().find_map(|l| {
        let mut f = l.split(':');
        let name = f.next()?;
        (f.nth(1)?.parse::<u32>().ok()? == gid).then(|| name.to_owned())
    })
}

pub fn detect() -> Result<Vec<DdcDisplay>> {
    let out = cmd::output("ddcutil", ["detect", "--brief"])?;
    if !out.ok() && out.stdout.trim().is_empty() {
        bail!("ddcutil detect: {}", out.stderr.trim());
    }
    Ok(parse_detect(&out.stdout))
}

pub fn parse_detect(text: &str) -> Vec<DdcDisplay> {
    let mut out = Vec::new();
    let mut cur: Option<DdcDisplay> = None;
    for line in text.lines() {
        let t = line.trim();
        if line.starts_with("Display ")
            || line.starts_with("Invalid display")
            || line.starts_with("Phantom display")
        {
            out.extend(cur.take());
            if line.starts_with("Display ") {
                cur = Some(DdcDisplay {
                    bus: u32::MAX,
                    connector: None,
                    monitor: String::new(),
                });
            }
            continue;
        }
        let Some(d) = cur.as_mut() else { continue };
        let Some((key, val)) = t.split_once(':') else {
            continue;
        };
        let val = val.trim();
        match key {
            "I2C bus" => {
                if let Some(n) = val.strip_prefix("/dev/i2c-").and_then(|n| n.parse().ok()) {
                    d.bus = n;
                }
            }
            "DRM connector" => {
                // `card1-HDMI-A-1` → `HDMI-A-1`.
                let name = val
                    .split_once('-')
                    .filter(|(card, _)| card.starts_with("card"))
                    .map_or(val, |(_, n)| n);
                d.connector = Some(name.to_owned());
            }
            "Monitor" => d.monitor = val.to_owned(),
            _ => {}
        }
    }
    out.extend(cur);
    out.retain(|d| d.bus != u32::MAX);
    out
}

pub fn capabilities(bus: u32) -> Result<Capabilities> {
    Ok(parse_capabilities(&ddcutil(bus, &["capabilities"])?))
}

pub fn parse_capabilities(text: &str) -> Capabilities {
    let mut caps = Capabilities::default();
    let mut current: Option<u8> = None;
    let mut in_values = false;
    for line in text.lines() {
        let t = line.trim();
        if let Some(rest) = t.strip_prefix("Feature: ") {
            current = rest.get(..2).and_then(|h| u8::from_str_radix(h, 16).ok());
            if let Some(code) = current {
                caps.features.entry(code).or_default();
            }
            in_values = false;
            continue;
        }
        let Some(code) = current else { continue };
        if t == "Values:" {
            in_values = true;
            continue;
        }
        if !in_values {
            continue;
        }
        // `0f: DisplayPort-1`; un-interpreted lists (`Values: 00 04 …`) are skipped.
        if let Some((hex, name)) = t.split_once(": ")
            && hex.len() == 2
            && let Ok(v) = u8::from_str_radix(hex, 16)
        {
            caps.features
                .entry(code)
                .or_default()
                .push((v, name.trim().to_owned()));
        } else if !t.is_empty() && !t.starts_with("Values") {
            in_values = false;
        }
    }
    caps
}

pub fn get(bus: u32, code: u8) -> Result<Vcp> {
    let code_s = format!("{code:02x}");
    let out = ddcutil(bus, &["-t", "getvcp", &code_s])?;
    parse_getvcp(&out).with_context(|| format!("unexpected ddcutil getvcp output: {}", out.trim()))
}

/// `VCP 10 C 75 100` / `VCP 60 SNC x12`.
pub fn parse_getvcp(text: &str) -> Option<Vcp> {
    let line = text.lines().find(|l| l.starts_with("VCP "))?;
    let f: Vec<&str> = line.split_whitespace().collect();
    match f.get(2)? {
        &"C" => Some(Vcp::Continuous {
            current: f.get(3)?.parse().ok()?,
            max: f.get(4)?.parse().ok()?,
        }),
        &"SNC" | &"CNC" => u8::from_str_radix(f.get(3)?.trim_start_matches('x'), 16)
            .ok()
            .map(Vcp::Value),
        _ => None,
    }
}

pub fn set(bus: u32, code: u8, value: u16) -> Result<()> {
    let code_s = format!("{code:02x}");
    let value_s = value.to_string();
    ddcutil(bus, &["setvcp", &code_s, &value_s]).map(drop)
}

/// The DDC display driving a Hyprland output.
pub fn for_output<'a>(displays: &'a [DdcDisplay], output: &str) -> Option<&'a DdcDisplay> {
    displays
        .iter()
        .find(|d| d.connector.as_deref() == Some(output))
}

/// Set brightness as a percentage of the monitor's maximum.
pub fn set_brightness_percent(bus: u32, percent: u8) -> Result<u16> {
    let max = match get(bus, BRIGHTNESS)? {
        Vcp::Continuous { max, .. } if max > 0 => max,
        _ => 100,
    };
    let value = (u32::from(percent.min(100)) * u32::from(max) / 100) as u16;
    set(bus, BRIGHTNESS, value)?;
    Ok(value)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_detect() {
        let text = "Display 1\n   I2C bus:          /dev/i2c-1\n   DRM connector:    card1-HDMI-A-1\n   \
                    drm_connector_id: 100\n   Monitor:          ABC:Example Monitor:0000001\n\n\
                    Invalid display\n   I2C bus:          /dev/i2c-5\n   DRM connector:    card1-DP-2\n\n\
                    Display 2\n   I2C bus:          /dev/i2c-7\n   Monitor:          XYZ:Other:1\n";
        let d = parse_detect(text);
        assert_eq!(d.len(), 2);
        assert_eq!(
            d[0],
            DdcDisplay {
                bus: 1,
                connector: Some("HDMI-A-1".into()),
                monitor: "ABC:Example Monitor:0000001".into()
            }
        );
        assert_eq!(d[1].bus, 7);
        assert_eq!(d[1].connector, None);
        assert_eq!(for_output(&d, "HDMI-A-1").map(|d| d.bus), Some(1));
        assert!(for_output(&d, "DP-2").is_none());
    }

    #[test]
    fn parses_capabilities() {
        let text = "Model: Example Monitor\nVCP Features:\n   Feature: 10 (Brightness)\n   Feature: 12 (Contrast)\n   \
                    Feature: 60 (Input Source)\n      Values:\n         0f: DisplayPort-1\n         11: HDMI-1\n         \
                    12: HDMI-2\n   Feature: D6 (Power mode)\n      Values:\n         01: DPM: On,  DPMS: Off\n         \
                    05: Write only value to turn off display\n   Feature: E2 (Manufacturer specific feature)\n      \
                    Values: 00 04 0E (interpretation unavailable)\n";
        let c = parse_capabilities(text);
        assert!(c.has(BRIGHTNESS) && c.has(CONTRAST) && c.has(0xE2));
        assert!(!c.has(0x14));
        assert_eq!(
            c.values(INPUT_SOURCE),
            [
                (0x0f, "DisplayPort-1".into()),
                (0x11, "HDMI-1".into()),
                (0x12, "HDMI-2".into())
            ]
        );
        assert_eq!(
            c.values(POWER_MODE)[0],
            (0x01, "DPM: On,  DPMS: Off".into())
        );
        assert!(c.values(0xE2).is_empty());
    }

    #[test]
    fn group_names() {
        let groups = "root:x:0:\nvideo:x:985:a,b\ni2c:x:972:\n";
        assert_eq!(group_name(groups, 972).as_deref(), Some("i2c"));
        assert_eq!(group_name(groups, 1), None);
    }

    #[test]
    fn parses_getvcp() {
        assert_eq!(
            parse_getvcp("VCP 10 C 75 100\n"),
            Some(Vcp::Continuous {
                current: 75,
                max: 100
            })
        );
        assert_eq!(parse_getvcp("VCP 60 SNC x12\n"), Some(Vcp::Value(0x12)));
        assert_eq!(parse_getvcp("VCP 10 ERR\n"), None);
        assert_eq!(parse_getvcp(""), None);
    }
}
