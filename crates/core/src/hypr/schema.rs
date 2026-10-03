//! Schema of Hyprland config options, read from the Lua API stubs shipped with
//! Hyprland (`/usr/share/hypr/stubs/hl.meta.lua`, class `HL.ConfigValueTypes`).

use std::collections::BTreeMap;
use std::sync::LazyLock;

const STUBS: &str = "/usr/share/hypr/stubs/hl.meta.lua";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OptKind {
    Bool,
    Int,
    Float,
    Str,
    /// `integer|string`
    IntOrStr,
    /// `string|HL.Gradient`
    Gradient,
    /// `integer|HL.CssGap`
    Gap,
    /// `HL.Vec2Like`
    Vec2,
}

/// Dotted option key (e.g. `input.follow_mouse`) → value kind.
pub static OPTIONS: LazyLock<BTreeMap<String, OptKind>> = LazyLock::new(|| {
    let text = std::fs::read_to_string(STUBS).unwrap_or_default();
    parse(&text)
});

fn parse(text: &str) -> BTreeMap<String, OptKind> {
    let mut map = BTreeMap::new();
    let mut in_types = false;
    for line in text.lines() {
        if line.starts_with("---@class ") {
            in_types = line.trim() == "---@class HL.ConfigValueTypes";
            continue;
        }
        if !in_types {
            continue;
        }
        let Some(rest) = line.strip_prefix("---@field ['") else {
            continue;
        };
        let Some((key, ty)) = rest.split_once("'] ") else {
            continue;
        };
        let kind = match ty.trim() {
            "boolean" => OptKind::Bool,
            "integer|boolean" => OptKind::Int,
            "number|boolean" => OptKind::Float,
            "string" => OptKind::Str,
            "integer|string" => OptKind::IntOrStr,
            "string|HL.Gradient" => OptKind::Gradient,
            "integer|HL.CssGap" => OptKind::Gap,
            "HL.Vec2Like" => OptKind::Vec2,
            _ => continue,
        };
        map.insert(key.to_owned(), kind);
    }
    map
}

pub fn kind(key: &str) -> Option<OptKind> {
    OPTIONS.get(key).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_only_value_types_class() {
        let text = "---@class HL.Other\n---@field ['x.y'] boolean\n---@class HL.ConfigValueTypes\n\
                    ---@field ['input.follow_mouse'] integer|boolean\n---@field ['general.col.active_border'] string|HL.Gradient\n\
                    local t = {}\n---@class HL.Next\n---@field ['z.z'] string\n";
        let m = parse(text);
        assert_eq!(m.len(), 2);
        assert_eq!(m["input.follow_mouse"], OptKind::Int);
        assert_eq!(m["general.col.active_border"], OptKind::Gradient);
    }
}
