//! Version 1 subprocess protocol. Unknown object fields are forward-compatible;
//! unknown API versions and control kinds are not.
use std::collections::HashSet;

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub const API_VERSION: u32 = 1;
pub const MAX_DOCUMENT: usize = 1024 * 1024;

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
                    !self.options.is_empty() && self.options.len() <= 256,
                    "invalid form choices"
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
    },
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

impl Visualization {
    fn validate(&self) -> Result<()> {
        match self {
            Self::Controller {
                name,
                connection,
                status,
                inputs,
            } => {
                text(name, "controller name", true)?;
                text(connection, "controller connection", false)?;
                text(status, "controller status", false)?;
                ensure!(inputs.len() <= 32, "too many controller inputs");
                let mut ids = HashSet::new();
                for input in inputs {
                    ensure!(
                        matches!(
                            input.id.as_str(),
                            "a" | "b"
                                | "x"
                                | "y"
                                | "lb"
                                | "rb"
                                | "lt"
                                | "rt"
                                | "left_stick"
                                | "right_stick"
                                | "dpad_up"
                                | "dpad_down"
                                | "dpad_left"
                                | "dpad_right"
                                | "view"
                                | "menu"
                                | "guide"
                                | "m1"
                                | "m2"
                                | "m3"
                                | "m4"
                                | "m5"
                                | "m6"
                        ),
                        "unknown controller input id {}",
                        input.id
                    );
                    ensure!(
                        ids.insert(&input.id),
                        "duplicate controller input id {}",
                        input.id
                    );
                    text(&input.label, "controller input label", true)?;
                    text(&input.detail, "controller input detail", false)?;
                    for (value, min, max) in [
                        (input.value, 0.0, 1.0),
                        (input.x, -1.0, 1.0),
                        (input.y, -1.0, 1.0),
                    ] {
                        ensure!(
                            value.is_none_or(
                                |value| value.is_finite() && (min..=max).contains(&value)
                            ),
                            "invalid normalized controller value"
                        );
                    }
                    if let Some(control) = &input.control {
                        control.validate()?;
                    }
                }
            }
        }
        Ok(())
    }
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
                    "mouse" | "controller" | "dock" | "peripheral"
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
}
