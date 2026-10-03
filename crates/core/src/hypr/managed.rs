//! hyprdeck-managed Hyprland settings.
//!
//! Source of truth: `~/.config/hyprdeck/hyprland.toml`. It is rendered to
//! `~/.config/hypr/hyprdeck.lua`, which `hyprland.lua` requires last so these
//! settings override the hand-written config. HyprMod's `hyprland-gui.lua` is
//! imported once and replaced.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use super::ctl::{self, lua_str};
use super::model::{self, Value, format_float};
use crate::store;

const STORE_NAME: &str = "hyprland";
const LUA_MODULE: &str = "hyprdeck";
const HYPRMOD_MODULE: &str = "hyprland-gui";

static LOCK: Mutex<()> = Mutex::new(());

/// A config value. Serialized untagged in TOML; `Lua` holds raw Lua text for
/// composite values (gradients, gaps).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OptValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Lua { lua: String },
}

impl OptValue {
    pub fn to_lua(&self) -> String {
        match self {
            OptValue::Bool(b) => b.to_string(),
            OptValue::Int(i) => i.to_string(),
            OptValue::Float(f) => format_float(*f),
            OptValue::Str(s) => lua_str(s),
            OptValue::Lua { lua } => lua.clone(),
        }
    }
}

impl From<&Value> for OptValue {
    fn from(v: &Value) -> Self {
        match v {
            Value::Bool(b) => OptValue::Bool(*b),
            Value::Int(i) => OptValue::Int(*i),
            Value::Float(f) => OptValue::Float(*f),
            Value::Str(s) => OptValue::Str(s.clone()),
            Value::Lua(t) => OptValue::Lua { lua: t.clone() },
        }
    }
}

/// `hl.monitor({...})`. `None` fields are omitted (compositor default).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct MonitorRule {
    pub output: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    /// `3840x2160@119.88Hz`, `preferred`, `highres`, `highrr`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// `0x0` or `auto`, `auto-right`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scale: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transform: Option<i64>,
    /// 0 off, 1 on, 2 fullscreen only, 3 fullscreen video/game.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vrr: Option<i64>,
    /// Color management preset: auto, srgb, dcip3, dp3, adobe, wide, edid, hdr, hdredid.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cm: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bitdepth: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdrbrightness: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdrsaturation: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdr_min_luminance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdr_max_luminance: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sdr_eotf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_luminance: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_luminance: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_avg_luminance: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_hdr: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supports_wide_color: Option<i64>,
    /// Path to an ICC profile.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icc: Option<String>,
    /// Output name to mirror.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mirror: Option<String>,
}

impl MonitorRule {
    /// `hl.monitor({...})` call text.
    pub fn to_lua(&self) -> String {
        let mut f: Vec<String> = vec![format!("output = {}", lua_str(&self.output))];
        let mut s = |k: &str, v: &Option<String>| {
            if let Some(v) = v {
                f.push(format!("{k} = {}", lua_str(v)));
            }
        };
        s("mode", &self.mode);
        s("position", &self.position);
        s("cm", &self.cm);
        s("sdr_eotf", &self.sdr_eotf);
        s("icc", &self.icc);
        s("mirror", &self.mirror);
        if let Some(v) = self.disabled {
            f.push(format!("disabled = {v}"));
        }
        for (k, v) in [
            ("scale", self.scale),
            ("sdrbrightness", self.sdrbrightness),
            ("sdrsaturation", self.sdrsaturation),
            ("sdr_min_luminance", self.sdr_min_luminance),
            ("min_luminance", self.min_luminance),
        ] {
            if let Some(v) = v {
                f.push(format!("{k} = {}", format_float(v)));
            }
        }
        for (k, v) in [
            ("transform", self.transform),
            ("vrr", self.vrr),
            ("bitdepth", self.bitdepth),
            ("sdr_max_luminance", self.sdr_max_luminance),
            ("max_luminance", self.max_luminance),
            ("max_avg_luminance", self.max_avg_luminance),
            ("supports_hdr", self.supports_hdr),
            ("supports_wide_color", self.supports_wide_color),
        ] {
            if let Some(v) = v {
                f.push(format!("{k} = {v}"));
            }
        }
        format!("hl.monitor({{\n    {},\n}})", f.join(",\n    "))
    }

