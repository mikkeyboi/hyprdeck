//! Global input options and per-device overrides: live values, provenance and
//! the managed-state operations behind the Input Devices page.

use std::collections::{BTreeMap, HashMap};

use anyhow::Result;
use hyprdeck_core::hypr::managed::{self, OptValue};
use hyprdeck_core::hypr::model::{self, ConfigModel, OptionEntry, SpecEntry, Value};
use hyprdeck_core::hypr::{ctl, schema};

use crate::devices::{self, DeviceGroup};

pub const KEYS: [&str; 29] = [
    "input.kb_layout",
    "input.kb_variant",
    "input.kb_options",
    "input.repeat_rate",
    "input.repeat_delay",
    "input.numlock_by_default",
    "input.resolve_binds_by_sym",
    "input.sensitivity",
    "input.accel_profile",
    "input.natural_scroll",
    "input.scroll_factor",
    "input.left_handed",
    "input.follow_mouse",
    "input.mouse_refocus",
    "input.float_switch_override_focus",
    "input.touchpad.natural_scroll",
    "input.touchpad.tap_to_click",
    "input.touchpad.disable_while_typing",
    "input.touchpad.scroll_factor",
    "input.touchpad.clickfinger_behavior",
    "input.touchpad.middle_button_emulation",
    "input.touchpad.tap_and_drag",
    "cursor.inactive_timeout",
    "cursor.hide_on_key_press",
    "cursor.no_hardware_cursors",
    "cursor.enable_hyprcursor",
    "cursor.no_warps",
    "cursor.hide_on_touch",
    "cursor.warp_on_change_workspace",
];

/// Where an option's current value comes from.
#[derive(Debug, Clone, PartialEq)]
pub enum Origin {
    Default,
    /// Hand-written config, `config/inputs.lua:5`.
    Config(String),
    /// hyprdeck's managed file; `overrides` is the hand-written source it beats.
    Hyprdeck {
        overrides: Option<String>,
    },
}

impl Origin {
    pub fn label(&self) -> &str {
        match self {
            Origin::Default => "default",
            Origin::Config(src) => src,
            Origin::Hyprdeck { .. } => "hyprdeck",
        }
    }

    pub fn tooltip(&self) -> String {
        match self {
            Origin::Default => "Hyprland's built-in default".into(),
            Origin::Config(src) => format!("Set in your config at {src}"),
            Origin::Hyprdeck {
                overrides: Some(src),
            } => format!("Set by hyprdeck, overriding {src}"),
            Origin::Hyprdeck { overrides: None } => "Set by hyprdeck".into(),
        }
    }
}

fn is_managed_src(file: &std::path::Path) -> bool {
    file.file_name().is_some_and(|f| f == "hyprdeck.lua")
}

fn last_handwritten<'a>(m: &'a ConfigModel, key: &str) -> Option<&'a OptionEntry> {
    m.options
        .iter()
        .filter(|o| o.key == key && !is_managed_src(&o.source.file))
        .max_by_key(|o| o.seq)
}

pub fn origin(m: &ConfigModel, managed: &managed::Managed, key: &str) -> Origin {
    let hand = last_handwritten(m, key).map(|o| o.source.display());
    if managed.options.contains_key(key) {
        Origin::Hyprdeck { overrides: hand }
    } else {
        hand.map_or(Origin::Default, Origin::Config)
    }
}

/// Live values of `keys` via `hl.get_config` (blocking). Missing keys are omitted.
pub fn live_values(keys: &[&str]) -> Result<HashMap<String, String>> {
    let list: Vec<String> = keys.iter().map(|k| ctl::lua_str(k)).collect();
    let lua = format!(
        "local out = {{}}\nfor _, k in ipairs({{ {} }}) do\n  local v = hl.get_config(k)\n  \
         if v ~= nil then out[#out + 1] = k .. \"\\t\" .. tostring(v) end\nend\nreturn \"#\" .. table.concat(out, \"\\n\")",
        list.join(", ")
    );
    let out = ctl::repl(&lua)?;
    let body = out.strip_prefix('#').unwrap_or(&out);
    Ok(body
        .lines()
        .filter_map(|l| l.split_once('\t'))
        .map(|(k, v)| (k.to_owned(), v.to_owned()))
        .collect())
}

/// Per-device override state of one device name.
#[derive(Debug, Clone, Default)]
pub struct DeviceSettings {
    /// hyprdeck's `hl.device` settings.
    pub managed: BTreeMap<String, OptValue>,
    /// Hand-written `hl.device` rule fields and its source.
    pub config: Option<(BTreeMap<String, Value>, String)>,
}

fn last_handwritten_device<'a>(m: &'a ConfigModel, name: &str) -> Option<&'a SpecEntry> {
    m.devices
        .iter()
        .filter(|d| d.str("name") == Some(name) && !is_managed_src(&d.source.file))
        .max_by_key(|d| d.seq)
}

pub struct InputState {
    pub values: HashMap<String, String>,
    pub origins: HashMap<&'static str, Origin>,
    pub groups: Vec<DeviceGroup>,
    pub device_settings: HashMap<String, DeviceSettings>,
    pub has_touchpad: bool,
}

impl InputState {
    pub fn value(&self, key: &str) -> &str {
        self.values.get(key).map_or("", String::as_str)
    }

    pub fn origin(&self, key: &str) -> Origin {
        self.origins.get(key).cloned().unwrap_or(Origin::Default)
    }
}

