//! Key capture: GDK key events → Hyprland combo strings, and the empty
//! `hyprdeck_capture` submap that stops Hyprland from consuming bound combos
//! while the user records a new shortcut.

use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Result, bail};
use gtk::gdk;
use hyprdeck_core::hypr::ctl;

pub const SUBMAP: &str = "hyprdeck_capture";

/// Safety net inside the compositor: if hyprdeck never resets the submap
/// (crash, kill), a timer does after this long.
const SAFETY_TIMEOUT_MS: u32 = 30_000;

static CAPTURING: AtomicBool = AtomicBool::new(false);
static PANIC_HOOK: Once = Once::new();

/// Keysyms that are modifiers on their own (keep waiting for the real key).
pub fn is_modifier_sym(sym: &str) -> bool {
    matches!(
        sym,
        "Super_L"
            | "Super_R"
            | "Control_L"
            | "Control_R"
            | "Alt_L"
            | "Alt_R"
            | "Shift_L"
            | "Shift_R"
            | "Meta_L"
            | "Meta_R"
            | "Hyper_L"
            | "Hyper_R"
            | "ISO_Level3_Shift"
            | "ISO_Level5_Shift"
            | "ISO_Group_Shift"
            | "Caps_Lock"
            | "Num_Lock"
    )
}

/// Hyprland key name for an xkb keysym name: letters uppercase, everything
/// else (Return, space, Print, XF86AudioMute, F5, 1, …) as xkb names it.
pub fn hypr_key_name(sym: &str) -> String {
    let mut chars = sym.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) if c.is_ascii_alphabetic() => c.to_ascii_uppercase().to_string(),
        _ => sym.to_owned(),
    }
}

/// Hyprland modifier names held in `state`, in canonical order.
pub fn modifier_names(state: gdk::ModifierType) -> Vec<&'static str> {
    let mut out = Vec::with_capacity(4);
    if state.intersects(gdk::ModifierType::SUPER_MASK | gdk::ModifierType::HYPER_MASK) {
        out.push("SUPER");
    }
    if state.contains(gdk::ModifierType::CONTROL_MASK) {
        out.push("CTRL");
    }
    if state.contains(gdk::ModifierType::ALT_MASK) {
        out.push("ALT");
    }
    if state.contains(gdk::ModifierType::SHIFT_MASK) {
        out.push("SHIFT");
    }
    out
}

/// `SUPER + SHIFT + Q`.
pub fn combo(mods: &[&str], key: &str) -> String {
    let mut parts: Vec<&str> = mods.to_vec();
    if !key.is_empty() {
        parts.push(key);
    }
    parts.join(" + ")
}

/// Unshifted keysym name for a hardware keycode (so SHIFT+1 records `1`, not
/// `exclam`, matching how Hyprland resolves binds by keycode).
pub fn base_keysym(display: &gdk::Display, keycode: u32, fallback: gdk::Key) -> Option<String> {
    use gdk::prelude::DisplayExtManual;
    let key = display
        .translate_key(keycode, gdk::ModifierType::empty(), 0)
        .map(|(k, ..)| k)
        .unwrap_or_else(|| fallback.to_lower());
    key.name().map(|n| n.to_string())
}

/// Enter the capture submap (blocking). Defines it on first use per config
/// generation: a single non-consuming Escape bind that resets it, so even
/// without hyprdeck the user can get out, plus a timer as last resort.
pub fn begin() -> Result<()> {
    PANIC_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            if CAPTURING.load(Ordering::SeqCst) {
                let _ = end();
            }
            prev(info);
        }));
    });
    let lua = format!(
        r#"if not __hd_capture_defined then
  hl.define_submap("{SUBMAP}", function()
    hl.bind("Escape", hl.dsp.submap("reset"), {{ non_consuming = true }})
  end)
  __hd_capture_defined = true
end
hl.dispatch(hl.dsp.submap("{SUBMAP}"))
__hd_capture_gen = (__hd_capture_gen or 0) + 1
local gen = __hd_capture_gen
hl.timer(function()
  if __hd_capture_gen == gen and hl.get_current_submap() == "{SUBMAP}" then
    hl.dispatch(hl.dsp.submap("reset"))
  end
end, {{ timeout = {SAFETY_TIMEOUT_MS}, type = "oneshot" }})"#
    );
    CAPTURING.store(true, Ordering::SeqCst);
    if let Err(e) = ctl::eval(&lua) {
        let _ = end();
        return Err(e);
    }
    if current_submap()? != SUBMAP {
        let _ = end();
        bail!("Hyprland did not enter the capture submap");
    }
    Ok(())
}

/// Leave the capture submap if we are in it (blocking). Safe to call repeatedly.
pub fn end() -> Result<()> {
    CAPTURING.store(false, Ordering::SeqCst);
    ctl::eval(&format!(
        r#"if hl.get_current_submap() == "{SUBMAP}" then hl.dispatch(hl.dsp.submap("reset")) end"#
    ))
}

/// Name of the active submap (empty = default).
pub fn current_submap() -> Result<String> {
    // `hyprctl repl` prints "unknown request" for an empty reply, so wrap it.
    let out = ctl::repl(r#"return "[" .. hl.get_current_submap() .. "]""#)?;
    Ok(out
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned())
}

/// Recover from a capture left active by a previous hyprdeck process.
pub fn reset_leftover() -> Result<()> {
    if current_submap()? == SUBMAP {
        end()
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keysym_names_map_to_hyprland_keys() {
        assert_eq!(hypr_key_name("q"), "Q");
        assert_eq!(hypr_key_name("Q"), "Q");
        assert_eq!(hypr_key_name("Return"), "Return");
        assert_eq!(hypr_key_name("space"), "space");
        assert_eq!(hypr_key_name("Print"), "Print");
        assert_eq!(hypr_key_name("XF86AudioMute"), "XF86AudioMute");
        assert_eq!(hypr_key_name("1"), "1");
        assert_eq!(hypr_key_name("F12"), "F12");
        assert_eq!(hypr_key_name("period"), "period");
    }

    #[test]
    fn modifiers_in_canonical_order() {
        use gdk::ModifierType as M;
        assert_eq!(
            modifier_names(M::SHIFT_MASK | M::SUPER_MASK),
            ["SUPER", "SHIFT"]
        );
        assert_eq!(
            modifier_names(M::ALT_MASK | M::CONTROL_MASK | M::SHIFT_MASK),
            ["CTRL", "ALT", "SHIFT"]
        );
        assert!(modifier_names(M::BUTTON1_MASK | M::LOCK_MASK).is_empty());
        assert_eq!(combo(&["SUPER", "SHIFT"], "S"), "SUPER + SHIFT + S");
        assert_eq!(combo(&[], "Print"), "Print");
        assert_eq!(combo(&["SUPER"], ""), "SUPER");
    }

    #[test]
    fn modifier_keysyms_are_not_keys() {
        assert!(is_modifier_sym("Super_L"));
        assert!(is_modifier_sym("Shift_R"));
        assert!(!is_modifier_sym("Escape"));
        assert!(!is_modifier_sym("Tab"));
    }
}
