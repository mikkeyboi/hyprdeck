//! Updates: pending repo/AUR upgrades, upstream releases of detected desktop
//! components (Hyprland, shells, bars, …) and hyprdeck's own version, with
//! background checks and a tray entry.

mod aur;
mod check;
mod github;
mod page;
mod parse;
mod selfupdate;
mod state;
mod vercmp;

use std::fmt::Write as _;

use anyhow::{Result, anyhow};
use hyprdeck_core::ui::PageInfo;

pub fn pages() -> Vec<PageInfo> {
    vec![PageInfo {
        id: "updates",
        title: "Updates",
        icon: "software-update-available-symbolic",
        build: page::build,
    }]
}

pub fn start_background() {
    state::start();
}

pub fn cli(args: &[String]) -> Option<Result<()>> {
    if args.first().map(String::as_str) != Some("updates") {
        return None;
    }
    Some(match args.get(1).map(String::as_str) {
        Some("check") => {
            let report = check::run_check();
            save(&report);
            print!("{}", render_text(&report, check::now()));
            if report.failed() {
                Err(anyhow!("update check incomplete"))
            } else {
                Ok(())
            }
        }
        Some("json") => {
            let report = check::run_check();
            save(&report);
            serde_json::to_string_pretty(&report)
                .map(|s| println!("{s}"))
                .map_err(Into::into)
        }
        _ => Err(anyhow!("usage: hyprdeck updates <check|json>")),
    })
}

fn save(report: &check::Report) {
    if let Err(e) = check::save_report(report) {
        tracing::warn!("saving update report: {e:#}");
    }
}

fn render_text(report: &check::Report, now: i64) -> String {
    let mut out = String::new();
    let section =
        |out: &mut String, title: &str, list: &[parse::Update], err: &Option<String>| match err {
            Some(e) => {
                let _ = writeln!(out, "{title}: error: {e}");
            }
            None if list.is_empty() => {
                let _ = writeln!(out, "{title}: none");
            }
            None => {
                let _ = writeln!(out, "{title} ({}):", list.len());
                let width = list.iter().map(|u| u.name.len()).max().unwrap_or(0);
                for u in list {
                    let mark = if u.important { '*' } else { ' ' };
                    let _ = writeln!(out, " {mark} {:<width$}  {} -> {}", u.name, u.old, u.new);
                }
            }
        };
    let tooling = &report.tooling;
    if tooling.pacman {
        section(
            &mut out,
            "Repository updates",
            &report.repo,
            &report.repo_error,
        );
        match &tooling.aur_helper {
            Some(helper) => section(
                &mut out,
                &format!("AUR updates ({helper})"),
                &report.aur,
                &report.aur_error,
            ),
            None => out.push_str("AUR updates: not checked (no AUR helper; install paru or yay)\n"),
        }
        if report.important().next().is_some() {
            out.push_str("  (* = important package)\n");
        }
    } else {
        out.push_str("System packages: not checked (needs an Arch-based distro with pacman)\n");
    }
    if report.tracked.is_empty() {
        out.push_str("\nTracked upstream: no known desktop components detected\n");
    } else {
        out.push_str("\nTracked upstream:\n");
    }
    for t in &report.tracked {
        let installed = match (&t.installed, &t.binary) {
            (Some(i), _) => format!(
                "{} {} ({})",
                i.package,
                i.version,
                i.repo.as_deref().unwrap_or("AUR/foreign")
            ),
            (None, Some(path)) => format!("{path} (not managed by pacman)"),
            (None, None) => "unknown".into(),
        };
        let repo = t.repo.as_ref().map_or_else(
            || "not in repos".into(),
            |r| format!("{} in {}", r.version, r.repo),
        );
        let upstream = match &t.upstream {
            Some(r) => format!(
                "{} ({}, {})",
                r.tag,
                utc_date(r.published_at),
                days_ago(now - r.published_at)
            ),
            None => "unknown".into(),
        };
        let _ = writeln!(out, "  {}", t.title);
        let _ = writeln!(out, "    installed: {installed}");
        if tooling.pacman {
            let _ = writeln!(out, "    repos:     {repo}");
        }
        let _ = writeln!(
            out,
            "    upstream:  {upstream}  {}",
            t.upstream.as_ref().map_or("", |r| r.url.as_str())
        );
        if let Some(e) = &t.upstream_error {
            let _ = writeln!(out, "    warning:   {e}");
        }
        let _ = writeln!(out, "    status:    {}", t.status(now).text);
    }
    out
}

/// `YYYY-MM-DD` (UTC) for Unix seconds.
fn utc_date(secs: i64) -> String {
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// "just now", "5 min ago", "3 h ago", "2 days ago".
pub(crate) fn ago(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{} min ago", s / 60),
        s if s < 86_400 => format!("{} h ago", s / 3600),
        s => days_ago(s),
    }
}

/// Whole days: "today", "yesterday", "N days ago".
pub(crate) fn days_ago(secs: i64) -> String {
    match secs.max(0) / 86_400 {
        0 => "today".into(),
        1 => "yesterday".into(),
        n => format!("{n} days ago"),
    }
}

/// "45 min", "2 h 5 min".
pub(crate) fn duration(secs: i64) -> String {
    let mins = (secs.max(0) + 59) / 60;
    if mins < 60 {
        format!("{mins} min")
    } else {
        format!("{} h {} min", mins / 60, mins % 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_relative_times() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(1_790_905_267), "2026-10-02");
        assert_eq!(utc_date(951_825_600), "2000-02-29");
        assert_eq!(ago(30), "just now");
        assert_eq!(ago(300), "5 min ago");
        assert_eq!(ago(7200), "2 h ago");
        assert_eq!(ago(86_400 + 5), "yesterday");
        assert_eq!(days_ago(3 * 86_400), "3 days ago");
        assert_eq!(duration(61), "2 min");
        assert_eq!(duration(3 * 3600 + 300), "3 h 5 min");
    }
}
