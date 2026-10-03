//! Effective keybinds (hand-written Lua config joined with hyprdeck's managed
//! overrides) and the operations that edit the managed side.

use std::collections::HashMap;

use anyhow::Result;
use gtk::gio;
use gtk::prelude::*;
use hyprdeck_core::hypr::managed::{self, BindFlags, BindRule, Managed};
use hyprdeck_core::hypr::model::{self, BindEntry, Combo, ConfigModel};

use crate::actions::{self, Category};

/// A hand-written bind shadowed by a managed bind or unbind.
#[derive(Debug, Clone)]
pub struct Shadowed {
    pub label: String,
    pub source: String,
}

#[derive(Debug, Clone)]
pub struct BindItem {
    /// Key string as written.
    pub keys: String,
    pub combo: Combo,
    /// Dispatcher path, `None` for a raw Lua function.
    pub dispatcher: Option<String>,
    /// Lua argument text of the dispatcher call.
    pub args: String,
    pub flags: BindFlags,
    pub description: Option<String>,
    pub submap: String,
    /// `keybinds.lua:52`, or `hyprdeck` for managed binds.
    pub source: String,
    pub managed: bool,
    /// Hand-written binds on the same combo that this managed bind replaces.
    pub overrides: Vec<Shadowed>,
    pub label: String,
    pub category: Category,
    /// Technical detail: the command line or the dispatcher call.
    pub detail: String,
}

impl BindItem {
    /// A hand-written bind exists on this combo (itself or one it overrides).
    pub fn has_handwritten(&self) -> bool {
        !self.managed || !self.overrides.is_empty()
    }

    pub fn flag_labels(&self) -> Vec<&'static str> {
        let f = &self.flags;
        [
            (f.locked, "locked"),
            (f.repeating, "repeat"),
            (f.release, "on release"),
            (f.non_consuming, "non-consuming"),
            (f.transparent, "transparent"),
            (f.ignore_mods, "ignore mods"),
            (f.long_press, "long press"),
            (f.click, "click"),
            (f.drag, "drag"),
        ]
        .into_iter()
        .filter_map(|(on, l)| on.then_some(l))
        .collect()
    }
}

/// A combo disabled through `managed.unbinds`.
#[derive(Debug, Clone)]
pub struct Disabled {
    pub keys: String,
    pub combo: Combo,
    /// The hand-written binds it switched off.
    pub was: Vec<Shadowed>,
}

#[derive(Debug, Clone, Default)]
pub struct Snapshot {
    pub binds: Vec<BindItem>,
    pub disabled: Vec<Disabled>,
    /// Lua evaluation errors of the user's config.
    pub errors: Vec<String>,
}

/// Desktop id and executable → application display name (blocking).
///
/// Several entries can share an executable (`kitty.desktop` and the hidden
/// `kitty-open.desktop` "kitty URL Launcher"); prefer the one whose id matches
/// the executable, then visible entries.
pub fn app_names() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let mut by_exe: HashMap<String, (u8, String)> = HashMap::new();
    for app in gio::AppInfo::all() {
        let name = app.display_name().to_string();
        let id = app.id().map(|i| i.to_string());
        if let Some(exe) = app.executable().file_name() {
            let exe = exe.to_string_lossy().into_owned();
            let stem_matches = id
                .as_deref()
                .and_then(|i| i.strip_suffix(".desktop"))
                .is_some_and(|s| s == exe);
            let rank = u8::from(stem_matches) * 2 + u8::from(app.should_show());
            match by_exe.get(&exe) {
                Some((best, _)) if *best >= rank => {}
                _ => {
                    by_exe.insert(exe, (rank, name.clone()));
                }
            }
        }
        if let Some(id) = id {
            map.insert(id, name);
        }
    }
    for (exe, (_, name)) in by_exe {
        map.entry(exe).or_insert(name);
    }
    map
}

fn is_managed(b: &BindEntry) -> bool {
    b.source
        .file
        .file_name()
        .is_some_and(|f| f == "hyprdeck.lua")
}

