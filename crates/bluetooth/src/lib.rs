use anyhow::Result;
use hyprdeck_core::ui::PageInfo;

mod agent;
mod bt;
mod cli;
mod info;
mod page;
mod tray;

pub(crate) const PAGE_ID: &str = "bluetooth";

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: PAGE_ID,
        title: "Bluetooth",
        icon: "bluetooth-symbolic",
        build: page::build,
    }]
}

/// Registers the tray submenu and its BlueZ watcher. The pairing agent is not
/// registered here: it only exists while a pairing started from the page runs
/// (see `agent` module docs).
pub fn start_background() {
    tray::start();
}

/// `hyprdeck bluetooth list | connect <dev> | disconnect <dev>`.
pub fn cli(args: &[String]) -> Option<Result<()>> {
    (args.first()? == "bluetooth").then(|| cli::run(&args[1..]))
}
