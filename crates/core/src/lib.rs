//! Shared foundation for hyprdeck feature crates.

pub mod cmd;
pub mod events;
pub mod hypr;
pub mod notify;
pub mod rt;
pub mod store;
pub mod tray;
pub mod ui;

/// GApplication id, desktop-file name and icon name.
pub const APP_ID: &str = "io.github.mikkeyboi.Hyprdeck";
/// Icon name installed under hicolor.
pub const APP_ICON: &str = APP_ID;

/// Release version of this build.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
/// Full git commit this binary was built from, when known.
pub const BUILD_COMMIT: Option<&str> = option_env!("HYPRDECK_COMMIT");

/// `0.1.0 (533e43f)` or just `0.1.0` when the commit is unknown.
pub fn version_string() -> String {
    match BUILD_COMMIT {
        Some(c) => format!("{VERSION} ({})", &c[..7]),
        None => VERSION.to_owned(),
    }
}