pub fn parse_flags(opts_lua: &str) -> BindFlags {
    let a = actions::parse_args(opts_lua);
    BindFlags {
        repeating: a.flag("repeating"),
        locked: a.flag("locked"),
        release: a.flag("release"),
        non_consuming: a.flag("non_consuming"),
        transparent: a.flag("transparent"),
        ignore_mods: a.flag("ignore_mods"),
        long_press: a.flag("long_press"),
        click: a.flag("click"),
        drag: a.flag("drag"),
    }
}

/// Hand-written binds still active when hyprdeck's file is ignored.
fn handwritten(m: &ConfigModel) -> Vec<&BindEntry> {
    m.binds
        .iter()
        .filter(|b| !is_managed(b))
        .filter(|b| {
            !m.unbinds.iter().any(|u| {
                u.seq > b.seq
                    && u.combo.matches(&b.combo)
                    && u.source
                        .file
                        .file_name()
                        .is_none_or(|f| f != "hyprdeck.lua")
            })
        })
        .collect()
}

pub fn build(m: &ConfigModel, unbinds: &[String], names: &HashMap<String, String>) -> Snapshot {
    let lookup = |p: &str| names.get(p).cloned();
    let describe = |b: &BindEntry| {
        let args = b
            .dispatcher
            .as_deref()
            .map_or("", |d| actions::dsp_args(d, &b.action_lua));
        let (label, cat) = actions::describe(b.dispatcher.as_deref(), args, &lookup);
        (args.to_owned(), label, cat)
    };
    let hand = handwritten(m);
    let shadowed = |combo: &Combo, before: u64| -> Vec<Shadowed> {
        hand.iter()
            .filter(|h| h.seq < before && h.combo.matches(combo))
            .map(|h| Shadowed {
                label: describe(h).1,
                source: h.source.display(),
            })
            .collect()
    };
    let binds = m
        .effective_binds()
        .into_iter()
        .map(|b| {
            let (args, label, category) = describe(b);
            let managed = is_managed(b);
            BindItem {
                keys: b.keys.clone(),
                combo: b.combo.clone(),
                dispatcher: b.dispatcher.clone(),
                args,
                flags: parse_flags(&b.opts_lua),
                description: b.description.clone(),
                submap: b.submap.clone(),
                source: if managed {
                    "hyprdeck".into()
                } else {
                    b.source.display()
                },
                managed,
                overrides: if managed {
                    shadowed(&b.combo, b.seq)
                } else {
                    Vec::new()
                },
                label,
                category,
                detail: b.exec.clone().unwrap_or_else(|| b.action_lua.clone()),
            }
        })
        .collect();
    let disabled = unbinds
        .iter()
        .map(|keys| {
            let combo = Combo::parse(keys);
            let was = shadowed(&combo, u64::MAX);
            Disabled {
                keys: keys.clone(),
                combo,
                was,
            }
        })
        .collect();
    Snapshot {
        binds,
        disabled,
        errors: m.errors.clone(),
    }
}

/// Load the config model, managed state and app names (blocking).
pub fn snapshot() -> Result<Snapshot> {
    let managed = managed::load()?;
    Ok(build(&model::load(), &managed.unbinds, &app_names()))
}

fn same(keys: &str, combo: &Combo) -> bool {
    Combo::parse(keys).matches(combo)
}

/// The bind being replaced by an edit.
#[derive(Debug, Clone)]
pub struct Original {
    pub combo: Combo,
    pub has_handwritten: bool,
}

/// Put `rule` into the managed state, replacing `original` (if editing).
/// Moving a bind that has a hand-written counterpart to a new combo disables
/// the old combo, so the action really moves.
fn put_bind(m: &mut Managed, original: Option<&Original>, mut rule: BindRule) {
    let combo = Combo::parse(&rule.keys);
    rule.keys = combo.0.clone();
    if let Some(o) = original {
        m.binds.retain(|b| !same(&b.keys, &o.combo));
        if !o.combo.matches(&combo)
            && o.has_handwritten
            && !m.unbinds.iter().any(|u| same(u, &o.combo))
        {
            m.unbinds.push(o.combo.0.clone());
        }
    }
    m.binds.retain(|b| !same(&b.keys, &combo));
    m.unbinds.retain(|u| !same(u, &combo));
    m.binds.push(rule);
}

