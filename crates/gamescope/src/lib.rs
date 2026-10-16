//! Nested Gamescope launcher for Steam Big Picture, with display-derived settings.

pub mod config;
pub mod engine;
mod page;
pub mod startup;

use anyhow::{Result, anyhow};
use hyprdeck_core::ui::PageInfo;

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: "gamescope",
        title: "Gaming",
        icon: "input-gaming-symbolic",
        build: page::build,
    }]
}

/// If "Launch Steam Big Picture at login" is enabled, ensure the login entry uses
/// an absolute executable path (so `systemd-xdg-autostart-generator` never skips it
/// when `~/.local/bin` is absent from the early systemd user PATH) and launch the
/// saved session if `xdg-desktop-autostart.target` skipped it during login.
pub fn start_background() {
    static STARTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if STARTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    hyprdeck_core::rt::spawn(async {
        let _ = tokio::task::spawn_blocking(|| {
            if startup::enabled().unwrap_or(false) {
                // Upgrade legacy bare `Exec=hyprdeck gamescope launch` entries to absolute path.
                let _ = startup::set_enabled(true);
                let settings = config::Settings::load().unwrap_or_default();
                if let Err(err) = engine::launch(&settings, false) {
                    tracing::warn!("login gamescope launch: {err:#}");
                }
            }
        })
        .await;
    });
}

const USAGE: &str =
    "usage: hyprdeck gamescope <plan | launch [--restart-steam] | autostart <status | on | off>>";

/// Headless launcher for keyboard shortcuts; uses the same saved settings as the UI.
pub fn cli(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("gamescope") {
        return None;
    }
    Some(
        match args[1..]
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
            .as_slice()
        {
            ["plan"] => print_plan(),
            ["launch"] => launch(false),
            ["launch", "--restart-steam"] => launch(true),
            ["autostart", "status"] => {
                startup::enabled().map(|on| println!("{}", if on { "on" } else { "off" }))
            }
            ["autostart", "on"] => startup::set_enabled(true),
            ["autostart", "off"] => startup::set_enabled(false),
            _ => Err(anyhow!(USAGE)),
        },
    )
}

fn print_plan() -> Result<()> {
    let settings = config::Settings::load()?;
    let snapshot = engine::snapshot()?;
    let plan = engine::plan(&settings, &snapshot)?;
    println!(
        "{}: {}×{} at {:.2} Hz",
        plan.output, plan.width, plan.height, plan.refresh_rate
    );
    println!("{}", plan.command_line());
    for warning in &plan.warnings {
        eprintln!("Note: {warning}");
    }
    Ok(())
}

fn launch(restart_steam: bool) -> Result<()> {
    match engine::launch(&config::Settings::load()?, restart_steam)? {
        engine::LaunchOutcome::Launched => println!("Steam Big Picture started inside Gamescope."),
        engine::LaunchOutcome::Focused => println!("Opened the existing Gamescope Steam session."),
    }
    Ok(())
}
