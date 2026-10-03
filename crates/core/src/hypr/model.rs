//! Static model of the user's Hyprland Lua config, produced by evaluating it
//! against a recording mock of the `hl` API (see `mock.lua`). This resolves
//! variables, loops and `require`s exactly like the compositor would, and keeps
//! the source file/line of every bind, rule and option.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mlua::{Lua, Table, Value as LuaValue};

use super::schema;

const MOCK: &str = include_str!("mock.lua");

/// Directory holding `hyprland.lua`.
pub fn hypr_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| crate::store::home().join(".config"))
        .join("hypr")
}

#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    /// Absolute path of the Lua file.
    pub file: PathBuf,
    pub line: u32,
}

impl Source {
    /// `config/binds.lua:12` relative to the hypr dir when possible.
    pub fn display(&self) -> String {
        let dir = hypr_dir();
        let rel = self.file.strip_prefix(&dir).unwrap_or(&self.file);
        format!("{}:{}", rel.display(), self.line)
    }
}

/// A scalar or rendered Lua value.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    /// Anything else, rendered as Lua source text.
    Lua(String),
}

impl Value {
    /// Lua source text for this value.
    pub fn to_lua(&self) -> String {
        match self {
            Value::Bool(b) => b.to_string(),
            Value::Int(i) => i.to_string(),
            Value::Float(f) => format_float(*f),
            Value::Str(s) => super::ctl::lua_str(s),
            Value::Lua(t) => t.clone(),
        }
    }

    /// Human-readable form (strings unquoted).
    pub fn display(&self) -> String {
        match self {
            Value::Str(s) => s.clone(),
            other => other.to_lua(),
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Value::Bool(b) => Some(*b),
            Value::Int(i) => Some(*i != 0),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Int(i) => Some(*i as f64),
            Value::Float(f) => Some(*f),
            Value::Bool(b) => Some(f64::from(u8::from(*b))),
            Value::Str(s) => s.trim().parse().ok(),
            Value::Lua(_) => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) if f.fract() == 0.0 => Some(*f as i64),
            Value::Bool(b) => Some(i64::from(*b)),
            Value::Str(s) => s.trim().parse().ok(),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }
}

/// Render a float so Lua reads it back as a float (`1.0`, not `1`).
pub fn format_float(f: f64) -> String {
    if f.is_finite() && f.fract() == 0.0 {
        format!("{f:.1}")
    } else {
        format!("{f}")
    }
}

/// Canonical key combination used for comparing binds (`SUPER + SHIFT + S`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Combo(pub String);

const MOD_ORDER: [&str; 6] = ["SUPER", "CTRL", "ALT", "SHIFT", "MOD3", "MOD5"];

impl Combo {
    pub fn parse(keys: &str) -> Combo {
        let mut mods: Vec<&'static str> = Vec::new();
        let mut rest: Vec<String> = Vec::new();
        for tok in keys.split('+').map(str::trim).filter(|t| !t.is_empty()) {
            let up = tok.to_ascii_uppercase();
            let m = match up.as_str() {
                "SUPER" | "WIN" | "LOGO" | "MOD4" | "META" => Some("SUPER"),
                "CTRL" | "CONTROL" => Some("CTRL"),
                "ALT" | "MOD1" => Some("ALT"),
                "SHIFT" => Some("SHIFT"),
                "MOD3" => Some("MOD3"),
                "MOD5" => Some("MOD5"),
                _ => None,
            };
            match m {
                Some(m) if !mods.contains(&m) => mods.push(m),
                Some(_) => {}
                None if tok.chars().count() == 1 => rest.push(up),
                None => rest.push(tok.to_owned()),
            }
        }
        mods.sort_by_key(|m| MOD_ORDER.iter().position(|o| o == m));
        let mut parts: Vec<String> = mods.into_iter().map(str::to_owned).collect();
        parts.extend(rest);
        Combo(parts.join(" + "))
    }

    /// Case-insensitive identity for matching (`super + q` == `SUPER + Q`).
    pub fn matches(&self, other: &Combo) -> bool {
        self.0.eq_ignore_ascii_case(&other.0)
    }
}

#[derive(Debug, Clone)]
pub struct BindEntry {
    pub seq: u64,
    /// Key string as written (`SUPER + SHIFT + S`).
    pub keys: String,
    pub combo: Combo,
    /// Dispatcher path (`exec_cmd`, `window.close`, …) or `None` for a raw Lua function.
    pub dispatcher: Option<String>,
    /// Full Lua text of the action (`hl.dsp.exec_cmd("kitty")` or `<lua function>`).
    pub action_lua: String,
    /// Decoded command for `exec_cmd` actions.
    pub exec: Option<String>,
    /// Bind options rendered (`{ locked = true }`), empty when none.
    pub opts_lua: String,
    pub description: Option<String>,
    pub submap: String,
    pub source: Source,
}