    fn from_spec(spec: &model::SpecEntry) -> Option<MonitorRule> {
        let g = |k: &str| spec.fields.get(k);
        let s = |k: &str| g(k).map(Value::display);
        let fl = |k: &str| g(k).and_then(Value::as_f64);
        let i = |k: &str| g(k).and_then(Value::as_i64);
        Some(MonitorRule {
            output: spec.str("output")?.to_owned(),
            disabled: g("disabled").and_then(Value::as_bool),
            mode: s("mode"),
            position: s("position"),
            scale: fl("scale"),
            transform: i("transform"),
            vrr: i("vrr"),
            cm: s("cm"),
            bitdepth: i("bitdepth"),
            sdrbrightness: fl("sdrbrightness"),
            sdrsaturation: fl("sdrsaturation"),
            sdr_min_luminance: fl("sdr_min_luminance"),
            sdr_max_luminance: i("sdr_max_luminance"),
            sdr_eotf: s("sdr_eotf"),
            min_luminance: fl("min_luminance"),
            max_luminance: i("max_luminance"),
            max_avg_luminance: i("max_avg_luminance"),
            supports_hdr: i("supports_hdr"),
            supports_wide_color: i("supports_wide_color"),
            icc: s("icc"),
            mirror: s("mirror"),
        })
    }
}

/// `hl.device({ name = ..., ... })` per-device input overrides.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DeviceRule {
    pub name: String,
    #[serde(default)]
    pub settings: BTreeMap<String, OptValue>,
}

impl DeviceRule {
    pub fn to_lua(&self) -> String {
        let mut f = vec![format!("name = {}", lua_str(&self.name))];
        f.extend(
            self.settings
                .iter()
                .map(|(k, v)| format!("{k} = {}", v.to_lua())),
        );
        format!("hl.device({{ {} }})", f.join(", "))
    }
}

/// Bind behaviour flags (`HL.BindOptions`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct BindFlags {
    pub repeating: bool,
    pub locked: bool,
    pub release: bool,
    pub non_consuming: bool,
    pub transparent: bool,
    pub ignore_mods: bool,
    pub long_press: bool,
    pub click: bool,
    pub drag: bool,
}

/// A managed keybind. Rendered as `hl.unbind(keys)` + `hl.bind(...)` so it
/// replaces whatever the hand-written config bound to the same combo.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BindRule {
    pub keys: String,
    /// Dispatcher path under `hl.dsp` (`exec_cmd`, `window.close`, `focus`, …).
    pub dispatcher: String,
    /// Lua argument list text (without parentheses), e.g. `"kitty"` or `{ mode = 1 }`.
    #[serde(default)]
    pub args: String,
    #[serde(default, skip_serializing_if = "BindFlags::is_default")]
    pub flags: BindFlags,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl BindFlags {
    pub fn is_default(&self) -> bool {
        *self == BindFlags::default()
    }
}

impl BindRule {
    /// Convenience constructor for `exec_cmd` binds.
    pub fn exec(keys: &str, command: &str) -> BindRule {
        BindRule {
            keys: keys.to_owned(),
            dispatcher: "exec_cmd".into(),
            args: lua_str(command),
            flags: BindFlags::default(),
            description: None,
        }
    }

    pub fn action_lua(&self) -> String {
        format!("hl.dsp.{}({})", self.dispatcher, self.args)
    }

