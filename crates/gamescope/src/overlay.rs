//! MangoApp's native configuration is shared by launch and the real preview.
use anyhow::{Context, Result, bail};
use hyprdeck_core::{cmd, store};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG: &str = "fps\nfps_text=FPS\nhide_fps_superscript=1\ngpu_stats\ngpu_temp\ncpu_stats\ncpu_temp\nframetime=0\nframe_timing=0\nmangoapp_steam=1\nposition=top-left\nfont_size=24\nbackground_alpha=0.5\n";

// Plan refreshes and saves run on workers. Serialize profile/native-file writes
// so an older plan cannot overwrite a just-saved appearance.
static CONFIG_WRITE: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub enabled: bool,
    pub config: String,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            config: DEFAULT_CONFIG.into(),
        }
    }
}

impl Settings {
    pub fn load() -> Result<Self> {
        store::load("overlay")
    }

    pub fn save(&self) -> Result<()> {
        validate(&self.config)?;
        let _guard = CONFIG_WRITE
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        store::save("overlay", self)?;
        materialize(&self.config)?;
        Ok(())
    }
}

/// Persist appearance without changing the separately saved next-launch toggle.
pub fn save_appearance(config: &str) -> Result<()> {
    validate(config)?;
    let _guard = CONFIG_WRITE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let mut settings = Settings::load()?;
    settings.config = config.into();
    store::save("overlay", &settings)?;
    materialize(config)?;
    Ok(())
}

pub fn validate(config: &str) -> Result<()> {
    if config.contains('\0') {
        bail!("MangoHUD configuration cannot contain NUL bytes.");
    }
    Ok(())
}

fn materialize(config: &str) -> Result<std::path::PathBuf> {
    validate(config)?;
    let path = store::config_dir().join("MangoHud.conf");
    if std::fs::read(&path).ok().as_deref() != Some(config.as_bytes()) {
        store::write_atomic(&path, config.as_bytes())?;
    }
    std::fs::canonicalize(path).context("Resolving the MangoHUD configuration path")
}

pub fn launch_environment() -> Result<Option<(String, String)>> {
    let _guard = CONFIG_WRITE
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    let settings = Settings::load().context("Reading performance overlay settings")?;
    if !settings.enabled {
        return Ok(None);
    }
    require_mangoapp()?;
    let path = materialize(&settings.config)?;
    Ok(Some((
        "MANGOHUD_CONFIGFILE".into(),
        path.to_string_lossy().into_owned(),
    )))
}

pub fn require_mangoapp() -> Result<()> {
    if cmd::which("mangoapp").is_none() {
        bail!(
            "MangoApp is missing. Install the mangohud package (or mangoapp on distributions packaging it separately), then retry; or disable the performance overlay."
        );
    }
    Ok(())
}

/// Replace just one native option, retaining comments and unrecognised settings.
/// Remove duplicate assignments so the control's value is unambiguous.
pub fn set_option(config: &str, key: &str, value: &str) -> String {
    let mut result = remove_option(config, key);
    result.reserve(key.len() + value.len() + 2);
    result.push_str(key);
    result.push('=');
    result.push_str(value);
    result.push('\n');
    result
}

/// Remove only active assignments, keeping unrelated options and comments.
pub fn remove_option(config: &str, key: &str) -> String {
    let mut result = String::with_capacity(config.len());
    for line in config.lines() {
        if parsed_option(line).is_some_and(|(option, _)| option == key) {
            continue;
        }
        result.push_str(line);
        result.push('\n');
    }
    result
}

pub(super) fn parsed_option(line: &str) -> Option<(&str, &str)> {
    // MangoHUD strips everything after #, including on assignment lines.
    let line = line.split('#').next()?.trim();
    if line.is_empty() {
        return None;
    }
    Some(
        line.split_once('=')
            .map_or((line, "1"), |(key, value)| (key.trim(), value.trim())),
    )
}

pub fn option<'a>(config: &'a str, key: &str) -> Option<&'a str> {
    config.lines().rev().find_map(|line| {
        let (option, value) = parsed_option(line)?;
        (option == key).then_some(value)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editing_preserves_custom_settings_but_removes_conflicting_duplicates() {
        let input = "# fps=0\nfps\ncustom_text=My HUD\nfps = 0\ngraphs=gpu_load,cpu_load\n";
        let edited = set_option(input, "fps", "1");
        assert_eq!(
            edited,
            "# fps=0\ncustom_text=My HUD\ngraphs=gpu_load,cpu_load\nfps=1\n"
        );
        assert_eq!(option(&edited, "fps"), Some("1"));
        assert_eq!(option(&edited, "custom_text"), Some("My HUD"));
    }
    #[test]
    fn comments_do_not_override_native_values() {
        assert_eq!(option("fps=0\n#fps\n", "fps"), Some("0"));
        assert_eq!(option("fps=0\nfps\n", "fps"), Some("1"));
        assert_eq!(option("#font_size=24\n", "font_size"), None);
    }
    #[test]
    fn inline_comments_and_reset_follow_native_precedence() {
        let config =
            "# FPS heading\nfps=1\nfps = 0 # disabled\ncustom_text=Game # native comment\n";
        assert_eq!(option(config, "fps"), Some("0"));
        assert_eq!(option(config, "custom_text"), Some("Game"));
        assert_eq!(
            set_option(config, "fps", "1"),
            "# FPS heading\ncustom_text=Game # native comment\nfps=1\n"
        );
        assert_eq!(
            remove_option(config, "fps"),
            "# FPS heading\ncustom_text=Game # native comment\n"
        );
    }
}
