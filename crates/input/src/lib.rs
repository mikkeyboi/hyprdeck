//! hyprdeck input: global input options, per-device overrides and keybinds.

mod actions;
mod binds;
mod devices;
mod editor;
mod inputs;
mod keys;
mod noctalia;
mod page_binds;
mod page_input;
mod widgets;
mod xkb;

use hyprdeck_core::rt;
use hyprdeck_core::ui::PageInfo;

pub fn pages() -> Vec<PageInfo> {
    vec![
        PageInfo {
            id: "input",
            title: "Input Devices",
            icon: "input-keyboard-symbolic",
            build: page_input::build,
        },
        PageInfo {
            id: "keybinds",
            title: "Keybinds",
            icon: "preferences-desktop-keyboard-shortcuts-symbolic",
            build: page_binds::build,
        },
    ]
}

/// Leave a key-capture submap a previous (crashed) hyprdeck left active, and
/// look up optional tools off the main thread.
pub fn start_background() {
    rt::spawn(async {
        rt::blocking(noctalia::installed).await;
        if let Err(e) = rt::blocking(keys::reset_leftover).await {
            tracing::debug!("capture submap check: {e:#}");
        }
    });
}

const USAGE: &str = "usage: hyprdeck input binds | devices";

pub fn cli(args: &[String]) -> Option<anyhow::Result<()>> {
    if args.first().map(String::as_str) != Some("input") {
        return None;
    }
    Some(match args.get(1).map(String::as_str) {
        Some("binds") => binds::print_binds(),
        Some("devices") => inputs::print_devices(),
        _ => Err(anyhow::anyhow!(USAGE)),
    })
}
