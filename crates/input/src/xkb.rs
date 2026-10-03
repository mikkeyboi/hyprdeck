//! XKB layout / variant / option catalogue from `evdev.lst`.

use std::sync::LazyLock;

const RULES: &str = "/usr/share/X11/xkb/rules/evdev.lst";

#[derive(Debug, Default)]
pub struct Catalog {
    /// (code, description), e.g. ("us", "English (US)").
    pub layouts: Vec<(String, String)>,
    /// (layout, variant, description).
    pub variants: Vec<(String, String, String)>,
    /// (option, description), e.g. ("caps:escape", "Make Caps Lock an additional Esc").
    pub options: Vec<(String, String)>,
    /// Option group descriptions, e.g. ("caps", "Caps Lock behavior").
    pub groups: Vec<(String, String)>,
}

pub static CATALOG: LazyLock<Catalog> =
    LazyLock::new(|| parse(&std::fs::read_to_string(RULES).unwrap_or_default()));

/// Common `kb_options` offered as switches.
pub const COMMON_OPTIONS: [&str; 16] = [
    "caps:escape",
    "caps:swapescape",
    "ctrl:nocaps",
    "ctrl:swapcaps",
    "caps:none",
    "compose:ralt",
    "compose:rwin",
    "compose:menu",
    "altwin:swap_alt_win",
    "grp:alt_shift_toggle",
    "grp:win_space_toggle",
    "grp:ctrl_shift_toggle",
    "grp:caps_toggle",
    "shift:both_capslock",
    "numpad:mac",
    "terminate:ctrl_alt_bksp",
];

fn parse(text: &str) -> Catalog {
    let mut c = Catalog::default();
    let mut section = "";
    for line in text.lines() {
        if let Some(s) = line.strip_prefix("! ") {
            section = match s.trim() {
                "layout" => "layout",
                "variant" => "variant",
                "option" => "option",
                _ => "",
            };
            continue;
        }
        let line = line.trim();
        let Some((name, desc)) = line.split_once(char::is_whitespace) else {
            continue;
        };
        let desc = desc.trim();
        match section {
            "layout" => c.layouts.push((name.to_owned(), desc.to_owned())),
            "variant" => {
                if let Some((layout, d)) = desc.split_once(": ") {
                    c.variants
                        .push((layout.to_owned(), name.to_owned(), d.to_owned()));
                }
            }
            "option" if name.contains(':') => c.options.push((name.to_owned(), desc.to_owned())),
            "option" => c.groups.push((name.to_owned(), desc.to_owned())),
            _ => {}
        }
    }
    c
}

impl Catalog {
    pub fn layout_desc(&self, code: &str) -> Option<&str> {
        self.layouts
            .iter()
            .find(|(c, _)| c == code)
            .map(|(_, d)| d.as_str())
    }

    pub fn variants_of<'a>(&'a self, layout: &'a str) -> impl Iterator<Item = (&'a str, &'a str)> {
        self.variants
            .iter()
            .filter(move |(l, ..)| l == layout)
            .map(|(_, v, d)| (v.as_str(), d.as_str()))
    }

    pub fn option_desc(&self, opt: &str) -> Option<&str> {
        self.options
            .iter()
            .find(|(o, _)| o == opt)
            .map(|(_, d)| d.as_str())
    }

    /// Group description for `caps:escape` → "Caps Lock behavior".
    pub fn group_desc(&self, opt: &str) -> Option<&str> {
        let g = opt.split_once(':').map_or(opt, |(g, _)| g);
        self.groups
            .iter()
            .find(|(n, _)| n == g)
            .map(|(_, d)| d.as_str())
    }
}

/// Split a comma list keeping empty slots (`"us,de"`, `",nodeadkeys"`).
pub fn split_list(s: &str) -> Vec<String> {
    if s.trim().is_empty() {
        Vec::new()
    } else {
        s.split(',').map(|p| p.trim().to_owned()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_sections() {
        let c = parse(
            "! model\n  pc105  Generic 105\n\n! layout\n  us              English (US)\n  de  German\n\n\
             ! variant\n  intl            us: English (US, intl., with dead keys)\n\n\
             ! option\n  caps                 Caps Lock behavior\n  caps:escape          Make Caps Lock an additional Esc\n",
        );
        assert_eq!(c.layouts.len(), 2);
        assert_eq!(c.layout_desc("us"), Some("English (US)"));
        assert_eq!(
            c.variants_of("us").collect::<Vec<_>>(),
            [("intl", "English (US, intl., with dead keys)")]
        );
        assert_eq!(
            c.option_desc("caps:escape"),
            Some("Make Caps Lock an additional Esc")
        );
        assert_eq!(c.group_desc("caps:escape"), Some("Caps Lock behavior"));
    }

    #[test]
    fn splits_lists() {
        assert_eq!(split_list("us, de"), ["us", "de"]);
        assert_eq!(split_list(",nodeadkeys"), ["", "nodeadkeys"]);
        assert!(split_list("").is_empty());
    }
}
