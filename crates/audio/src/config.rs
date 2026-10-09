//! Persistent settings (`~/.config/hyprdeck/audio.toml`): the simultaneous-output
//! group (desired on/off state, members, sink to restore) and per-app routing rules.

use std::hash::{BuildHasher, Hasher};

use anyhow::Result;
use serde::{Deserialize, Deserializer, Serialize};

use crate::pw::{self, Stream};

const STORE_NAME: &str = "audio";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MatchKind {
    #[default]
    Binary,
    App,
    Media,
}

impl MatchKind {
    pub const ALL: [MatchKind; 3] = [MatchKind::Binary, MatchKind::App, MatchKind::Media];

    /// Unknown values fall back to `binary` (a hand-edited file never breaks loading).
    pub fn parse(s: &str) -> Self {
        match s {
            "app" => MatchKind::App,
            "media" => MatchKind::Media,
            _ => MatchKind::Binary,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            MatchKind::Binary => "binary",
            MatchKind::App => "app",
            MatchKind::Media => "media",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            MatchKind::Binary => "Executable name",
            MatchKind::App => "Application name",
            MatchKind::Media => "Stream title",
        }
    }
}

impl<'de> Deserialize<'de> for MatchKind {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(MatchKind::parse(&String::deserialize(d)?))
    }
}

/// Send matching application streams somewhere other than the default output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Route {
    pub id: String,
    pub label: String,
    pub match_kind: MatchKind,
    pub match_value: String,
    pub sinks: Vec<String>,
    pub enabled: bool,
    /// Stream volume (percent) applied after moving a stream onto this route.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub volume: Option<u32>,
}

impl Default for Route {
    fn default() -> Self {
        Route {
            id: new_id(),
            label: String::new(),
            match_kind: MatchKind::Binary,
            match_value: String::new(),
            sinks: Vec::new(),
            enabled: true,
            volume: None,
        }
    }
}

impl Route {
    /// Enforce invariants every construction path relies on: targets are real
    /// devices (a combine sink nested inside another never recovers), unique and non-empty.
    pub fn normalize(&mut self) {
        let mut seen: Vec<String> = Vec::with_capacity(self.sinks.len());
        for s in std::mem::take(&mut self.sinks) {
            if !s.is_empty() && !pw::is_virtual_name(&s) && !seen.contains(&s) {
                seen.push(s);
            }
        }
        self.sinks = seen;
        if self.id.is_empty() {
            self.id = new_id();
        }
        self.volume = self.volume.map(|v| v.min(pw::MAX_VOLUME));
    }

    /// Where matching streams must end up: the device itself, or a dedicated combine sink.
    pub fn sink_name(&self) -> String {
        if self.sinks.len() == 1 {
            return self.sinks[0].clone();
        }
        pw::route_sink_name(&format!("{}_{}", slugify(self.slug_source()), self.id))
    }

    fn slug_source(&self) -> &str {
        if self.label.is_empty() {
            &self.match_value
        } else {
            &self.label
        }
    }

    pub fn needs_combine(&self) -> bool {
        self.sinks.len() > 1
    }

    pub fn description(&self) -> String {
        let slug = slugify(self.slug_source()).replace('_', "-");
        format!("Route-{}", if slug.is_empty() { "app" } else { &slug })
    }

    pub fn valid(&self) -> bool {
        !self.match_value.trim().is_empty() && !self.sinks.is_empty()
    }

    pub fn active(&self) -> bool {
        self.enabled && self.valid()
    }

    /// Case-insensitive substring test of the chosen stream property (ignores `enabled`).
    pub fn matches_ignoring_state(&self, stream: &Stream) -> bool {
        if !self.valid() {
            return false;
        }
        let hay = match self.match_kind {
            MatchKind::Binary => &stream.binary,
            MatchKind::App => &stream.app_name,
            MatchKind::Media => &stream.media_name,
        };
        hay.to_lowercase()
            .contains(&self.match_value.trim().to_lowercase())
    }