#[derive(Debug, Clone)]
pub struct UnbindEntry {
    pub seq: u64,
    pub keys: String,
    pub combo: Combo,
    pub source: Source,
}

#[derive(Debug, Clone)]
pub struct OptionEntry {
    pub seq: u64,
    /// Dotted key (`input.follow_mouse`).
    pub key: String,
    pub value: Value,
    pub source: Source,
}

/// A table passed to `hl.monitor`, `hl.device`, `hl.gesture`, rule constructors…
#[derive(Debug, Clone)]
pub struct SpecEntry {
    pub seq: u64,
    pub fields: BTreeMap<String, Value>,
    pub source: Source,
}

impl SpecEntry {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(Value::as_str)
    }
}

#[derive(Debug, Clone)]
pub struct ExecEntry {
    pub seq: u64,
    pub cmd: String,
    /// Inside an `hl.on("hyprland.start", …)` callback, i.e. runs at login.
    pub on_start: bool,
    pub source: Source,
}

#[derive(Debug, Clone, Default)]
pub struct ConfigModel {
    pub binds: Vec<BindEntry>,
    pub unbinds: Vec<UnbindEntry>,
    pub options: Vec<OptionEntry>,
    pub monitors: Vec<SpecEntry>,
    pub devices: Vec<SpecEntry>,
    pub gestures: Vec<SpecEntry>,
    pub window_rules: Vec<SpecEntry>,
    pub layer_rules: Vec<SpecEntry>,
    pub workspace_rules: Vec<SpecEntry>,
    pub execs: Vec<ExecEntry>,
    pub env: Vec<(String, String, Source)>,
    /// Evaluation errors (the model is still populated up to the failure).
    pub errors: Vec<String>,
}

impl ConfigModel {
    /// Binds still active after later `hl.unbind` calls removed earlier ones.
    pub fn effective_binds(&self) -> Vec<&BindEntry> {
        self.binds
            .iter()
            .filter(|b| {
                !self
                    .unbinds
                    .iter()
                    .any(|u| u.seq > b.seq && u.combo.matches(&b.combo))
            })
            .collect()
    }

    /// Final value of an option (last assignment wins).
    pub fn option(&self, key: &str) -> Option<&OptionEntry> {
        self.options
            .iter()
            .filter(|o| o.key == key)
            .max_by_key(|o| o.seq)
    }

    /// Last `hl.monitor` rule for an output name.
    pub fn monitor_rule(&self, output: &str) -> Option<&SpecEntry> {
        self.monitors
            .iter()
            .filter(|m| m.str("output") == Some(output))
            .max_by_key(|m| m.seq)
    }

    /// Last `hl.device` rule for a device name.
    pub fn device_rule(&self, name: &str) -> Option<&SpecEntry> {
        self.devices
            .iter()
            .filter(|m| m.str("name") == Some(name))
            .max_by_key(|m| m.seq)
    }

    /// Commands launched from `hl.on("hyprland.start", …)`.
    pub fn startup_execs(&self) -> impl Iterator<Item = &ExecEntry> {
        self.execs.iter().filter(|e| e.on_start)
    }
}

/// Evaluate `~/.config/hypr/hyprland.lua`.
pub fn load() -> ConfigModel {
    load_from(&hypr_dir().join("hyprland.lua"))
}

/// Evaluate an entry config file. Never fails: errors land in `model.errors`.
pub fn load_from(entry: &Path) -> ConfigModel {
    match evaluate(entry) {
        Ok(model) => model,
        Err(e) => ConfigModel {
            errors: vec![format!("{e:#}")],
            ..Default::default()
        },
    }
}

fn evaluate(entry: &Path) -> Result<ConfigModel> {
    let dir = entry.parent().context("config path has no parent")?;
    // SAFETY: the debug library is only used by our own mock (`debug.getinfo`
    // for source locations); mlua requires `unsafe_new_with` to load it.
    let lua = unsafe {
        Lua::unsafe_new_with(
            mlua::StdLib::ALL_SAFE | mlua::StdLib::DEBUG,
            mlua::LuaOptions::new(),
        )
    };
    lua.load(MOCK)
        .set_name("@hyprdeck-mock.lua")
        .exec()
        .map_err(lua_err)?;
    let package: Table = lua.globals().get("package").map_err(lua_err)?;
    package
        .set("path", format!("{0}/?.lua;{0}/?/init.lua", dir.display()))
        .map_err(lua_err)?;

    let code =
        std::fs::read_to_string(entry).with_context(|| format!("reading {}", entry.display()))?;
    let mut errors = Vec::new();
    if let Err(e) = lua
        .load(&code)
        .set_name(format!("@{}", entry.display()))
        .exec()
    {
        errors.push(format!("{e}"));
    }

    let ser: mlua::Function = lua.globals().get("__hd_ser").map_err(lua_err)?;
    let records: Table = lua.globals().get("__hd_records").map_err(lua_err)?;
    let mut model = ConfigModel {
        errors,
        ..Default::default()
    };
    for rec in records.sequence_values::<Table>() {
        let rec = rec.map_err(lua_err)?;
        read_record(&rec, &ser, &mut model).map_err(lua_err)?;
    }
    Ok(model)
}

