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