/// Gather everything the page shows (blocking).
pub fn load() -> Result<InputState> {
    let keys: Vec<&str> = KEYS
        .iter()
        .copied()
        .filter(|k| schema::kind(k).is_some())
        .collect();
    let values = live_values(&keys)?;
    let m = model::load();
    let managed = managed::load()?;
    let origins = keys.iter().map(|k| (*k, origin(&m, &managed, k))).collect();
    let devs = ctl::devices()?;
    let groups = devices::group(&devs, &devices::touchpad_names());
    let mut device_settings = HashMap::new();
    for g in &groups {
        for mem in &g.members {
            let s = DeviceSettings {
                managed: managed
                    .devices
                    .iter()
                    .find(|d| d.name == mem.name)
                    .map(|d| d.settings.clone())
                    .unwrap_or_default(),
                config: last_handwritten_device(&m, &mem.name)
                    .map(|d| (d.fields.clone(), d.source.display())),
            };
            device_settings.insert(mem.name.clone(), s);
        }
    }
    let has_touchpad = groups
        .iter()
        .any(|g| g.has(|k| k == devices::DevKind::Touchpad));
    Ok(InputState {
        values,
        origins,
        groups,
        device_settings,
        has_touchpad,
    })
}

/// Set (or with `None` remove) hyprdeck's global options and reload (blocking).
pub fn set_options(changes: &[(&str, Option<OptValue>)]) -> Result<Vec<String>> {
    managed::update(|m| {
        for (k, v) in changes {
            match v {
                Some(v) => {
                    m.options.insert((*k).to_owned(), v.clone());
                }
                None => {
                    m.options.remove(*k);
                }
            }
        }
    })?;
    managed::apply(false)
}

/// Live value + provenance of one key after a change (blocking).
pub fn refresh_key(key: &str) -> Result<(String, Origin)> {
    let v = live_values(&[key])?.remove(key).unwrap_or_default();
    let o = origin(&model::load(), &managed::load()?, key);
    Ok((v, o))
}

/// Set or remove (`None`) one per-device setting on every named device (blocking).
pub fn set_device(names: &[String], key: &str, value: Option<OptValue>) -> Result<Vec<String>> {
    managed::update(|m| {
        for n in names {
            match &value {
                Some(v) => {
                    m.device_mut(n).settings.insert(key.to_owned(), v.clone());
                }
                None => {
                    if let Some(d) = m.devices.iter_mut().find(|d| &d.name == n) {
                        d.settings.remove(key);
                    }
                }
            }
        }
        m.devices.retain(|d| !d.settings.is_empty());
    })?;
    managed::apply(false)
}

/// Drop hyprdeck's rules for these devices (blocking).
pub fn reset_devices(names: &[String]) -> Result<Vec<String>> {
    managed::update(|m| m.devices.retain(|d| !names.contains(&d.name)))?;
    managed::apply(false)
}

/// Parse a `tostring()`ed live value.
pub fn parse_bool(s: &str) -> bool {
    matches!(s.trim(), "true" | "1")
}

/// `hyprdeck input devices`.
pub fn print_devices() -> Result<()> {
    let devs = ctl::devices()?;
    let groups = devices::group(&devs, &devices::touchpad_names());
    let managed = managed::load()?;
    let m = model::load();
    for g in &groups {
        let class = match g.class {
            devices::GroupClass::Physical => "",
            devices::GroupClass::System => " (system)",
            devices::GroupClass::Virtual => " (virtual)",
        };
        println!("{}{class} — {}", g.label, g.kinds());
        for mem in &g.members {
            let mut notes = Vec::new();
            if let Some(d) = managed.devices.iter().find(|d| d.name == mem.name) {
                let s: Vec<String> = d
                    .settings
                    .iter()
                    .map(|(k, v)| format!("{k} = {}", v.to_lua()))
                    .collect();
                notes.push(format!("hyprdeck: {}", s.join(", ")));
            }
            if let Some(d) = last_handwritten_device(&m, &mem.name) {
                notes.push(format!("config rule at {}", d.source.display()));
            }
            let notes = if notes.is_empty() {
                String::new()
            } else {
                format!("  [{}]", notes.join("; "))
            };
            println!("  {:<10} {}{notes}", mem.kind.label(), mem.name);
        }
    }
    let kb: Vec<&str> = devs
        .keyboards
        .iter()
        .filter(|k| k.main)
        .map(|k| k.active_keymap.as_str())
        .collect();
    if let Some(active) = kb.first() {
        println!("\nActive keymap: {active}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_prefers_managed_and_reports_override() {
        let dir = std::env::temp_dir().join(format!("hd-input-origin-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("config")).unwrap();
        std::fs::write(
            dir.join("hyprland.lua"),
            "require('config.inputs')\nrequire('hyprdeck')\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("config/inputs.lua"),
            "hl.config({ input = { accel_profile = 'flat', follow_mouse = 1 } })\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("hyprdeck.lua"),
            "hl.config({ input = { follow_mouse = 2 } })\n",
        )
        .unwrap();
        let m = model::load_from(&dir.join("hyprland.lua"));
        std::fs::remove_dir_all(&dir).unwrap();
        let mut managed = managed::Managed::default();
        managed
            .options
            .insert("input.follow_mouse".into(), OptValue::Int(2));
        match origin(&m, &managed, "input.follow_mouse") {
            Origin::Hyprdeck {
                overrides: Some(src),
            } => assert!(src.ends_with("config/inputs.lua:1"), "{src}"),
            other => panic!("unexpected {other:?}"),
        }
        assert!(matches!(
            origin(&m, &managed, "input.accel_profile"),
            Origin::Config(_)
        ));
        assert_eq!(origin(&m, &managed, "input.sensitivity"), Origin::Default);
    }
}
