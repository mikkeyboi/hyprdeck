//! Version 1 subprocess protocol. Unknown object fields are forward-compatible;
//! unknown API versions and control kinds are not.
use std::collections::HashSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const API_VERSION: u32 = 1;
pub const MAX_DOCUMENT: usize = 1024 * 1024;
/// Optional renderer features this host supports, advertised in every request.
/// Plugins may use them to omit fallback-only output; older hosts send none.
pub const HOST_FEATURES: &[&str] = &["keyboard_visualization", "keyboard_animation"];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub api_version: u32,
    pub id: String,
    pub name: String,
    pub description: String,
    pub version: String,
    pub executable: String,
    pub update_repo: String,
    pub asset: String,
}

pub fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id.as_bytes()[0].is_ascii_lowercase()
        && id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"_-".contains(&b))
}

pub fn filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 200
        && name.as_bytes()[0].is_ascii_alphanumeric()
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

pub fn validate_repo(repo: &str) -> Result<()> {
    let parts: Vec<_> = repo.split('/').collect();
    ensure!(
        parts.len() == 2 && parts.iter().all(|p| filename(p)),
        "expected a GitHub owner/repo, not a URL or path"
    );
    Ok(())
}

impl Manifest {
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.api_version == API_VERSION,
            "unsupported plugin API {} (host supports {API_VERSION})",
            self.api_version
        );
        ensure!(
            valid_id(&self.id),
            "invalid plugin id {:?}; use a lowercase letter followed by lowercase letters, digits, _ or -",
            self.id
        );
        text(&self.name, "manifest name", true)?;
        text(&self.description, "manifest description", false)?;
        let version = semver::Version::parse(&self.version)?;
        ensure!(
            version.pre.is_empty() && version.build.is_empty(),
            "plugin version must be stable X.Y.Z"
        );
        ensure!(
            filename(&self.executable) && self.executable != "plugin.json",
            "executable must be a single safe filename other than plugin.json"
        );
        validate_repo(&self.update_repo)?;
        ensure!(
            filename(&self.asset),
            "asset must be a single safe filename"
        );
        let expected = format!("{}-linux-{}", self.executable, std::env::consts::ARCH);
        ensure!(
            self.asset == expected,
            "incompatible asset {:?}; this host requires {expected}",
            self.asset
        );
        Ok(())
    }

    pub fn validate_update(&self, installed: &Self) -> Result<()> {
        self.validate()?;
        ensure!(
            self.id == installed.id,
            "release identity mismatch: {} != {}",
            self.id,
            installed.id
        );
        ensure!(
            self.update_repo == installed.update_repo,
            "release repository changed; refusing to transfer trust"
        );
        ensure!(
            self.executable == installed.executable && self.asset == installed.asset,
            "release executable/asset identity changed"
        );
        ensure!(
            semver::Version::parse(&self.version)? > semver::Version::parse(&installed.version)?,
            "release {} is not newer than {}",
            self.version,
            installed.version
        );
        Ok(())
    }
}