/// Drop any managed bind on the combo and unbind the hand-written one.
fn put_unbind(m: &mut Managed, keys: &str) {
    let combo = Combo::parse(keys);
    m.binds.retain(|b| !same(&b.keys, &combo));
    if !m.unbinds.iter().any(|u| same(u, &combo)) {
        m.unbinds.push(combo.0);
    }
}

/// Create or update a managed bind (blocking). Returns config errors.
pub fn save(original: Option<Original>, rule: BindRule) -> Result<Vec<String>> {
    managed::update(|m| put_bind(m, original.as_ref(), rule))?;
    managed::apply(false)
}

/// Switch a combo off (blocking).
pub fn disable(keys: &str) -> Result<Vec<String>> {
    managed::update(|m| put_unbind(m, keys))?;
    managed::apply(false)
}

/// Remove hyprdeck's bind on a combo (the hand-written one, if any, returns).
pub fn remove_managed(keys: &str) -> Result<Vec<String>> {
    let combo = Combo::parse(keys);
    managed::update(|m| m.binds.retain(|b| !same(&b.keys, &combo)))?;
    managed::apply(false)
}

/// Re-enable a disabled combo.
pub fn restore(keys: &str) -> Result<Vec<String>> {
    let combo = Combo::parse(keys);
    managed::update(|m| m.unbinds.retain(|u| !same(u, &combo)))?;
    managed::apply(false)
}

/// Validate a dispatcher call by constructing it in the running compositor (blocking).
pub fn validate(dispatcher: &str, args: &str) -> Result<()> {
    if dispatcher.is_empty()
        || !dispatcher
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        anyhow::bail!("“{dispatcher}” is not a dispatcher path (e.g. window.move)");
    }
    let lua = format!("return type(hl.dsp.{dispatcher}({args}))");
    let out = hyprdeck_core::hypr::ctl::repl(&lua)?;
    if out.trim() != "userdata" {
        anyhow::bail!(
            "hl.dsp.{dispatcher}({args}) did not produce a dispatcher (got {})",
            out.trim()
        );
    }
    Ok(())
}

