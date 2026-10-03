//! Audio: simultaneous output across several devices (module-combine-sink), per-app
//! routing rules and volumes.

mod cli;
pub mod config;
mod daemon;
pub mod engine;
pub mod pw;
mod tray;
mod ui;

use hyprdeck_core::ui::PageInfo;

/// Sidebar id of the Audio page.
const PAGE_ID: &str = "audio";

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: PAGE_ID,
        title: "Audio",
        icon: "audio-speakers-symbolic",
        build: ui::audio_page,
    }]
}

/// Starts the PipeWire watcher (routing rules, restore of the simultaneous output)
/// and registers the "Audio" tray submenu. Returns immediately.
pub fn start_background() {
    daemon::start();
    tray::register();
}

/// `hyprdeck audio …`
pub fn cli(args: &[String]) -> Option<anyhow::Result<()>> {
    (args.first().map(String::as_str) == Some("audio")).then(|| cli::run(&args[1..]))
}