#[derive(Debug, Serialize)]
pub struct Request<'a> {
    pub api_version: u32,
    pub method: &'a str,
    pub action: Option<&'a str>,
    pub args: Map<String, Value>,
    pub host_features: &'a [&'a str],
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Response {
    pub api_version: u32,
    #[serde(default)]
    pub error: Option<String>,
    #[serde(default)]
    pub state: Option<State>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct State {
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub groups: Vec<Group>,
    #[serde(default = "default_refresh_interval")]
    pub refresh_interval_ms: u64,
    #[serde(default)]
    pub products: Vec<Product>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Product {
    pub id: String,
    pub name: String,
    pub kind: String,
    pub description: String,
    pub status: String,
    pub connection: String,
    #[serde(default)]
    pub battery: Option<Battery>,
    pub sections: Vec<ProductSection>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Battery {
    pub percentage: Option<f64>,
    pub charging: Option<bool>,
    pub source: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ProductSection {
    pub id: String,
    pub title: String,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FormField {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub value: Value,
    #[serde(default)]
    pub min: Option<f64>,
    #[serde(default)]
    pub max: Option<f64>,
    #[serde(default)]
    pub step: Option<f64>,
    #[serde(default)]
    pub options: Vec<OptionItem>,
}

pub fn color_value(value: &str) -> bool {
    value.len() == 7
        && value.starts_with('#')
        && value.as_bytes()[1..].iter().all(u8::is_ascii_hexdigit)
}

impl FormField {
    fn validate(&self) -> Result<()> {
        ensure!(valid_id(&self.id), "invalid form field id");
        text(&self.label, "form label", true)?;
        match self.kind.as_str() {
            "number" => {
                let value = self.value.as_f64();
                ensure!(
                    value.is_some_and(f64::is_finite)
                        && self.min.is_some_and(f64::is_finite)
                        && self.max.is_some_and(f64::is_finite)
                        && self.step.is_some_and(|step| step.is_finite() && step > 0.0),
                    "invalid form number bounds"
                );
                ensure!(
                    self.min <= value && value <= self.max,
                    "form number outside bounds"
                );
            }
            "switch" => ensure!(self.value.is_boolean(), "form switch must be boolean"),
            "text" => text(
                self.value
                    .as_str()
                    .ok_or_else(|| anyhow::anyhow!("form text must be string"))?,
                "form value",
                false,
            )?,
            "color" => ensure!(
                self.value.as_str().is_some_and(color_value),
                "form color must be #RRGGBB"
            ),
            "choice" => {
                ensure!(
                    !self.options.is_empty() && self.options.len() <= 1024,
                    "form choice must have 1–1024 options"
                );
                let mut values = HashSet::new();
                for option in &self.options {
                    text(&option.value, "form option", false)?;
                    text(&option.label, "form option label", true)?;
                    ensure!(values.insert(&option.value), "duplicate form option");
                }
                ensure!(
                    self.value.as_str().is_some_and(|value| self
                        .options
                        .iter()
                        .any(|option| option.value == value)),
                    "form choice absent from options"
                );
            }
            _ => anyhow::bail!("unsupported form field kind"),
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Group {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub description: String,
    pub rows: Vec<Row>,
    #[serde(default)]
    pub visualization: Option<Visualization>,
    #[serde(default)]
    pub collapsed: bool,
}

pub fn default_refresh_interval() -> u64 {
    2000
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Visualization {
    Controller {
        name: String,
        connection: String,
        status: String,
        inputs: Vec<ControllerInput>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<LiveStream>,
    },
    /// Free-form key rectangles in key units ("u"), drawn and hit-tested by the host.
    Keyboard {
        name: String,
        connection: String,
        status: String,
        layout: KeyboardLayout,
        keys: Vec<KeyboardKey>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        paint: Option<KeyPaint>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        remap: Option<KeyRemap>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stream: Option<LiveStream>,
        /// Preview of a lighting effect, drawn on keys whose `color` is null.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        animation: Option<Box<KeyAnimation>>,
    },
}

/// A named lighting effect the host previews on keys without a known colour. The preview is an
/// approximation drawn by the host, not a readback of the keyboard.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyAnimation {
    pub kind: KeyAnimationKind,
    #[serde(default)]
    pub speed: AnimationSpeed,
    /// `#RRGGBB` colours the effect uses; empty means the effect's own (rainbow) colours.
    #[serde(default)]
    pub colors: Vec<String>,
}

pub(crate) const MAX_ANIMATION_COLORS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyAnimationKind {
    RainbowWave,
    Wave,
    Spiral,
    Rain,
    Watercolor,
    Shift,
    Pulse,
    Reactive,
    Ripple,
    /// A kind this host does not know; drawn as an unknown colour, never rejected.
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnimationSpeed {
    Slow,
    #[default]
    Medium,
    Fast,
}

pub(crate) const MAX_KEYS: usize = 512;
const MAX_KEY_LABEL: usize = 32;
const MAX_LAYOUT_UNITS: f64 = 64.0;
// Tolerates decimal key-unit sums such as 0.1 + 0.2 at the layout edge.
const LAYOUT_EPSILON: f64 = 1e-6;

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct KeyboardLayout {
    pub width: f64,
    pub height: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyboardKey {
    pub id: String,
    pub label: String,
    #[serde(default)]
    pub detail: String,
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    /// `#RRGGBB`, or null when the colour is unknown (no readback, animated effect).
    #[serde(default)]
    pub color: Option<String>,
    #[serde(default)]
    pub pressed: Option<bool>,
    /// Current target, one of the visualization's `remap.options` values.
    /// Some target makes this key eligible for shared remapping; absent/null
    /// means ineligible. A standalone `control` is independent of this field.
    #[serde(default)]
    pub remap: Option<String>,
    #[serde(default)]
    pub control: Option<Control>,
}

impl KeyboardKey {
    pub(crate) fn can_remap(&self) -> bool {
        self.remap.is_some()
    }
}

/// Host-submitted: `args.value = {"keys": [selected ids in layout order], "color": "#RRGGBB"}`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyPaint {
    pub label: String,
    pub action: String,
    #[serde(default)]
    pub args: Map<String, Value>,
}

/// Host-submitted: `args.value = {"key": "<id>", "target": "<option value>"}`. Options are
/// shared by eligible keys (`KeyboardKey.remap` is Some), so large target lists
/// are not repeated per key. Absence of a per-key assignment forbids submission.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct KeyRemap {
    pub label: String,
    pub action: String,
    #[serde(default)]
    pub args: Map<String, Value>,
    pub options: Vec<OptionItem>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ControllerInput {
    pub id: String,
    pub label: String,
    pub detail: String,
    #[serde(default)]
    pub pressed: Option<bool>,
    #[serde(default)]
    pub value: Option<f64>,
    #[serde(default)]
    pub x: Option<f64>,
    #[serde(default)]
    pub y: Option<f64>,
    #[serde(default)]
    pub control: Option<Control>,
}

/// Arguments for the enabled plugin's own installed executable, never a command.
/// Shared by every visualization kind that offers a live input stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LiveStream {
    pub args: Vec<String>,
}

impl LiveStream {
    pub(crate) fn validate(&self) -> Result<()> {
        ensure!(
            !self.args.is_empty() && self.args.len() <= 16,
            "input stream requires 1..16 arguments"
        );
        let mut total = 0;
        for arg in &self.args {
            ensure!(
                !arg.is_empty() && arg.len() <= 256 && !arg.chars().any(char::is_control),
                "invalid input stream argument"
            );
            total += arg.len();
        }
        ensure!(total <= 2048, "input stream arguments are too large");
        Ok(())
    }
}

pub(crate) const MAX_INPUT_FRAME: usize = 64 * 1024;

#[derive(Debug, Deserialize)]
pub(crate) struct ControllerFrame {
    pub inputs: Vec<ControllerInput>,
    pub status: String,
}

impl ControllerFrame {
    pub(crate) fn validate(&self) -> Result<()> {
        text(&self.status, "controller stream status", false)?;
        validate_controller_inputs(&self.inputs)
    }
}

/// One complete key-tester snapshot: every declared key not listed is released.
/// `pressed: null` means live key state is unavailable (for example, no permission).
#[derive(Debug, Deserialize)]
pub(crate) struct KeyboardFrame {
    pub status: String,
    #[serde(default)]
    pub pressed: Option<Vec<String>>,
}

impl KeyboardFrame {
    pub(crate) fn validate(&self) -> Result<()> {
        text(&self.status, "keyboard stream status", false)?;
        if let Some(pressed) = &self.pressed {
            ensure!(pressed.len() <= MAX_KEYS, "too many pressed keys");
            let mut ids = HashSet::new();
            for id in pressed {
                ensure!(
                    valid_id(id) && ids.insert(id),
                    "invalid or duplicate pressed key id"
                );
            }
        }
        Ok(())
    }
}

impl Visualization {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Controller {
                name,
                connection,
                status,
                inputs,
                stream,
            } => {
                text(name, "controller name", true)?;
                text(connection, "controller connection", false)?;
                text(status, "controller status", false)?;
                validate_controller_inputs(inputs)?;
                if let Some(stream) = stream {
                    stream.validate()?;
                }
            }
            Self::Keyboard {
                name,
                connection,
                status,
                layout,
                keys,
                paint,
                remap,
                stream,
                animation,
            } => {
                if let Some(animation) = animation {
                    ensure!(
                        animation.colors.len() <= MAX_ANIMATION_COLORS
                            && animation.colors.iter().all(|color| color_value(color)),
                        "keyboard animation colors must be at most {MAX_ANIMATION_COLORS} #RRGGBB values"
                    );
                }
                text(name, "keyboard name", true)?;
                text(connection, "keyboard connection", false)?;
                text(status, "keyboard status", false)?;
                if let Some(paint) = paint {
                    text(&paint.label, "keyboard paint label", true)?;
                    action_name(&paint.action)?;
                }
                let options = match remap {
                    Some(remap) => {
                        text(&remap.label, "keyboard remap label", true)?;
                        action_name(&remap.action)?;
                        ensure!(
                            !remap.options.is_empty() && remap.options.len() <= 1024,
                            "keyboard remap must have 1–1024 options"
                        );
                        let mut values = HashSet::new();
                        for option in &remap.options {
                            text(&option.value, "keyboard remap option", false)?;
                            text(&option.label, "keyboard remap option label", true)?;
                            ensure!(
                                values.insert(option.value.as_str()),
                                "duplicate keyboard remap option"
                            );
                        }
                        Some(values)
                    }
                    None => None,
                };
                validate_keyboard_keys(layout, keys, options.as_ref())?;
                if let Some(stream) = stream {
                    stream.validate()?;
                }
            }
        }
        Ok(())
    }
}

fn validate_keyboard_keys(
    layout: &KeyboardLayout,
    keys: &[KeyboardKey],
    remap: Option<&HashSet<&str>>,
) -> Result<()> {
    let units = 1.0..=MAX_LAYOUT_UNITS;
    ensure!(
        units.contains(&layout.width) && units.contains(&layout.height),
        "keyboard layout must be 1–64 units in each dimension"
    );
    ensure!(
        !keys.is_empty() && keys.len() <= MAX_KEYS,
        "keyboard must have 1–{MAX_KEYS} keys"
    );
    let mut ids = HashSet::new();
    for key in keys {
        ensure!(
            valid_id(&key.id) && ids.insert(key.id.as_str()),
            "invalid or duplicate keyboard key id {:?}",
            key.id
        );
        text(&key.label, "keyboard key label", true)?;
        ensure!(
            key.label.len() <= MAX_KEY_LABEL && !key.label.chars().any(char::is_control),
            "keyboard key label for {} must be at most {MAX_KEY_LABEL} bytes without control characters",
            key.id
        );
        text(&key.detail, "keyboard key detail", false)?;
        ensure!(
            [key.x, key.y, key.w, key.h]
                .iter()
                .all(|value| value.is_finite())
                && key.x >= 0.0
                && key.y >= 0.0
                && key.w > 0.0
                && key.h > 0.0
                && key.x + key.w <= layout.width + LAYOUT_EPSILON
                && key.y + key.h <= layout.height + LAYOUT_EPSILON,
            "keyboard key {} lies outside the layout",
            key.id
        );
        ensure!(
            key.color.as_deref().is_none_or(color_value),
            "keyboard key color must be #RRGGBB or null"
        );
        if let Some(value) = &key.remap {
            ensure!(
                remap.is_some_and(|options| options.contains(value.as_str())),
                "keyboard key {} remap value is absent from remap options",
                key.id
            );
        }
        if let Some(control) = &key.control {
            control.validate()?;
        }
    }
    Ok(())
}

fn validate_controller_inputs(inputs: &[ControllerInput]) -> Result<()> {
    ensure!(inputs.len() <= 32, "too many controller inputs");
    const IDS: [&str; 23] = [
        "a",
        "b",
        "x",
        "y",
        "lb",
        "rb",
        "lt",
        "rt",
        "left_stick",
        "right_stick",
        "dpad_up",
        "dpad_down",
        "dpad_left",
        "dpad_right",
        "view",
        "menu",
        "guide",
        "m1",
        "m2",
        "m3",
        "m4",
        "m5",
        "m6",
    ];
    let mut ids = 0u32;
    for input in inputs {
        let index = IDS
            .iter()
            .position(|id| *id == input.id)
            .ok_or_else(|| anyhow::anyhow!("unknown controller input id {}", input.id))?;
        let bit = 1u32 << index;
        ensure!(ids & bit == 0, "duplicate controller input id {}", input.id);
        ids |= bit;
        text(&input.label, "controller input label", true)?;
        text(&input.detail, "controller input detail", false)?;
        for (value, min, max) in [
            (input.value, 0.0, 1.0),
            (input.x, -1.0, 1.0),
            (input.y, -1.0, 1.0),
        ] {
            ensure!(
                value.is_none_or(|value| value.is_finite() && (min..=max).contains(&value)),
                "invalid normalized controller value"
            );
        }
        if let Some(control) = &input.control {
            control.validate()?;
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Row {
    pub id: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    #[serde(default)]
    pub control: Option<Control>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OptionItem {
    pub value: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DpiStage {
    pub id: String,
    pub label: String,
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Control {
    Button {
        label: String,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
        #[serde(default)]
        destructive: bool,
    },
    Switch {
        value: bool,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    Number {
        value: f64,
        min: f64,
        max: f64,
        step: f64,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    Choice {
        value: String,
        options: Vec<OptionItem>,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    Text {
        value: String,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    Color {
        value: String,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    Dpi {
        x: Option<f64>,
        y: Option<f64>,
        min: f64,
        max: f64,
        step: f64,
        linked: bool,
        stages: Vec<DpiStage>,
        active: Option<String>,
        storage: String,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
    },
    Form {
        fields: Vec<FormField>,
        action: String,
        #[serde(default)]
        args: Map<String, Value>,
        #[serde(default = "default_apply_label")]
        label: String,
    },
}

fn default_apply_label() -> String {
    "Apply".into()
}

pub fn text(value: &str, what: &str, required: bool) -> Result<()> {
    ensure!(!required || !value.trim().is_empty(), "{what} is empty");
    ensure!(
        value.len() <= 16_384 && !value.contains('\0'),
        "{what} is too long or contains NUL"
    );
    Ok(())
}

pub fn action_name(action: &str) -> Result<()> {
    text(action, "action", true)?;
    ensure!(action.len() <= 128, "action name is too long");
    Ok(())
}

impl State {
    pub fn validate(&self) -> Result<()> {
        text(&self.title, "state title", true)?;
        text(&self.description, "state description", false)?;
        ensure!(
            (250..=10_000).contains(&self.refresh_interval_ms),
            "refresh_interval_ms must be 250..10000"
        );
        ensure!(self.groups.len() <= 128, "too many groups");
        let mut groups = HashSet::new();
        let mut total = 0;
        for group in &self.groups {
            text(&group.id, "group id", true)?;
            ensure!(groups.insert(&group.id), "duplicate group id {}", group.id);
            text(&group.title, "group title", true)?;
            text(&group.description, "group description", false)?;
            if let Some(visualization) = &group.visualization {
                visualization.validate()?;
            }
            total += group.rows.len();
            ensure!(total <= 1024, "too many rows");
            let mut rows = HashSet::new();
            for row in &group.rows {
                text(&row.id, "row id", true)?;
                ensure!(rows.insert(&row.id), "duplicate row id {}", row.id);
                text(&row.title, "row title", true)?;
                text(&row.subtitle, "row subtitle", false)?;
                if let Some(control) = &row.control {
                    control.validate()?;
                }
            }
        }
        ensure!(self.products.len() <= 64, "too many products");
        let mut products = HashSet::new();
        for product in &self.products {
            ensure!(
                valid_id(&product.id) && products.insert(&product.id),
                "invalid or duplicate product id"
            );
            ensure!(
                matches!(
                    product.kind.as_str(),
                    "mouse"
                        | "controller"
                        | "dock"
                        | "keyboard"
                        | "keypad"
                        | "headset"
                        | "mousemat"
                        | "accessory"
                        | "peripheral"
                ),
                "unsupported product depiction"
            );
            text(&product.name, "product name", true)?;
            for value in [&product.description, &product.status, &product.connection] {
                text(value, "product text", false)?;
            }
            if let Some(battery) = &product.battery {
                ensure!(
                    battery
                        .percentage
                        .is_none_or(|value| value.is_finite() && (0.0..=100.0).contains(&value)),
                    "invalid battery percentage"
                );
                text(&battery.source, "battery source", false)?;
            }
            ensure!(product.sections.len() <= 12, "too many product sections");
            let mut sections = HashSet::new();
            for section in &product.sections {
                ensure!(
                    valid_id(&section.id) && sections.insert(&section.id),
                    "invalid or duplicate section id"
                );
                text(&section.title, "section title", true)?;
                let mut refs = HashSet::new();
                for id in &section.groups {
                    ensure!(
                        groups.contains(id) && refs.insert(id),
                        "unknown or duplicate product group reference"
                    );
                }
            }
        }
        Ok(())
    }
}

impl Control {
    fn validate(&self) -> Result<()> {
        let action = match self {
            Self::Button { label, action, .. } => {
                text(label, "button label", true)?;
                action
            }
            Self::Switch { action, .. } => action,
            Self::Number {
                value,
                min,
                max,
                step,
                action,
                ..
            } => {
                ensure!(
                    value.is_finite()
                        && min.is_finite()
                        && max.is_finite()
                        && step.is_finite()
                        && min <= max
                        && value >= min
                        && value <= max
                        && *step > 0.0,
                    "invalid numeric control bounds/value/step"
                );
                action
            }
            Self::Choice {
                value,
                options,
                action,
                ..
            } => {
                ensure!(
                    !options.is_empty() && options.len() <= 256,
                    "choice must have 1–256 options"
                );
                let mut seen = HashSet::new();
                for option in options {
                    text(&option.value, "choice value", false)?;
                    text(&option.label, "choice label", true)?;
                    ensure!(seen.insert(&option.value), "duplicate choice value");
                }
                ensure!(
                    options.iter().any(|o| &o.value == value),
                    "choice value is absent from options"
                );
                action
            }
            Self::Text { value, action, .. } => {
                text(value, "text value", false)?;
                action
            }
            Self::Color { value, action, .. } => {
                ensure!(color_value(value), "color must be #RRGGBB");
                action
            }
            Self::Form {
                fields,
                action,
                label,
                ..
            } => {
                ensure!(
                    !fields.is_empty() && fields.len() <= 32,
                    "form must have 1..32 fields"
                );
                text(label, "form apply label", true)?;
                let mut ids = HashSet::new();
                for field in fields {
                    field.validate()?;
                    ensure!(ids.insert(&field.id), "duplicate form field id");
                }
                action
            }
            Self::Dpi {
                x,
                y,
                min,
                max,
                step,
                stages,
                active,
                storage,
                action,
                ..
            } => {
                ensure!(
                    min.is_finite()
                        && max.is_finite()
                        && min <= max
                        && *min >= 1.0
                        && *max <= 65535.0
                        && step.is_finite()
                        && *step > 0.0,
                    "invalid DPI bounds"
                );
                let valid = |value: f64| {
                    value.is_finite() && value.fract() == 0.0 && (*min..=*max).contains(&value)
                };
                ensure!(
                    x.is_none_or(valid) && y.is_none_or(valid),
                    "invalid current DPI"
                );
                ensure!(
                    matches!(storage.as_str(), "host" | "device"),
                    "invalid DPI preset storage"
                );
                ensure!(
                    !stages.is_empty() && stages.len() <= 8,
                    "DPI editor requires 1..8 stages"
                );
                let mut ids = HashSet::new();
                for stage in stages {
                    ensure!(
                        valid_id(&stage.id) && ids.insert(&stage.id),
                        "invalid or duplicate DPI stage id"
                    );
                    text(&stage.label, "DPI stage label", true)?;
                    ensure!(valid(stage.x) && valid(stage.y), "invalid DPI stage values");
                }
                ensure!(
                    active.as_ref().is_none_or(|id| ids.contains(id)),
                    "active DPI stage is missing"
                );
                action
            }
        };
        action_name(action)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn manifest() -> Manifest {
        Manifest {
            api_version: 1,
            id: "sample".into(),
            name: "Sample".into(),
            description: "".into(),
            version: "1.0.0".into(),
            executable: "hyprdeck-sample".into(),
            update_repo: "owner/sample".into(),
            asset: format!("hyprdeck-sample-linux-{}", std::env::consts::ARCH),
        }
    }
    #[test]
    fn rejects_traversal_and_incompatible_manifest() {
        for path in ["../outside", "/bin/sh", "sub/program", "..", "a\\b"] {
            let mut m = manifest();
            m.executable = path.into();
            assert!(m.validate().is_err());
        }
        let mut m = manifest();
        m.api_version = 2;
        assert!(m.validate().is_err());
        let mut m = manifest();
        m.asset = "foreign-platform".into();
        assert!(m.validate().is_err());
    }
    #[test]
    fn update_requires_same_identity_and_new_stable_version() {
        let old = manifest();
        for version in ["0.9.0", "1.0.0", "2.0.0-rc.1", "garbage"] {
            let mut new = old.clone();
            new.version = version.into();
            assert!(new.validate_update(&old).is_err());
        }
        let mut new = old.clone();
        new.version = "1.1.0".into();
        new.id = "other".into();
        assert!(new.validate_update(&old).is_err());
        new.id = old.id.clone();
        new.update_repo = "attacker/sample".into();
        assert!(new.validate_update(&old).is_err());
        new.update_repo = old.update_repo.clone();
        assert!(new.validate_update(&old).is_ok());
    }

    #[test]
    fn controller_visualization_rejects_ambiguous_and_invalid_telemetry() {
        let input = ControllerInput {
            id: "a".into(),
            label: "A".into(),
            detail: String::new(),
            pressed: None,
            value: None,
            x: None,
            y: None,
            control: None,
        };
        let visualization = |inputs| Visualization::Controller {
            name: "Controller".into(),
            connection: String::new(),
            status: String::new(),
            inputs,
            stream: None,
        };
        assert!(
            visualization(vec![input.clone(), input.clone()])
                .validate()
                .is_err()
        );
        let mut invalid = input.clone();
        invalid.id = "unknown".into();
        assert!(visualization(vec![invalid]).validate().is_err());
        for value in [f64::NAN, f64::INFINITY, -0.1, 1.1] {
            let mut invalid = input.clone();
            invalid.value = Some(value);
            assert!(visualization(vec![invalid]).validate().is_err());
        }
        let mut invalid = input;
        invalid.x = Some(-1.1);
        assert!(visualization(vec![invalid]).validate().is_err());
    }

    #[test]
    fn controller_stream_is_optional_and_arguments_are_bounded() {
        let legacy: Visualization = serde_json::from_value(serde_json::json!({
            "kind": "controller", "name": "Controller", "connection": "USB",
            "status": "Connected", "inputs": []
        }))
        .unwrap();
        assert!(legacy.validate().is_ok());
        assert!(matches!(
            legacy,
            Visualization::Controller { stream: None, .. }
        ));
        let stream = LiveStream {
            args: vec!["--input-stream".into(), "usb:1-8".into()],
        };
        assert!(stream.validate().is_ok());
        for args in [
            vec![],
            vec![String::new()],
            vec!["x".repeat(257)],
            vec!["x\0y".into()],
            vec!["line\nbreak".into()],
            vec!["x".into(); 17],
            vec!["x".repeat(256); 9],
        ] {
            assert!(LiveStream { args }.validate().is_err());
        }
    }

    #[test]
    fn live_frames_share_snapshot_telemetry_validation() {
        let base = serde_json::json!({"inputs": [{
            "id": "left_stick", "label": "Left stick", "detail": "Live",
            "x": 0.25, "y": -0.5
        }], "status": "Live input"});
        assert!(
            serde_json::from_value::<ControllerFrame>(base.clone())
                .unwrap()
                .validate()
                .is_ok()
        );
        let mut invalid = base.clone();
        invalid["inputs"][0]["x"] = serde_json::json!(1.01);
        assert!(
            serde_json::from_value::<ControllerFrame>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut duplicate = base.clone();
        duplicate["inputs"]
            .as_array_mut()
            .unwrap()
            .push(base["inputs"][0].clone());
        assert!(
            serde_json::from_value::<ControllerFrame>(duplicate)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    fn keyboard() -> Value {
        serde_json::json!({
            "kind": "keyboard", "name": "Keyboard", "connection": "USB", "status": "",
            "layout": {"width": 15.0, "height": 5.0},
            "keys": [
                {"id": "esc", "label": "Esc", "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0,
                 "color": "#FF2200", "pressed": null, "remap": "default"},
                {"id": "enter", "label": "Enter", "x": 12.75, "y": 2.0, "w": 2.25, "h": 1.0}
            ],
            "paint": {"label": "Apply colour", "action": "set-key-colors", "args": {"device": "kb"}},
            "remap": {"label": "Assign key", "action": "remap-key", "args": {"device": "kb"},
                      "options": [{"value": "default", "label": "Default"},
                                  {"value": "f13", "label": "F13"}]},
            "stream": {"args": ["--input-stream", "kb"]}
        })
    }

    fn keyboard_result(value: Value) -> Result<()> {
        serde_json::from_value::<Visualization>(value)?.validate()
    }

    #[test]
    fn keyboard_visualization_accepts_valid_and_legacy_minimal_documents() {
        assert!(keyboard_result(keyboard()).is_ok());
        let minimal = serde_json::json!({
            "kind": "keyboard", "name": "Keyboard", "connection": "", "status": "",
            "layout": {"width": 1.0, "height": 1.0},
            "keys": [{"id": "a", "label": "A", "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0}]
        });
        let parsed: Visualization = serde_json::from_value(minimal).unwrap();
        assert!(parsed.validate().is_ok());
        let Visualization::Keyboard {
            paint,
            remap,
            stream,
            keys,
            ..
        } = &parsed
        else {
            panic!("expected keyboard");
        };
        assert!(paint.is_none() && remap.is_none() && stream.is_none());
        assert_eq!((keys[0].color.as_ref(), keys[0].pressed), (None, None));
        // Optional fields stay absent when the host re-serializes a state.
        let round_trip = serde_json::to_value(&parsed).unwrap();
        assert!(round_trip.get("paint").is_none() && round_trip.get("stream").is_none());
        assert_eq!(
            serde_json::from_value::<Visualization>(round_trip).unwrap(),
            parsed
        );
        // Decimal key-unit sums at the layout edge are not rejected by rounding.
        // In binary floating point 1.1 + 2.2 exceeds 3.3.
        let mut edge = keyboard();
        edge["layout"] = serde_json::json!({"width": 3.3, "height": 1.0});
        edge["keys"] = serde_json::json!([
            {"id": "a", "label": "A", "x": 1.1, "y": 0.0, "w": 2.2, "h": 1.0}
        ]);
        assert!(keyboard_result(edge).is_ok());
    }

    #[test]
    fn keyboard_animation_is_optional_tolerant_and_colour_checked() {
        let mut animated = keyboard();
        animated["animation"] =
            serde_json::json!({"kind": "rain", "speed": "slow", "colors": ["#00AAFF"]});
        let parsed: Visualization = serde_json::from_value(animated.clone()).unwrap();
        assert!(parsed.validate().is_ok());
        let Visualization::Keyboard { animation, .. } = &parsed else {
            panic!("expected keyboard");
        };
        let animation = animation.as_deref().unwrap();
        assert_eq!(
            (animation.kind, animation.speed),
            (KeyAnimationKind::Rain, AnimationSpeed::Slow)
        );
        // Speed and colours are optional.
        animated["animation"] = serde_json::json!({"kind": "rainbow_wave"});
        assert!(keyboard_result(animated.clone()).is_ok());
        // A kind from a newer plugin is drawn as unknown rather than failing the whole state.
        animated["animation"] = serde_json::json!({"kind": "aurora_from_the_future"});
        let parsed: Visualization = serde_json::from_value(animated.clone()).unwrap();
        assert!(parsed.validate().is_ok());
        let Visualization::Keyboard { animation, .. } = &parsed else {
            panic!("expected keyboard");
        };
        assert_eq!(
            animation.as_deref().unwrap().kind,
            KeyAnimationKind::Unknown
        );
        animated["animation"] = serde_json::json!({"kind": "wave", "colors": ["red"]});
        assert!(keyboard_result(animated.clone()).is_err());
        animated["animation"] = serde_json::json!({"kind": "wave", "colors": vec!["#FF0000"; 9]});
        assert!(keyboard_result(animated).is_err());
    }

    #[test]
    fn keyboard_visualization_rejects_invalid_identity_geometry_and_values() {
        type Mutation = (&'static str, fn(&mut Value));
        let mutations: Vec<Mutation> = vec![
            ("empty name", |v| v["name"] = " ".into()),
            ("zero layout", |v| v["layout"]["width"] = 0.5.into()),
            ("huge layout", |v| v["layout"]["height"] = 65.0.into()),
            ("no keys", |v| v["keys"] = serde_json::json!([])),
            ("duplicate id", |v| v["keys"][1]["id"] = "esc".into()),
            ("invalid id", |v| v["keys"][0]["id"] = "Esc".into()),
            ("empty label", |v| v["keys"][0]["label"] = "".into()),
            ("long label", |v| {
                v["keys"][0]["label"] = "x".repeat(33).into()
            }),
            ("control label", |v| v["keys"][0]["label"] = "a\nb".into()),
            ("negative x", |v| v["keys"][0]["x"] = (-0.5).into()),
            ("zero width", |v| v["keys"][0]["w"] = 0.0.into()),
            ("outside x", |v| v["keys"][1]["x"] = 13.0.into()),
            ("outside y", |v| v["keys"][1]["h"] = 3.5.into()),
            ("bad color", |v| v["keys"][0]["color"] = "red".into()),
            ("unknown remap", |v| v["keys"][0]["remap"] = "f99".into()),
            ("remap without action", |v| {
                v.as_object_mut().unwrap().remove("remap");
            }),
            ("empty remap options", |v| {
                v["remap"]["options"] = serde_json::json!([])
            }),
            ("duplicate remap option", |v| {
                v["remap"]["options"][1]["value"] = "default".into();
            }),
            ("empty paint action", |v| v["paint"]["action"] = "".into()),
            ("empty paint label", |v| v["paint"]["label"] = "".into()),
            ("bad stream", |v| {
                v["stream"]["args"] = serde_json::json!([])
            }),
            ("bad control", |v| {
                v["keys"][0]["control"] =
                    serde_json::json!({"kind": "color", "value": "red", "action": "set"});
            }),
        ];
        for (what, mutate) in mutations {
            let mut value = keyboard();
            mutate(&mut value);
            assert!(keyboard_result(value).is_err(), "{what} was accepted");
        }
        for value in [f64::NAN, f64::INFINITY] {
            let Ok(mut visualization) = serde_json::from_value::<Visualization>(keyboard()) else {
                panic!("valid fixture");
            };
            if let Visualization::Keyboard { keys, .. } = &mut visualization {
                keys[0].x = value;
            }
            assert!(visualization.validate().is_err());
        }
        let mut many = keyboard();
        many["layout"] = serde_json::json!({"width": 64.0, "height": 64.0});
        many["keys"] = (0..=MAX_KEYS)
            .map(|index| {
                serde_json::json!({"id": format!("k{index}"), "label": "K",
                    "x": (index % 64) as f64, "y": (index / 64) as f64, "w": 1.0, "h": 1.0})
            })
            .collect();
        assert!(keyboard_result(many.clone()).is_err());
        many["keys"].as_array_mut().unwrap().pop();
        assert!(keyboard_result(many).is_ok());
    }

    #[test]
    fn keyboard_frames_list_unique_valid_pressed_ids_or_null() {
        let frame = |value: Value| {
            serde_json::from_value::<KeyboardFrame>(value)
                .map_err(anyhow::Error::from)
                .and_then(|frame| frame.validate().map(|()| frame))
        };
        let parsed =
            frame(serde_json::json!({"status": "Live keys", "pressed": ["a", "l-shift"]})).unwrap();
        assert_eq!(parsed.pressed.unwrap(), ["a", "l-shift"]);
        assert!(
            frame(serde_json::json!({"status": "No permission", "pressed": null}))
                .unwrap()
                .pressed
                .is_none()
        );
        assert!(frame(serde_json::json!({"status": "x", "pressed": ["a", "a"]})).is_err());
        assert!(frame(serde_json::json!({"status": "x", "pressed": ["A"]})).is_err());
        assert!(frame(serde_json::json!({"status": "x\0", "pressed": []})).is_err());
        let too_many: Vec<_> = (0..=MAX_KEYS).map(|index| format!("k{index}")).collect();
        assert!(frame(serde_json::json!({"status": "x", "pressed": too_many})).is_err());
    }

    #[test]
    fn keyboard_example_fixture_is_a_valid_state() {
        let response: Response = serde_json::from_str(include_str!(
            "../../../docs/examples/keyboard-visualization.json"
        ))
        .unwrap();
        assert_eq!(response.api_version, API_VERSION);
        response.state.unwrap().validate().unwrap();
    }

    #[test]
    fn requests_advertise_host_features() {
        let request = Request {
            api_version: API_VERSION,
            method: "state",
            action: None,
            args: Map::new(),
            host_features: HOST_FEATURES,
        };
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            serde_json::json!({"api_version": 1, "method": "state", "action": null,
                "args": {}, "host_features": ["keyboard_visualization", "keyboard_animation"]})
        );
    }

    #[test]
    fn product_references_and_battery_bounds_are_validated() {
        let base = serde_json::json!({"title":"Devices","groups":[{"id":"overview","title":"Overview","rows":[]}],"products":[{"id":"mouse","name":"Mouse","kind":"mouse","description":"","status":"Connected","connection":"USB","battery":{"percentage":75.0,"charging":false,"source":"device"},"sections":[{"id":"overview","title":"Overview","groups":["overview"]}]}]});
        let mut missing = base.clone();
        missing["products"][0]["sections"][0]["groups"] = serde_json::json!(["missing"]);
        assert!(
            serde_json::from_value::<State>(missing)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut invalid = base.clone();
        invalid["products"][0]["battery"]["percentage"] = serde_json::json!(101.0);
        assert!(
            serde_json::from_value::<State>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut duplicate = base.clone();
        duplicate["products"][0]["sections"]
            .as_array_mut()
            .unwrap()
            .push(base["products"][0]["sections"][0].clone());
        assert!(
            serde_json::from_value::<State>(duplicate)
                .unwrap()
                .validate()
                .is_err()
        );
    }

    #[test]
    fn structured_controls_reject_invalid_colors_and_field_values() {
        let invalid: Control = serde_json::from_value(
            serde_json::json!({"kind":"color","value":"red","action":"set"}),
        )
        .unwrap();
        assert!(invalid.validate().is_err());
        let invalid: Control = serde_json::from_value(serde_json::json!({"kind":"form","action":"set","fields":[{"id":"dpi","label":"DPI","kind":"number","value":900,"min":100,"max":800,"step":100}]})).unwrap();
        assert!(invalid.validate().is_err());
        let invalid: Control = serde_json::from_value(serde_json::json!({"kind":"form","action":"set","fields":[{"id":"effect","label":"Effect","kind":"choice","value":"missing","options":[{"value":"static","label":"Static"}]}]})).unwrap();
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn dpi_presets_require_valid_identity_and_real_value_bounds() {
        let base = serde_json::json!({"kind":"dpi","x":null,"y":null,"min":100,"max":20000,"step":50,"linked":true,"stages":[{"id":"stage-1","label":"800 DPI","x":800,"y":800}],"active":null,"storage":"host","action":"dpi-presets"});
        let mut invalid = base.clone();
        invalid["stages"][0]["x"] = serde_json::json!(0);
        assert!(
            serde_json::from_value::<Control>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut invalid = base.clone();
        invalid["active"] = serde_json::json!("missing");
        assert!(
            serde_json::from_value::<Control>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut invalid = base.clone();
        invalid["stages"]
            .as_array_mut()
            .unwrap()
            .push(base["stages"][0].clone());
        assert!(
            serde_json::from_value::<Control>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
        let mut invalid = base;
        invalid["x"] = serde_json::json!(400.5);
        assert!(
            serde_json::from_value::<Control>(invalid)
                .unwrap()
                .validate()
                .is_err()
        );
    }
}