fn lua_err(e: mlua::Error) -> anyhow::Error {
    anyhow::anyhow!("lua: {e}")
}

fn read_record(rec: &Table, ser: &mlua::Function, model: &mut ConfigModel) -> mlua::Result<()> {
    let kind: String = rec.get("kind")?;
    let seq: u64 = rec.get("seq")?;
    let source = Source {
        file: PathBuf::from(rec.get::<String>("file")?),
        line: rec.get::<u32>("line").unwrap_or(0),
    };
    match kind.as_str() {
        "bind" => {
            let keys: String = rec.get::<Option<String>>("keys")?.unwrap_or_default();
            let action: LuaValue = rec.get("action")?;
            let (dispatcher, exec) = match &action {
                LuaValue::Table(t) => {
                    let d: Option<String> = t.get("__hd_dsp")?;
                    let exec = if d.as_deref() == Some("exec_cmd") {
                        t.get::<Option<String>>(1)?
                    } else {
                        None
                    };
                    (d, exec)
                }
                _ => (None, None),
            };
            let opts: LuaValue = rec.get("opts")?;
            let (opts_lua, description) = match &opts {
                LuaValue::Table(t) => {
                    let desc = t
                        .get::<Option<String>>("description")?
                        .or(t.get::<Option<String>>("desc")?);
                    (ser.call::<String>(opts.clone())?, desc)
                }
                _ => (String::new(), None),
            };
            model.binds.push(BindEntry {
                seq,
                combo: Combo::parse(&keys),
                keys,
                dispatcher,
                action_lua: ser.call::<String>(action)?,
                exec,
                opts_lua,
                description,
                submap: rec.get("submap")?,
                source,
            });
        }
        "unbind" => {
            let keys: String = rec.get::<Option<String>>("keys")?.unwrap_or_default();
            model.unbinds.push(UnbindEntry {
                seq,
                combo: Combo::parse(&keys),
                keys,
                source,
            });
        }
        "config" => {
            if let LuaValue::Table(t) = rec.get::<LuaValue>("value")? {
                let mut flat = Vec::new();
                flatten("", &t, ser, &mut flat)?;
                for (key, value) in flat {
                    model.options.push(OptionEntry {
                        seq,
                        key,
                        value,
                        source: source.clone(),
                    });
                }
            }
        }
        "monitor" | "device" | "gesture" | "window_rule" | "layer_rule" | "workspace_rule" => {
            let fields = match rec.get::<LuaValue>("value")? {
                LuaValue::Table(t) => spec_fields(&t, ser)?,
                _ => BTreeMap::new(),
            };
            let entry = SpecEntry {
                seq,
                fields,
                source,
            };
            match kind.as_str() {
                "monitor" => model.monitors.push(entry),
                "device" => model.devices.push(entry),
                "gesture" => model.gestures.push(entry),
                "window_rule" => model.window_rules.push(entry),
                "layer_rule" => model.layer_rules.push(entry),
                _ => model.workspace_rules.push(entry),
            }
        }
        "exec" => {
            if let Some(cmd) = rec.get::<Option<String>>("cmd")? {
                model.execs.push(ExecEntry {
                    seq,
                    cmd,
                    on_start: rec.get("on_start")?,
                    source,
                });
            }
        }
        "env" => {
            let k: Option<String> = rec.get("key")?;
            let v: LuaValue = rec.get("value")?;
            if let Some(k) = k {
                let v = match v {
                    LuaValue::String(s) => s.to_string_lossy(),
                    other => ser.call::<String>(other)?,
                };
                model.env.push((k, v, source));
            }
        }
        "error" => {
            let msg: String = rec.get("msg")?;
            model.errors.push(format!("{}: {msg}", source.display()));
        }
        _ => {}
    }
    Ok(())
}

