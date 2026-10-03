//! The display-rescue keyboard shortcut, stored as a hyprdeck-managed bind.

use anyhow::Result;
use hyprdeck_core::hypr::ctl::lua_str;
use hyprdeck_core::hypr::managed::{self, BindFlags, BindRule};
use hyprdeck_core::hypr::model::{self, Combo};

pub const RESCUE_CMD: &str = "hyprdeck display rescue";
pub const DEFAULT_KEYS: &str = "SUPER + CTRL + SHIFT + R";

fn is_rescue(b: &BindRule) -> bool {
    b.dispatcher == "exec_cmd" && b.args == lua_str(RESCUE_CMD)
}

/// Keys of the installed shortcut. Blocking.
pub fn current() -> Result<Option<String>> {
    Ok(managed::load()?
        .binds
        .into_iter()
        .find(is_rescue)
        .map(|b| b.keys))
}

/// Other binds (hand-written or managed) currently on `keys`. Blocking.
pub fn conflicts(keys: &str) -> Vec<String> {
    let combo = Combo::parse(keys);
    let m = model::load();
    m.effective_binds()
        .into_iter()
        .filter(|b| b.combo.matches(&combo) && b.exec.as_deref() != Some(RESCUE_CMD))
        .map(|b| {
            format!(
                "{} ({})",
                b.exec.as_deref().unwrap_or(&b.action_lua),
                b.source.display()
            )
        })
        .collect()
}

/// Install (or move) the shortcut and reload Hyprland; returns config errors. Blocking.
pub fn install(keys: &str) -> Result<Vec<String>> {
    let keys = Combo::parse(keys).0;
    managed::update(|m| {
        m.binds.retain(|b| !is_rescue(b));
        m.binds.push(BindRule {
            keys,
            dispatcher: "exec_cmd".into(),
            args: lua_str(RESCUE_CMD),
            // `locked`: also works while the lock screen is up, which it is after every wake.
            flags: BindFlags {
                locked: true,
                ..Default::default()
            },
            description: Some("Reset displays (hyprdeck)".into()),
        });
    })?;
    managed::apply(false)
}

/// Remove the shortcut and reload Hyprland. Blocking.
pub fn remove() -> Result<Vec<String>> {
    managed::update(|m| m.binds.retain(|b| !is_rescue(b)))?;
    managed::apply(false)
}

/// Whether `hyprdeck` resolves on `PATH` (the shortcut runs it by name).
pub fn on_path() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|p| std::env::split_paths(&p).any(|d| d.join("hyprdeck").is_file()))
}
