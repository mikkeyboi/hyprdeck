//! Updates: pending repo/AUR upgrades, upstream releases of detected desktop
//! components (Hyprland, shells, bars, …) and hyprdeck's own version, with
//! background checks, a tray entry and an in-app system update (review →
//! one password prompt → progress → summary).

mod apply;
mod aur;
mod check;
mod dialog;
mod github;
mod helper;
mod news;
mod page;
mod parse;
mod pkgbuild;
mod progress;
mod review;
mod scan;
mod selfstate;
mod selfupdate;
mod state;
mod vercmp;

use std::fmt::Write as _;
use std::io::Write as _;

use anyhow::{Result, anyhow, bail};
use hyprdeck_core::ui::PageInfo;
use selfupdate::{Channel, Mode, SelfCheck};

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
        Some("self") => self_cli(&args[2..]),
        Some("apply") => apply_cli(&args[2..]),
        Some("root-helper") => helper::main(&args[2..]),
        _ => Err(anyhow!(
            "usage: hyprdeck updates <check|json|apply [--yes] [--skip-aur]|self [check|install] [--channel stable|nightly]>"
        )),
    })
}

const APPLY_USAGE: &str = "usage: hyprdeck updates apply [--yes] [--skip-aur]";

/// Ask on the terminal; anything but y/yes (including EOF) is "no".
fn ask(prompt: &str) -> bool {
    print!("{prompt} [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer).is_ok()
        && matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// `hyprdeck updates apply`: the review as text, confirmation, then the same
/// engine as the app with textual progress.
fn apply_cli(args: &[String]) -> Result<()> {
    let (mut yes, mut skip_aur) = (false, false);
    for a in args {
        match a.as_str() {
            "--yes" | "-y" => yes = true,
            "--skip-aur" => skip_aur = true,
            _ => bail!(APPLY_USAGE),
        }
    }
    let _lock = apply::lock()?;
    println!("Preparing the review (checking updates, news and AUR repositories)…");
    let review = review::prepare(skip_aur);
    println!();
    print!("{}", review::render(&review, true));
    if review.repo_error.is_some() && review.repo.is_empty() && review.aur.is_empty() {
        bail!("could not determine pending updates");
    }
    if !review.has_updates() {
        println!("\nNothing to update.");
        return Ok(());
    }
    let mut approved = review.default_approved();
    println!();
    for a in review.aur.iter().filter(|a| a.approvable()) {
        let Some(risk) = a.risk().filter(|r| *r >= scan::Severity::Warning) else {
            continue;
        };
        if yes {
            println!(
                "Skipping AUR {} ({} findings; --yes only installs packages without warnings)",
                a.pkgbase,
                risk.label()
            );
            continue;
        }
        let include = ask(&format!(
            "Build AUR {} despite {} findings?",
            a.pkgbase,
            risk.label()
        )) && (risk < scan::Severity::Critical
            || ask(&format!(
                "Critical findings can mean {} is malicious. Really build and install it?",
                a.pkgbase
            )));
        if include {
            approved.insert(a.pkgbase.clone());
        }
    }
    let plan = review.plan(&approved);
    for (name, reason) in &plan.skipped {
        println!("Skipping AUR {name}: {reason}");
    }
    if !yes {
        if !review.unread_news().is_empty() && !ask("Have you read the news above?") {
            bail!("cancelled: read the news first");
        }
        let what = format!(
            "{} repository and {} AUR package update(s)",
            review.repo.len(),
            plan.aur.len()
        );
        if !ask(&format!(
            "Proceed with {what}? pacman asks for your password via polkit."
        )) {
            bail!("cancelled");
        }
    }
    let mut steps: Vec<apply::Step> = Vec::new();
    let summary = apply::run(
        &plan,
        &apply::RealSystem,
        &std::sync::atomic::AtomicBool::new(false),
        &mut |event| match event {
            apply::Event::Steps(s) => steps = s,
            apply::Event::Step {
                index,
                state,
                detail,
            } => {
                let Some(step) = steps.get(index) else { return };
                let mark = match state {
                    apply::StepState::Running => "…",
                    apply::StepState::Done => "✓",
                    apply::StepState::Failed => "✗",
                    apply::StepState::Skipped => "-",
                    apply::StepState::Pending => return,
                };
                match detail.filter(|d| !d.is_empty() && state != apply::StepState::Running) {
                    Some(d) => println!("==> [{mark}] {}: {d}", step.title),
                    None if state == apply::StepState::Running && step.state == state => {}
                    None => println!("==> [{mark}] {}", step.title),
                }
                steps[index].state = state;
            }
            apply::Event::Phase(p) if p.starts_with("Waiting") => println!("==> {p}"),
            apply::Event::Log(l) => println!("{l}"),
            _ => {}
        },
    );
    apply::record(&summary, check::now());
    print!("\n{}", render_summary(&summary));
    match summary.outcome {
        apply::Outcome::Success | apply::Outcome::Partial => Ok(()),
        apply::Outcome::Cancelled => bail!("cancelled"),
        apply::Outcome::Failed => bail!("system update failed"),
    }
}

fn render_summary(s: &apply::Summary) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", s.headline);
    if let Some(m) = &s.message {
        let _ = writeln!(out, "  {m}");
    }
    let changed: Vec<&str> = s
        .changes
        .iter()
        .filter(|(op, _)| *op != progress::Op::Remove)
        .map(|(_, n)| n.as_str())
        .collect();
    if !changed.is_empty() {
        let _ = writeln!(out, "Updated ({}): {}", changed.len(), changed.join(", "));
    }
    for (name, reason) in &s.failed {
        let _ = writeln!(out, "Failed: {name}: {reason}");
    }
    for (name, reason) in &s.skipped {
        let _ = writeln!(out, "Skipped: {name}: {reason}");
    }
    if !s.restart.is_empty() {
        let _ = writeln!(
            out,
            "Restart recommended (updated: {})",
            s.restart.join(", ")
        );
    }
    if !s.pacnew.is_empty() {
        let _ = writeln!(out, "Configuration files to merge (pacdiff):");
        for p in &s.pacnew {
            let new = if s.new_pacnew.contains(p) {
                " (new)"
            } else {
                ""
            };
            let _ = writeln!(out, "  {p}{new}");
        }
    }
    if let Some(cmd) = &s.fallback {
        let _ = writeln!(out, "Run it in a terminal instead: {cmd}");
    }
    if let Some(p) = &s.log_path {
        let _ = writeln!(out, "Log: {}", p.display());
    }
    out
}

/// Exit status of `hyprdeck updates self check` when an update is available.
const EXIT_UPDATE_AVAILABLE: i32 = 10;
const SELF_USAGE: &str = "usage: hyprdeck updates self [check|install] [--channel stable|nightly]";

fn self_cli(args: &[String]) -> Result<()> {
    let settings = state::settings();
    let mode = selfupdate::mode();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let (command, channel) = match args[..] {
        [] => (None, None),
        ["--channel", c] => (None, Some(c)),
        [cmd] => (Some(cmd), None),
        [cmd, "--channel", c] => (Some(cmd), Some(c)),
        _ => bail!(SELF_USAGE),
    };
    let channel_given = channel.is_some();
    let channel = match channel {
        Some(c) => Channel::parse(c).ok_or_else(|| anyhow!("unknown channel {c:?}"))?,
        None => settings.self_update_channel,
    };
    match command {
        None | Some("check") => {
            let force = command.is_some();
            let now = check::now();
            let result = SelfCheck::run(&mode, channel, now, force);
            print!(
                "{}",
                render_self(&mode, settings.self_update_policy, &result, now)
            );
            let check = result?;
            if force && check.offer().is_some() {
                use std::io::Write as _;
                std::io::stdout().flush()?;
                std::process::exit(EXIT_UPDATE_AVAILABLE);
            }
            Ok(())
        }
        Some("install") => self_install(&mode, channel, channel_given),
        _ => bail!(SELF_USAGE),
    }
}

struct CliProgress;

impl selfupdate::Progress for CliProgress {
    fn step(&self, msg: &str) {
        println!("{msg}");
    }

    fn log(&self, text: &str) {
        print!("{text}");
    }
}

fn self_install(mode: &Mode, channel: Channel, channel_given: bool) -> Result<()> {
    let _lock = selfupdate::lock()?;
    let applied = match mode {
        Mode::AppImage(path) => {
            println!("Checking the {} channel…", channel.as_str());
            let c = selfupdate::check_appimage(path, channel, check::now(), true)?;
            let Some(label) = c.target_label() else {
                bail!(
                    "{}",
                    match channel {
                        Channel::Stable => "no releases published yet",
                        Channel::Nightly => "no nightly build published yet",
                    }
                );
            };
            if !c.available {
                println!(
                    "Hyprdeck {} is up to date (latest on {}: {label})",
                    hyprdeck_core::version_string(),
                    channel.as_str()
                );
                return Ok(());
            }
            selfupdate::apply_appimage(&c, false, &CliProgress)?
        }
        Mode::Source(dir) => {
            if channel_given {
                println!(
                    "Note: source installs follow their branch's upstream; --channel is ignored"
                );
            }
            let c = selfupdate::check_source(dir, true, hyprdeck_core::BUILD_COMMIT)?;
            if !c.plan.available() && c.plan.blocker.is_none() {
                println!("Hyprdeck {} is up to date", hyprdeck_core::version_string());
                return Ok(());
            }
            selfupdate::apply_source(dir, &CliProgress)?
        }
        Mode::Installed => bail!(
            "this installation can't update itself; get new versions from {}",
            selfupdate::RELEASES_URL
        ),
    };
    println!("{}", applied.message);
    if applied.restart.service {
        match selfupdate::restart_service()? {
            Some(_) => println!("Restarted hyprdeck.service"),
            None => println!("Start hyprdeck again to use the new version"),
        }
    } else if matches!(mode, Mode::Source(_)) {
        println!("install.sh restarted hyprdeck.service if it was running");
    } else {
        println!(
            "hyprdeck.service doesn't run this AppImage; start it again to use the new version"
        );
    }
    Ok(())
}

fn render_self(
    mode: &Mode,
    policy: selfupdate::Policy,
    result: &Result<SelfCheck>,
    now: i64,
) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "Hyprdeck {}", hyprdeck_core::version_string());
    let _ = writeln!(out, "Install:   {}", mode.describe());
    let _ = writeln!(out, "Policy:    {}", policy.label());
    let check = match result {
        Ok(c) => c,
        Err(e) => {
            let _ = writeln!(out, "Status:    check failed: {e:#}");
            return out;
        }
    };
    let offer = check.offer();
    match check {
        SelfCheck::Installed => {
            let _ = writeln!(
                out,
                "Status:    can't update itself; releases: {}",
                selfupdate::RELEASES_URL
            );
        }
        SelfCheck::AppImage(c) => {
            let _ = writeln!(out, "Channel:   {}", c.channel.as_str());
            let status = match (&c.release, &offer) {
                (None, _) if c.channel == Channel::Nightly => {
                    "no nightly build published yet".to_owned()
                }
                (None, _) => "no releases published yet".to_owned(),
                (Some(_), Some(o)) => o.title.clone(),
                (Some(r), None) => format!(
                    "up to date (latest: {})",
                    c.target_label().unwrap_or_else(|| r.tag.clone())
                ),
            };
            let _ = writeln!(
                out,
                "Status:    {status} · checked {}",
                ago(now - c.fetched_at)
            );
            if let Some(w) = &c.warning {
                let _ = writeln!(out, "Warning:   {w}");
            }
            if let (Some(o), Some(r)) = (&offer, &c.release) {
                let _ = writeln!(out, "           {}  {}", o.detail, r.url);
            }
        }
        SelfCheck::Source(c) => {
            if let Some(g) = &c.git {
                let _ = writeln!(
                    out,
                    "Branch:    {} → {}",
                    g.branch.as_deref().unwrap_or("(detached)"),
                    g.upstream.as_deref().unwrap_or("(no upstream)")
                );
            }
            if let Some(h) = &c.head {
                let _ = writeln!(
                    out,
                    "Checkout:  {} {}",
                    selfupdate::short(&h.hash),
                    h.subject
                );
            }
            let status = match &offer {
                Some(o) => o.title.clone(),
                None if c.plan.blocker.is_some() => "can't follow upstream".into(),
                None => "up to date".into(),
            };
            let _ = writeln!(out, "Status:    {status}");
            if let Some(b) = &c.plan.blocker {
                let _ = writeln!(out, "Blocked:   {}", b.message());
            }
            if let Some(e) = &c.fetch_error {
                let _ = writeln!(out, "Warning:   {e}");
            }
            for commit in &c.incoming {
                let _ = writeln!(
                    out,
                    "  {} {}",
                    selfupdate::short(&commit.hash),
                    commit.subject
                );
            }
        }
    }
    out
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
        section(&mut out, "AUR updates", &report.aur, &report.aur_error);
        if report.important().next().is_some() {
            out.push_str("  (* = important package)\n");
        }
        if !report.not_in_aur.is_empty() {
            let names: Vec<String> = report
                .not_in_aur
                .iter()
                .map(|f| format!("{} {}", f.name, f.version))
                .collect();
            let _ = writeln!(
                out,
                "Not from the AUR (foreign, never updated): {}",
                names.join(", ")
            );
        }
        let _ = writeln!(out, "Note: {}", aur::VCS_NOTE);
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
pub(crate) fn utc_date(secs: i64) -> String {
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
