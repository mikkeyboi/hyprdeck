//! Per-feature persisted settings in `~/.config/hyprdeck/<name>.toml` and state
//! directories. Each feature crate owns its own schema type.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub fn config_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| home().join(".config"))
        .join("hyprdeck")
}

pub fn state_dir() -> PathBuf {
    dirs::state_dir()
        .unwrap_or_else(|| home().join(".local/state"))
        .join("hyprdeck")
}

pub fn home() -> PathBuf {
    dirs::home_dir().expect("HOME is not set")
}

fn path_for(name: &str) -> PathBuf {
    config_dir().join(format!("{name}.toml"))
}

/// Load `<config_dir>/<name>.toml`; missing file yields `T::default()`.
/// A malformed file is an error (never silently replaced with defaults).
pub fn load<T: DeserializeOwned + Default>(name: &str) -> Result<T> {
    let path = path_for(name);
    match std::fs::read_to_string(&path) {
        Ok(text) => toml::from_str(&text).with_context(|| format!("invalid {}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
    }
}

/// Atomically write `<config_dir>/<name>.toml`.
pub fn save<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let text = toml::to_string_pretty(value).context("serializing settings")?;
    write_atomic(&path_for(name), text.as_bytes())
}

/// Write via temp file + rename so readers never observe a partial file.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    let dir = path.parent().context("path has no parent")?;
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(())
}
