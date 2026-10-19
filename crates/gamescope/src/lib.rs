//! Nested Gamescope launcher for Steam Big Picture, with display-derived settings.

pub mod config;
pub mod engine;
pub mod overlay;
mod overlay_controls;
mod overlay_page;
mod overlay_preview;
mod page;
pub mod proton;
pub mod proton_cpu;
mod proton_page;
pub mod proton_repair;
pub mod recovery;
pub mod startup;
pub mod steam;
pub mod steam_assign;
mod vdf_edit;

use anyhow::{Result, anyhow};
use hyprdeck_core::ui::PageInfo;

pub fn pages() -> Vec<PageInfo> {
    vec![
        PageInfo {
            id: "gamescope",
            title: "Gaming",
            icon: "input-gaming-symbolic",
            build: page::build,
        },
        PageInfo {
            id: "gamescope-overlay",
            title: "Performance overlay",
            icon: "utilities-system-monitor-symbolic",
            build: overlay_page::build,
        },
        PageInfo {
            id: "gamescope-proton",
            title: "Compatibility tools",
            icon: "package-x-generic-symbolic",
            build: proton_page::build,
        },
    ]
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
            let result = (|| -> Result<()> {
                if !startup::enabled()? {
                    return Ok(());
                }
                // Migrate only Exec; retain desktop restrictions and user metadata.
                startup::set_enabled(true)?;
                if !startup::launch_allowed()? {
                    return Ok(());
                }
                let settings = config::Settings::load()?;
                engine::launch(&settings, false)?;
                Ok(())
            })();
            if let Err(err) = result {
                tracing::warn!("login gamescope launch: {err:#}");
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
    let plan = engine::plan(&settings, &snapshot)?.with_overlay()?;
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