    pub fn matches(&self, stream: &Stream) -> bool {
        self.enabled && self.matches_ignoring_state(stream)
    }

    pub fn display(&self) -> &str {
        [self.label.trim(), self.match_value.trim()]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("Untitled route")
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Desired state of the simultaneous output; restored at login and after PipeWire restarts.
    pub enabled: bool,
    /// Group members (kept when switched off, so the next "on" uses the same group).
    pub selected: Vec<String>,
    /// Output to fall back to when switching off.
    pub restore_sink: String,
    /// Volume ceiling (100–150 %) for every slider and preset.
    pub max_volume: u32,
    pub routes: Vec<Route>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            enabled: false,
            selected: Vec::new(),
            restore_sink: String::new(),
            max_volume: pw::MAX_VOLUME,
            routes: Vec::new(),
        }
    }
}

impl Config {
    pub fn normalize(&mut self) {
        self.max_volume = self.max_volume.clamp(100, pw::MAX_VOLUME);
        self.selected = clean_members(&self.selected);
        for r in &mut self.routes {
            r.normalize();
        }
    }

    pub fn route(&self, id: &str) -> Option<&Route> {
        self.routes.iter().find(|r| r.id == id)
    }

    pub fn active_routes(&self) -> impl Iterator<Item = &Route> {
        self.routes.iter().filter(|r| r.active())
    }

    /// First active route matching `stream` (config order wins).
    pub fn route_for(&self, stream: &Stream) -> Option<&Route> {
        self.routes.iter().find(|r| r.matches(stream))
    }
}

/// De-duplicate and drop virtual/empty names, keeping order.
pub fn clean_members<S: AsRef<str>>(names: &[S]) -> Vec<String> {
    let mut out: Vec<String> = Vec::with_capacity(names.len());
    for n in names {
        let n = n.as_ref();
        if !n.is_empty() && !pw::is_virtual_name(n) && !out.iter().any(|o| o == n) {
            out.push(n.to_owned());
        }
    }
    out
}

/// PipeWire node names accept only `[A-Za-z0-9_.-]`.
pub fn slugify(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') {
            out.push('_');
        }
    }
    let trimmed = out.trim_matches('_');
    let cut: String = trimmed.chars().take(24).collect();
    if cut.is_empty() {
        "route".to_owned()
    } else {
        cut
    }
}

/// 8 random hex characters.
pub fn new_id() -> String {
    let mut h = std::hash::RandomState::new().build_hasher();
    h.write_u128(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos()),
    );
    format!("{:08x}", h.finish() as u32)
}

/// Rule dialog validation: turn form input into a route, or explain what is missing.
/// Editing keeps the original id and enabled state.
pub fn rule_from_form(
    existing: Option<&Route>,
    label: &str,
    kind: MatchKind,
    value: &str,
    sinks: Vec<String>,
    volume: Option<u32>,
) -> Result<Route, &'static str> {
    if value.trim().is_empty() {
        return Err("Enter the text a stream must contain.");
    }
    let mut route = Route {
        id: existing.map_or_else(new_id, |r| r.id.clone()),
        label: label.trim().to_owned(),
        match_kind: kind,
        match_value: value.trim().to_owned(),
        sinks,
        enabled: existing.is_none_or(|r| r.enabled),
        volume,
    };
    route.normalize();
    if route.sinks.is_empty() {
        return Err("Choose at least one output to send this app to.");
    }
    Ok(route)
}

// --- persistence -----------------------------------------------------------

/// Load settings (defaults when `audio.toml` does not exist yet).
/// Blocking (small file IO); call off the GTK main thread.
pub fn load() -> Result<Config> {
    let mut cfg: Config = hyprdeck_core::store::load(STORE_NAME)?;
    cfg.normalize();
    Ok(cfg)
}