    pub fn to_lua(&self) -> String {
        let mut opts = Vec::new();
        let fl = &self.flags;
        for (k, v) in [
            ("repeating", fl.repeating),
            ("locked", fl.locked),
            ("release", fl.release),
            ("non_consuming", fl.non_consuming),
            ("transparent", fl.transparent),
            ("ignore_mods", fl.ignore_mods),
            ("long_press", fl.long_press),
            ("click", fl.click),
            ("drag", fl.drag),
        ] {
            if v {
                opts.push(format!("{k} = true"));
            }
        }
        if let Some(d) = &self.description {
            opts.push(format!("description = {}", lua_str(d)));
        }
        let keys = lua_str(&self.keys);
        if opts.is_empty() {
            format!("hl.unbind({keys})\nhl.bind({keys}, {})", self.action_lua())
        } else {
            format!(
                "hl.unbind({keys})\nhl.bind({keys}, {}, {{ {} }})",
                self.action_lua(),
                opts.join(", ")
            )
        }
    }

    fn from_entry(b: &model::BindEntry) -> Option<BindRule> {
        let dispatcher = b.dispatcher.clone()?;
        let prefix = format!("hl.dsp.{dispatcher}(");
        let args = b
            .action_lua
            .strip_prefix(&prefix)?
            .strip_suffix(')')?
            .to_owned();
        let o = &b.opts_lua;
        let has = |k: &str| o.contains(&format!("{k} = true"));
        Some(BindRule {
            keys: b.keys.clone(),
            dispatcher,
            args,
            flags: BindFlags {
                repeating: has("repeating"),
                locked: has("locked"),
                release: has("release"),
                non_consuming: has("non_consuming"),
                transparent: has("transparent"),
                ignore_mods: has("ignore_mods"),
                long_press: has("long_press"),
                click: has("click"),
                drag: has("drag"),
            },
            description: b.description.clone(),
        })
    }
}

/// Everything hyprdeck writes into the compositor config.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Managed {
    /// Dotted option key → value (`input.follow_mouse = 2`).
    #[serde(default)]
    pub options: BTreeMap<String, OptValue>,
    #[serde(default)]
    pub monitors: Vec<MonitorRule>,
    #[serde(default)]
    pub devices: Vec<DeviceRule>,
    /// Combos to remove from the hand-written config (disabled binds).
    #[serde(default)]
    pub unbinds: Vec<String>,
    #[serde(default)]
    pub binds: Vec<BindRule>,
}

impl Managed {
    pub fn monitor_mut(&mut self, output: &str) -> &mut MonitorRule {
        if let Some(i) = self.monitors.iter().position(|m| m.output == output) {
            return &mut self.monitors[i];
        }
        self.monitors.push(MonitorRule {
            output: output.to_owned(),
            ..Default::default()
        });
        self.monitors.last_mut().expect("just pushed")
    }

    pub fn device_mut(&mut self, name: &str) -> &mut DeviceRule {
        if let Some(i) = self.devices.iter().position(|d| d.name == name) {
            return &mut self.devices[i];
        }
        self.devices.push(DeviceRule {
            name: name.to_owned(),
            ..Default::default()
        });
        self.devices.last_mut().expect("just pushed")
    }

    /// Render the Lua module.
    pub fn to_lua(&self) -> String {
        let mut out = String::from(
            "-- Generated by hyprdeck from ~/.config/hyprdeck/hyprland.toml.\n\
             -- Manual edits are overwritten; change these settings in the hyprdeck app.\n",
        );
        if !self.options.is_empty() {
            out.push_str("\n-- Settings\n");
            out.push_str(&render_options(&self.options));
            out.push('\n');
        }
        if !self.monitors.is_empty() {
            out.push_str("\n-- Monitors\n");
            for m in &self.monitors {
                out.push_str(&m.to_lua());
                out.push('\n');
            }
        }
        if !self.devices.is_empty() {
            out.push_str("\n-- Input devices\n");
            for d in &self.devices {
                out.push_str(&d.to_lua());
                out.push('\n');
            }
        }
        if !self.unbinds.is_empty() {
            out.push_str("\n-- Disabled keybinds\n");
            for k in &self.unbinds {
                out.push_str(&format!("hl.unbind({})\n", lua_str(k)));
            }
        }
        if !self.binds.is_empty() {
            out.push_str("\n-- Keybinds\n");
            for b in &self.binds {
                out.push_str(&b.to_lua());
                out.push('\n');
            }
        }
        out
    }
}

