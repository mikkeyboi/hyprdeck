//! hd-system: "Sleep & Wake" (resume guard, diagnosis) and "Tweaks" (game
//! mode, config health, OpenLinkHub tuning, Hyprland log).

mod diagnose;
mod gamemode;
mod guard;
mod health;
mod journal;
mod nvidia;
mod openlinkhub;
mod settings;
mod shortcut;
mod sleep_page;
mod tweaks_page;
mod widgets;

use anyhow::{Result, bail};
use hyprdeck_core::ui::PageInfo;
use hyprdeck_core::{notify, rt};

use crate::settings::RescuePolicy;

pub fn pages() -> Vec<PageInfo> {
    vec![
        PageInfo {
            id: "sleep",
            title: "Sleep & Wake",
            icon: "weather-clear-night-symbolic",
            build: sleep_page::build,
        },
        PageInfo {
            id: "tweaks",
            title: "Tweaks",
            icon: "applications-engineering-symbolic",
            build: tweaks_page::build,
        },
    ]
}

/// Resume guard (logind watcher) and the game-mode tray item.
pub fn start_background() {
    guard::start();
    gamemode::start();
}

const SYSTEM_USAGE: &str = "usage: hyprdeck system diagnose | resume-log";
const TWEAKS_USAGE: &str = "usage: hyprdeck tweaks gamemode on|off|toggle|status";

pub fn cli(args: &[String]) -> Option<Result<()>> {
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    match args.first().map(String::as_str)? {
        "system" => Some(system_cli(&rest)),
        "tweaks" => Some(tweaks_cli(&rest)),
        _ => None,
    }
}

fn system_cli(args: &[&str]) -> Result<()> {
    match args {
        ["diagnose"] => {
            print!("{}", diagnose::run().to_text());
            Ok(())
        }
        ["resume-log"] => {
            let logs = guard::recent();
            let Some(latest) = logs.first() else {
                println!(
                    "No wake has been recorded yet in {}.",
                    guard::dir().display()
                );
                return Ok(());
            };
            println!("{}", latest.path.display());
            print_log_summary(latest);
            if logs.len() > 1 {
                println!(
                    "\n{} earlier report(s) in {}",
                    logs.len() - 1,
                    guard::dir().display()
                );
            }
            Ok(())
        }
        // Undocumented dev path: run the resume guard as if the system had just
        // slept and woken (PrepareForSleep true → false) without suspending.
        ["simulate-resume", opts @ ..] => {
            let mut policy = settings::load()?.resume.rescue;
            let mut delay = 2;
            let mut it = opts.iter();
            while let Some(o) = it.next() {
                match *o {
                    "--policy" => {
                        policy = it
                            .next()
                            .and_then(|p| RescuePolicy::parse(p))
                            .ok_or_else(|| anyhow::anyhow!("--policy needs never|problem|always"))?
                    }
                    "--delay" => {
                        delay = it
                            .next()
                            .and_then(|d| d.parse().ok())
                            .ok_or_else(|| anyhow::anyhow!("--delay needs seconds"))?
                    }
                    other => bail!("unknown option {other}"),
                }
            }
            let report = guard::simulate(policy, delay)?;
            println!("{}", report.path.display());
            let text = std::fs::read_to_string(&report.path)?;
            print_log_summary(&guard::parse_header(report.path.clone(), &text));
            if let Some((summary, body)) = report.notification() {
                rt::runtime().block_on(notify::notify(
                    notify::Category::System,
                    report.notification_severity(),
                    &summary,
                    &body,
                ))?;
            }
            Ok(())
        }
        _ => bail!("{SYSTEM_USAGE}"),
    }
}

fn print_log_summary(log: &guard::WakeLog) {
    if let Some(s) = &log.slept {
        println!("  slept:    {s}");
    }
    println!(
        "  woke:     {}{}",
        log.woke,
        if log.simulated { " (simulated)" } else { "" }
    );
    if log.problems.is_empty() {
        println!("  problems: none");
    }
    for p in &log.problems {
        println!("  problem:  {p}");
    }
    println!("  rescue:   {}", log.rescue);
}

fn tweaks_cli(args: &[&str]) -> Result<()> {
    match args {
        ["gamemode", "on"] => gamemode::set(true).map(|()| println!("Game mode on")),
        ["gamemode", "off"] => gamemode::set(false).map(|()| println!("Game mode off")),
        ["gamemode", "toggle"] => {
            gamemode::toggle().map(|on| println!("Game mode {}", if on { "on" } else { "off" }))
        }
        ["gamemode", "status"] | ["gamemode"] => {
            gamemode::is_on().map(|on| println!("Game mode {}", if on { "on" } else { "off" }))
        }
        _ => bail!("{TWEAKS_USAGE}"),
    }
}
