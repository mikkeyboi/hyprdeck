//! Startup Apps: XDG autostart entries, systemd user services and Hyprland
//! login commands in one place.

mod cleanup;
mod desktop;
mod entries;
mod hypr;
mod ui;
mod units;
mod xdg;

use anyhow::{Result, bail};
use hyprdeck_core::ui::PageInfo;

use entries::{Entry, Toggled};

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: "startup",
        title: "Startup Apps",
        icon: "system-run-symbolic",
        build: ui::build,
    }]
}

/// Nothing runs in the background; the page reads state on demand.
pub fn start_background() {}

const USAGE: &str = "usage: hyprdeck startup list\n       hyprdeck startup enable <id>\n       hyprdeck startup disable <id> [--force]\n\n<id> is a desktop file basename (steam.desktop), a unit name (foo.service) or hypr:<program>.";

pub fn cli(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("startup") {
        return None;
    }
    let rest: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    Some(match rest.as_slice() {
        [] | ["list"] => list(),
        ["enable", id] => toggle(id, true, false),
        ["disable", id] => toggle(id, false, false),
        ["disable", id, "--force"] | ["disable", "--force", id] => toggle(id, false, true),
        ["-h" | "--help" | "help"] => {
            println!("{USAGE}");
            Ok(())
        }
        _ => Err(anyhow::anyhow!("{USAGE}")),
    })
}

fn list() -> Result<()> {
    let snap = entries::load();
    let rows: Vec<[String; 5]> = snap
        .entries
        .iter()
        .map(|e| {
            let enabled = match e.toggle_blocker() {
                Some(_) if !matches!(e, Entry::App(..)) => "-".to_owned(),
                _ => if e.enabled() { "yes" } else { "no" }.to_owned(),
            };
            [
                e.source_label().to_owned(),
                e.id(),
                e.name(),
                enabled,
                e.state().label().to_lowercase(),
            ]
        })
        .collect();
    let header = ["SOURCE", "ID", "NAME", "ENABLED", "STATE"].map(str::to_owned);
    let mut widths = header.clone().map(|h| h.chars().count());
    for r in &rows {
        for (w, c) in widths.iter_mut().zip(r) {
            *w = (*w).max(c.chars().count());
        }
    }
    for r in std::iter::once(&header).chain(&rows) {
        let line: Vec<String> = r
            .iter()
            .zip(widths)
            .map(|(c, w)| format!("{c:<w$}"))
            .collect();
        println!("{}", line.join("  ").trim_end());
    }
    for e in &snap.errors {
        eprintln!("warning: {e}");
    }
    Ok(())
}

fn toggle(id: &str, on: bool, force: bool) -> Result<()> {
    let snap = entries::load();
    let entry = entries::find(&snap, id)?;
    if !on
        && let Some(warning) = entry.essential()
        && !force
    {
        bail!(
            "{} is essential: {warning}\nRe-run with --force to disable it anyway.",
            entry.name()
        );
    }
    if let Some(why) = entry.toggle_blocker() {
        if matches!(entry, Entry::App(..)) {
            eprintln!("note: {why}; the setting is saved but has no effect on this desktop");
        } else {
            bail!("{} can't be toggled: {why}", entry.id());
        }
    }
    match entry.set_enabled(on)? {
        Toggled::Done => {
            println!(
                "{} {} (takes effect at next login)",
                if on { "Enabled" } else { "Disabled" },
                entry.id()
            );
            Ok(())
        }
        Toggled::NeedsMask => bail!(
            "{} is enabled system-wide (/etc/systemd/user); run `systemctl --user mask {}` to stop it for your user",
            entry.id(),
            entry.id()
        ),
    }
}