/// `hyprdeck input binds`.
pub fn print_binds() -> Result<()> {
    let snap = snapshot()?;
    for e in &snap.errors {
        eprintln!("config error: {e}");
    }
    let width = snap
        .binds
        .iter()
        .map(|b| b.combo.0.chars().count())
        .max()
        .unwrap_or(0);
    let mut binds: Vec<&BindItem> = snap.binds.iter().collect();
    binds.sort_by_key(|b| b.category);
    let mut last = None;
    for b in binds {
        if last != Some(b.category) {
            println!("\n{}", b.category.title());
            last = Some(b.category);
        }
        let flags = b.flag_labels();
        let flags = if flags.is_empty() {
            String::new()
        } else {
            format!(" [{}]", flags.join(", "))
        };
        let submap = if b.submap.is_empty() {
            String::new()
        } else {
            format!(" (submap {})", b.submap)
        };
        println!(
            "  {:<width$}  {}{flags}{submap}  — {}",
            b.combo.0, b.label, b.source
        );
    }
    if !snap.disabled.is_empty() {
        println!("\nDisabled by hyprdeck");
        for d in &snap.disabled {
            let was: Vec<String> = d
                .was
                .iter()
                .map(|w| format!("{} ({})", w.label, w.source))
                .collect();
            println!(
                "  {:<width$}  {}",
                d.combo.0,
                if was.is_empty() {
                    "-".into()
                } else {
                    was.join("; ")
                }
            );
        }
    }
    println!("\n{} active binds", snap.binds.len());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bind_flags() {
        let f = parse_flags("{ locked = true, repeating = true, description = \"x\" }");
        assert!(f.locked && f.repeating && !f.release);
        assert_eq!(parse_flags(""), BindFlags::default());
    }

    #[test]
    fn joins_handwritten_and_managed() {
        let dir = std::env::temp_dir().join(format!("hd-input-binds-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("hyprland.lua"),
            "require('keybinds')\nrequire('hyprdeck')\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("keybinds.lua"),
            "hl.bind('SUPER + W', hl.dsp.exec_cmd('uwsm app -- firefox'))\n\
             hl.bind('SUPER + Q', hl.dsp.window.close())\n\
             hl.bind('SUPER + L', hl.dsp.exec_cmd('playerctl play-pause'), { locked = true })\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("hyprdeck.lua"),
            "hl.unbind(\"SUPER + Q\")\nhl.unbind(\"SUPER + W\")\nhl.bind(\"SUPER + W\", hl.dsp.exec_cmd(\"gtk-launch org.example.Browser.desktop\"))\n",
        )
        .unwrap();
        let m = model::load_from(&dir.join("hyprland.lua"));
        std::fs::remove_dir_all(&dir).unwrap();
        assert!(m.errors.is_empty(), "{:?}", m.errors);
        let names = HashMap::from([(
            "org.example.Browser.desktop".to_owned(),
            "Example Browser".to_owned(),
        )]);
        let s = build(&m, &["SUPER + Q".to_owned()], &names);
        assert_eq!(s.binds.len(), 2);
        let w = s.binds.iter().find(|b| b.combo.0 == "SUPER + W").unwrap();
        assert!(w.managed);
        assert_eq!(w.source, "hyprdeck");
        assert_eq!(w.label, "Launch Example Browser");
        assert_eq!(w.overrides.len(), 1);
        assert!(
            w.overrides[0].source.ends_with("keybinds.lua:1"),
            "{}",
            w.overrides[0].source
        );
        let l = s.binds.iter().find(|b| b.combo.0 == "SUPER + L").unwrap();
        assert!(l.flags.locked && !l.managed);
        assert!(l.source.ends_with("keybinds.lua:3"), "{}", l.source);
        assert_eq!(l.category, Category::Media);
        assert_eq!(s.disabled.len(), 1);
        assert_eq!(s.disabled[0].was[0].label, "Close window");
    }

    #[test]
    fn edits_disables_and_moves_binds() {
        let mut m = Managed::default();
        let orig = |keys: &str, hand: bool| Original {
            combo: Combo::parse(keys),
            has_handwritten: hand,
        };
        // Overriding a hand-written bind on the same combo: just a managed rule.
        put_bind(
            &mut m,
            Some(&orig("SUPER + W", true)),
            BindRule::exec("super + w", "firefox"),
        );
        assert_eq!(m.binds.len(), 1);
        assert_eq!(m.binds[0].keys, "SUPER + W");
        assert!(m.unbinds.is_empty());
        // Moving it to a new combo disables the hand-written bind on the old one.
        put_bind(
            &mut m,
            Some(&orig("SUPER + W", true)),
            BindRule::exec("SHIFT + SUPER + O", "firefox"),
        );
        assert_eq!(
            m.binds.iter().map(|b| b.keys.as_str()).collect::<Vec<_>>(),
            ["SUPER + SHIFT + O"]
        );
        assert_eq!(m.unbinds, ["SUPER + W"]);
        // Binding a disabled combo re-enables it with the new action.
        put_bind(&mut m, None, BindRule::exec("SUPER + W", "kitty"));
        assert!(m.unbinds.is_empty());
        assert_eq!(m.binds.len(), 2);
        // Moving a purely managed bind leaves no unbind behind.
        put_bind(
            &mut m,
            Some(&orig("SUPER + SHIFT + O", false)),
            BindRule::exec("SUPER + O", "firefox"),
        );
        assert!(m.unbinds.is_empty());
        // Disabling drops the managed rule and unbinds once.
        put_unbind(&mut m, "super + w");
        put_unbind(&mut m, "SUPER + W");
        assert_eq!(m.unbinds, ["SUPER + W"]);
        assert!(m.binds.iter().all(|b| b.keys != "SUPER + W"));
    }
}
