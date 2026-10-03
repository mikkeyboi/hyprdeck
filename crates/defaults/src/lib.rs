//! hd-defaults: the "Default Apps" page — XDG default applications by
//! category, Hyprland launcher variables, and a lookup for any MIME type or
//! URL scheme — plus `hyprdeck defaults list|set`.

mod advanced;
mod apps;
mod categories;
mod mimeapps;
mod page;
mod picker;
mod terminals;
mod vars;

use anyhow::{Result, bail};
use hyprdeck_core::ui::PageInfo;

use crate::categories::{CATEGORIES, State};

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: "defaults",
        title: "Default Apps",
        icon: "preferences-desktop-apps-symbolic",
        build: page::build,
    }]
}

/// Nothing runs in the background: defaults are read when the page or CLI asks.
pub fn start_background() {}

const USAGE: &str = "usage: hyprdeck defaults list | set <category> <app.desktop>";

pub fn cli(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str)? != "defaults" {
        return None;
    }
    let rest: Vec<&str> = args.iter().skip(1).map(String::as_str).collect();
    Some(match rest.as_slice() {
        ["list"] | [] => list(),
        ["set", key, app] => set(key, app),
        _ => Err(anyhow::anyhow!("{USAGE}\ncategories: {}", keys())),
    })
}

fn keys() -> String {
    CATEGORIES
        .iter()
        .map(|c| c.key)
        .collect::<Vec<_>>()
        .join(", ")
}

fn list() -> Result<()> {
    let reg = categories::registered();
    for cat in CATEGORIES {
        let value = match categories::state(cat, &reg) {
            State::Mime(m) => {
                let current = m.current.as_ref().map_or("-", |a| a.id.as_str()).to_owned();
                if m.fixable().is_empty() {
                    current
                } else {
                    let others: Vec<String> = m
                        .handlers()
                        .iter()
                        .map(|(a, n)| format!("{} ×{n}", a.id))
                        .collect();
                    format!("{current} (mixed: {})", others.join(", "))
                }
            }
            State::Terminal(t) if !t.launcher => {
                format!("- ({} not installed)", terminals::LAUNCHER)
            }
            State::Terminal(t) => t
                .current
                .map_or_else(|| "- (automatic)".to_owned(), |a| a.id),
        };
        println!("{:<10} {:<16} {value}", cat.key, cat.label);
    }
    Ok(())
}

fn set(key: &str, app: &str) -> Result<()> {
    let Some(cat) = categories::by_key(key) else {
        bail!("unknown category {key:?}; one of: {}", keys());
    };
    categories::set(cat, app)?;
    println!("{} → {}", cat.label, apps::desktop_id(app));
    Ok(())
}