/// Nest dotted keys into a single `hl.config({...})` call.
fn render_options(opts: &BTreeMap<String, OptValue>) -> String {
    #[derive(Default)]
    struct Node {
        leaf: Option<String>,
        children: BTreeMap<String, Node>,
    }
    let mut root = Node::default();
    for (key, v) in opts {
        let mut node = &mut root;
        for part in key.split('.') {
            node = node.children.entry(part.to_owned()).or_default();
        }
        node.leaf = Some(v.to_lua());
    }
    fn render(node: &Node, indent: usize, out: &mut String) {
        for (k, child) in &node.children {
            let pad = "    ".repeat(indent);
            match &child.leaf {
                Some(v) => out.push_str(&format!("{pad}{k} = {v},\n")),
                None => {
                    out.push_str(&format!("{pad}{k} = {{\n"));
                    render(child, indent + 1, out);
                    out.push_str(&format!("{pad}}},\n"));
                }
            }
        }
    }
    let mut body = String::new();
    render(&root, 1, &mut body);
    format!("hl.config({{\n{body}}})")
}

pub fn lua_path() -> PathBuf {
    model::hypr_dir().join(format!("{LUA_MODULE}.lua"))
}

/// Load the managed state (empty when never saved).
pub fn load() -> Result<Managed> {
    store::load(STORE_NAME)
}

/// Load, modify and persist atomically; returns the new state. Does not reload
/// the compositor — call [`apply`] for that.
pub fn update(f: impl FnOnce(&mut Managed)) -> Result<Managed> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let mut m = load()?;
    f(&mut m);
    write(&m)?;
    Ok(m)
}

/// Persist state and regenerate `hyprdeck.lua` (syntax-checked first).
pub fn write(m: &Managed) -> Result<()> {
    let lua = m.to_lua();
    mlua::Lua::new()
        .load(&lua)
        .set_name("@hyprdeck.lua")
        .into_function()
        .map_err(|e| anyhow::anyhow!("generated Lua does not compile: {e}"))?;
    store::save(STORE_NAME, m)?;
    store::write_atomic(&lua_path(), lua.as_bytes())?;
    Ok(())
}

/// Reload the compositor so the generated file takes effect, returning any
/// config errors it reports. `monitors` = also re-apply monitor modes.
pub fn apply(monitors: bool) -> Result<Vec<String>> {
    ensure_installed()?;
    ctl::reload(!monitors)?;
    ctl::config_errors()
}

/// Make `hyprland.lua` require the managed module last; migrate HyprMod's
/// `hyprland-gui.lua` into the managed state on first run.
pub fn ensure_installed() -> Result<()> {
    let _guard = LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let main = model::hypr_dir().join("hyprland.lua");
    let text =
        std::fs::read_to_string(&main).with_context(|| format!("reading {}", main.display()))?;
    let ours = format!("require(\"{LUA_MODULE}\")");
    let hyprmod = format!("require(\"{HYPRMOD_MODULE}\")");

    let mut state = load()?;
    if text.contains(&hyprmod) {
        let gui = model::hypr_dir().join(format!("{HYPRMOD_MODULE}.lua"));
        if gui.exists() {
            import_into(&mut state, &model::load_from(&gui))?;
            let backup = store::config_dir().join("backup");
            std::fs::create_dir_all(&backup)?;
            std::fs::rename(&gui, backup.join(format!("{HYPRMOD_MODULE}.lua")))
                .context("moving HyprMod's hyprland-gui.lua to the hyprdeck backup dir")?;
        }
    }
    if !lua_path().exists() || text.contains(&hyprmod) {
        write(&state)?;
    }

    let mut lines: Vec<String> = text
        .lines()
        .filter(|l| {
            let t = l.trim();
            t != hyprmod
                && t != ours
                && t != "-- HyprMod managed settings"
                && t != "-- hyprdeck managed settings (keep last)"
        })
        .map(str::to_owned)
        .collect();
    while lines.last().is_some_and(|l| l.trim().is_empty()) {
        lines.pop();
    }
    lines.push(String::new());
    lines.push("-- hyprdeck managed settings (keep last)".into());
    lines.push(ours);
    let new_text = lines.join("\n") + "\n";
    if new_text != text {
        store::write_atomic(&main, new_text.as_bytes())?;
    }
    Ok(())
}