fn to_value(v: LuaValue, ser: &mlua::Function) -> mlua::Result<Value> {
    Ok(match v {
        LuaValue::Boolean(b) => Value::Bool(b),
        LuaValue::Integer(i) => Value::Int(i),
        LuaValue::Number(n) => Value::Float(n),
        LuaValue::String(s) => Value::Str(s.to_string_lossy()),
        other => Value::Lua(ser.call::<String>(other)?),
    })
}

fn spec_fields(t: &Table, ser: &mlua::Function) -> mlua::Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for pair in t.pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair?;
        let key = match k {
            LuaValue::String(s) => s.to_string_lossy(),
            LuaValue::Integer(i) => i.to_string(),
            _ => continue,
        };
        out.insert(key, to_value(v, ser)?);
    }
    Ok(out)
}

/// Flatten nested `hl.config` tables into dotted keys, stopping at known
/// option keys so composite values (gradients, gaps) stay intact.
fn flatten(
    prefix: &str,
    t: &Table,
    ser: &mlua::Function,
    out: &mut Vec<(String, Value)>,
) -> mlua::Result<()> {
    for pair in t.pairs::<LuaValue, LuaValue>() {
        let (k, v) = pair?;
        let LuaValue::String(k) = k else { continue };
        let key = if prefix.is_empty() {
            k.to_string_lossy()
        } else {
            format!("{prefix}.{}", k.to_string_lossy())
        };
        match v {
            LuaValue::Table(sub) if schema::kind(&key).is_none() => flatten(&key, &sub, ser, out)?,
            other => out.push((key, to_value(other, ser)?)),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval_str(files: &[(&str, &str)]) -> ConfigModel {
        let dir = std::env::temp_dir().join(format!(
            "hyprdeck-model-{}-{}",
            std::process::id(),
            files.len()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        for (name, body) in files {
            let p = dir.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
        let m = load_from(&dir.join("hyprland.lua"));
        std::fs::remove_dir_all(&dir).unwrap();
        m
    }

    #[test]
    fn resolves_requires_loops_unbinds_and_start_execs() {
        let m = eval_str(&[
            (
                "hyprland.lua",
                "require('config.binds')\nhl.on('hyprland.start', function() hl.exec_cmd('noctalia') end)\n\
                 hl.config({ input = { follow_mouse = 2, touchpad = { natural_scroll = true } }, misc = { vrr = 0 } })\n\
                 hl.unbind('SUPER + W')\nhl.bind('SUPER + W', hl.dsp.exec_cmd('chromium'), { locked = true })\n",
            ),
            (
                "config/binds.lua",
                "local mod = 'SUPER'\nhl.bind(mod .. ' + W', hl.dsp.exec_cmd('firefox'))\n\
                 for i = 1, 2 do hl.bind(mod .. ' + ' .. i, hl.dsp.focus({ workspace = i })) end\n\
                 hl.exec_cmd('not-at-start')\n",
            ),
        ]);
        assert!(m.errors.is_empty(), "{:?}", m.errors);
        let eff = m.effective_binds();
        let w: Vec<_> = eff
            .iter()
            .filter(|b| b.combo == Combo::parse("super+w"))
            .collect();
        assert_eq!(w.len(), 1, "unbind must remove only the earlier SUPER+W");
        assert_eq!(w[0].exec.as_deref(), Some("chromium"));
        assert_eq!(w[0].opts_lua, "{ locked = true }");
        let ws2 = eff.iter().find(|b| b.keys == "SUPER + 2").unwrap();
        assert_eq!(ws2.action_lua, "hl.dsp.focus({ workspace = 2 })");
        assert!(ws2.source.file.ends_with("config/binds.lua"));
        assert_eq!(ws2.source.line, 3);
        let start: Vec<_> = m.startup_execs().map(|e| e.cmd.as_str()).collect();
        assert_eq!(start, ["noctalia"]);
        assert_eq!(m.option("input.follow_mouse").unwrap().value, Value::Int(2));
        assert_eq!(
            m.option("input.touchpad.natural_scroll").unwrap().value,
            Value::Bool(true)
        );
    }

    #[test]
    fn syntax_error_keeps_partial_model() {
        let m = eval_str(&[(
            "hyprland.lua",
            "hl.bind('SUPER + Q', hl.dsp.window.close())\nthis is not lua",
        )]);
        assert_eq!(m.errors.len(), 1);
        assert!(m.binds.is_empty(), "chunk fails to compile, so nothing ran");
    }

    #[test]
    fn combo_normalizes_aliases_and_order() {
        assert_eq!(Combo::parse("SHIFT + super + s").0, "SUPER + SHIFT + S");
        assert_eq!(
            Combo::parse("CONTROL + ALT + Delete").0,
            "CTRL + ALT + Delete"
        );
        assert!(Combo::parse("SUPER + mouse:272").matches(&Combo::parse("super + MOUSE:272")));
    }
}