pub fn save(cfg: &Config) -> Result<()> {
    hyprdeck_core::store::save(STORE_NAME, cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pw::tests::streams_json;

    fn make() -> Route {
        Route {
            id: "ab12".into(),
            label: "Chat".into(),
            match_kind: MatchKind::Binary,
            match_value: "chat".into(),
            sinks: vec!["a".into(), "b".into()],
            ..Route::default()
        }
    }

    fn chat() -> Stream {
        pw::parse_streams(&streams_json()).0.remove(1)
    }

    #[test]
    fn matches_are_case_insensitive_substrings_of_the_chosen_property() {
        let s = chat();
        assert!(make().matches(&s));
        assert!(
            Route {
                match_kind: MatchKind::App,
                match_value: "example c".into(),
                ..make()
            }
            .matches(&s)
        );
        assert!(
            Route {
                match_kind: MatchKind::Media,
                match_value: "voice".into(),
                ..make()
            }
            .matches(&s)
        );
        assert!(
            !Route {
                match_kind: MatchKind::Media,
                match_value: "playback".into(),
                ..make()
            }
            .matches(&s)
        );
    }

    #[test]
    fn disabled_or_incomplete_rules_never_match() {
        let s = chat();
        let disabled = Route {
            enabled: false,
            ..make()
        };
        assert!(!disabled.matches(&s));
        assert!(
            disabled.matches_ignoring_state(&s),
            "release must still find a disabled rule's streams"
        );
        assert!(
            !Route {
                sinks: vec![],
                ..make()
            }
            .matches(&s)
        );
        assert!(
            !Route {
                match_value: "  ".into(),
                ..make()
            }
            .matches(&s)
        );
    }

    #[test]
    fn single_target_needs_no_helper_sink() {
        let r = Route {
            sinks: vec!["alsa_output.usb-Generic_USB_Audio-01.analog-stereo".into()],
            ..make()
        };
        assert!(!r.needs_combine());
        assert_eq!(
            r.sink_name(),
            "alsa_output.usb-Generic_USB_Audio-01.analog-stereo"
        );
    }

    #[test]
    fn multi_target_gets_its_own_combine_sink_with_a_legal_name() {
        let r = Route {
            label: "Chat & Friends".into(),
            ..make()
        };
        assert!(r.needs_combine());
        assert_eq!(r.sink_name(), "simultaneous_route_chat_friends_ab12");
        assert!(
            r.sink_name()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
        );
        assert_eq!(r.description(), "Route-chat-friends");
    }

    #[test]
    fn virtual_and_duplicate_targets_are_stripped() {
        let mut r = Route {
            sinks: vec![
                pw::PRIMARY_SINK.into(),
                "a".into(),
                pw::route_sink_name("other_cd34"),
                "b".into(),
                "a".into(),
                "".into(),
            ],
            ..make()
        };
        r.normalize();
        assert_eq!(r.sinks, ["a", "b"]);
        let mut only_virtual = Route {
            sinks: vec![pw::PRIMARY_SINK.into(), pw::route_sink_name("x_1")],
            ..make()
        };
        only_virtual.normalize();
        assert!(only_virtual.sinks.is_empty());
        assert!(!only_virtual.valid());
        assert!(!only_virtual.matches(&chat()));
    }

    #[test]
    fn route_fanout_streams_are_recognised_as_ours() {
        let r = make();
        assert!(pw::is_virtual_name(&r.sink_name()));
        assert_eq!(
            pw::fanout_media_name(&r.sink_name()),
            format!("{} output", r.sink_name())
        );
    }

    #[test]
    fn slugify_produces_pipewire_safe_fragments() {
        assert_eq!(slugify("Chat & Friends!"), "chat_friends");
        assert_eq!(slugify("***"), "route");
        assert!(slugify(&"x".repeat(100)).len() <= 24);
    }

    #[test]
    fn rule_form_validation() {
        let sinks = || vec!["bt_head".to_owned(), "spdif".to_owned()];
        assert_eq!(
            rule_from_form(None, "", MatchKind::App, "  ", sinks(), None),
            Err("Enter the text a stream must contain.")
        );
        assert!(
            rule_from_form(None, "", MatchKind::App, "x", vec![], None)
                .unwrap_err()
                .contains("output")
        );
        assert!(
            rule_from_form(
                None,
                "",
                MatchKind::App,
                "x",
                vec![pw::PRIMARY_SINK.into()],
                None
            )
            .unwrap_err()
            .contains("output")
        );

        let r = rule_from_form(None, " Chat ", MatchKind::App, " chat ", sinks(), None).unwrap();
        assert_eq!(
            (r.label.as_str(), r.match_kind, r.match_value.as_str()),
            ("Chat", MatchKind::App, "chat")
        );
        assert_eq!(r.sinks, sinks());
        assert!(r.enabled && r.needs_combine());

        let single = rule_from_form(
            None,
            "",
            MatchKind::Binary,
            "x",
            vec!["usb_dac".into()],
            None,
        )
        .unwrap();
        assert_eq!(single.sink_name(), "usb_dac");

        let old = Route {
            enabled: false,
            volume: Some(70),
            ..make()
        };
        let edited = rule_from_form(
            Some(&old),
            "New",
            MatchKind::Media,
            "voice",
            sinks(),
            Some(70),
        )
        .unwrap();
        assert_eq!(edited.id, old.id);
        assert!(!edited.enabled);
        assert_eq!(edited.volume, Some(70));
    }

    #[test]
    fn config_survives_a_toml_round_trip() {
        let mut original = Config {
            enabled: true,
            selected: vec!["a".into(), "b".into()],
            restore_sink: "usb_dac".into(),
            max_volume: 120,
            routes: vec![
                Route {
                    volume: Some(80),
                    ..make()
                },
                Route {
                    volume: None,
                    id: "cd34".into(),
                    ..make()
                },
            ],
        };
        original.normalize();
        let text = toml::to_string_pretty(&original).unwrap();
        let mut loaded: Config = toml::from_str(&text).unwrap();
        loaded.normalize();
        assert_eq!(loaded, original);
    }

    #[test]
    fn bad_values_are_coerced_not_propagated() {
        let mut c: Config = toml::from_str("max_volume = 9000\nselected = [\"simultaneous_output\", \"a\", \"a\"]\n[[routes]]\nmatch_kind = \"nonsense\"\nmatch_value = \"x\"\nsinks = [\"simultaneous_output\", \"real\"]\n").unwrap();
        c.normalize();
        assert_eq!(c.max_volume, 150);
        assert_eq!(c.selected, ["a"]);
        assert_eq!(c.routes[0].match_kind, MatchKind::Binary);
        assert_eq!(c.routes[0].sinks, ["real"]);
        assert_eq!(c.routes[0].id.len(), 8);
    }

    #[test]
    fn existing_audio_toml_layout_still_loads() {
        let text = r#"enabled = false
selected = ["bluez_output.AA_BB_CC_DD_EE_FF.1", "alsa_output.usb-Generic_USB_Audio-01.analog-stereo"]
restore_sink = "alsa_output.usb-Generic_USB_Audio-01.analog-stereo"
max_volume = 150

[[routes]]
id = "e8ac9f7a"
label = "Browser"
match_kind = "binary"
match_value = "firefox"
sinks = ["alsa_output.usb-Generic_USB_Audio-01.analog-stereo"]
enabled = true

[[routes]]
id = "b020b5f9"
label = "Music"
match_kind = "app"
match_value = "Music Player"
sinks = ["alsa_output.usb-Generic_USB_Audio-01.analog-stereo"]
enabled = true
"#;
        let mut c: Config = toml::from_str(text).unwrap();
        c.normalize();
        assert_eq!(c.selected.len(), 2);
        assert_eq!(c.routes.len(), 2);
        assert_eq!(
            (c.routes[0].id.as_str(), c.routes[0].match_kind),
            ("e8ac9f7a", MatchKind::Binary)
        );
        assert_eq!(
            (c.routes[1].display(), c.routes[1].match_kind),
            ("Music", MatchKind::App)
        );
        assert!(c.routes.iter().all(|r| r.active() && r.volume.is_none()));
    }
}