/// Merge settings recorded from a standalone file into the managed state.
fn import_into(state: &mut Managed, m: &model::ConfigModel) -> Result<()> {
    if let Some(e) = m.errors.first() {
        bail!("cannot import HyprMod settings: {e}");
    }
    for o in &m.options {
        state
            .options
            .insert(o.key.clone(), OptValue::from(&o.value));
    }
    for spec in &m.monitors {
        if let Some(rule) = MonitorRule::from_spec(spec) {
            let output = rule.output.clone();
            *state.monitor_mut(&output) = rule;
        }
    }
    for spec in &m.devices {
        if let Some(name) = spec.str("name") {
            let dev = state.device_mut(name);
            for (k, v) in spec.fields.iter().filter(|(k, _)| k.as_str() != "name") {
                dev.settings.insert(k.clone(), OptValue::from(v));
            }
        }
    }
    for b in &m.binds {
        let Some(rule) = BindRule::from_entry(b) else {
            continue;
        };
        state
            .binds
            .retain(|x| !model::Combo::parse(&x.keys).matches(&b.combo));
        state.binds.push(rule);
    }
    // Unbinds that only cleared a combo re-bound in the same file are implied by
    // BindRule rendering; keep the rest as explicit disables.
    for u in &m.unbinds {
        let rebound = m.binds.iter().any(|b| b.combo.matches(&u.combo));
        if !rebound
            && !state
                .unbinds
                .iter()
                .any(|k| model::Combo::parse(k).matches(&u.combo))
        {
            state.unbinds.push(u.keys.clone());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn renders_nested_options_and_binds_that_compile() {
        let mut m = Managed::default();
        m.options
            .insert("input.follow_mouse".into(), OptValue::Int(2));
        m.options
            .insert("input.touchpad.natural_scroll".into(), OptValue::Bool(true));
        m.options.insert("misc.vrr".into(), OptValue::Int(0));
        m.monitor_mut("HDMI-A-1").mode = Some("3840x2160@119.88Hz".into());
        m.monitor_mut("HDMI-A-1").scale = Some(1.0);
        m.binds
            .push(BindRule::exec("SUPER + W", "uwsm app -- \"firefox\""));
        m.unbinds.push("SUPER + J".into());
        let lua = m.to_lua();
        assert!(lua.contains("    input = {\n        follow_mouse = 2,\n        touchpad = {\n            natural_scroll = true,"));
        assert!(lua.contains("scale = 1.0"));
        assert!(lua.contains("hl.unbind(\"SUPER + W\")\nhl.bind(\"SUPER + W\", hl.dsp.exec_cmd(\"uwsm app -- \\\"firefox\\\"\"))"));

        // Round trip: evaluating the generated file reproduces the state.
        let dir = std::env::temp_dir().join(format!("hyprdeck-managed-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("hyprdeck.lua");
        std::fs::write(&file, &lua).unwrap();
        let model = model::load_from(&file);
        std::fs::remove_dir_all(&dir).unwrap();
        let mut back = Managed::default();
        import_into(&mut back, &model).unwrap();
        assert_eq!(back, m);
    }

    #[test]
    fn toml_round_trip_keeps_value_types() {
        let mut m = Managed::default();
        m.options.insert("a.int".into(), OptValue::Int(3));
        m.options.insert("a.float".into(), OptValue::Float(0.5));
        m.options.insert("a.str".into(), OptValue::Str("x".into()));
        m.options.insert(
            "a.lua".into(),
            OptValue::Lua {
                lua: "{ top = 1 }".into(),
            },
        );
        let text = toml::to_string_pretty(&m).unwrap();
        assert_eq!(toml::from_str::<Managed>(&text).unwrap(), m);
    }
}
