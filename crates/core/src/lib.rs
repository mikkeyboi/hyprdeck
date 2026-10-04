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

/// Release version of this build: the `vX.Y.Z` tag it was built from or after
/// (see `build.rs`).
pub const VERSION: &str = env!("HYPRDECK_VERSION");
/// Commits between the release tag and this build (0 for release builds).
pub const COMMITS_SINCE_RELEASE: &str = env!("HYPRDECK_DISTANCE");
/// Full git commit this binary was built from, when known.
pub const BUILD_COMMIT: Option<&str> = option_env!("HYPRDECK_COMMIT");

/// `0.1.2 (533e43f)` for a release build, `0.1.2+5 (533e43f)` for a build five
/// commits after the 0.1.2 tag (nightly/source), `0.1.2` when the commit is unknown.
pub fn version_string() -> String {
    let version = match COMMITS_SINCE_RELEASE {
        "0" => VERSION.to_owned(),
        n => format!("{VERSION}+{n}"),
    };
    match BUILD_COMMIT {
        Some(c) => format!("{version} ({})", &c[..7]),
        None => version,
    }
}
