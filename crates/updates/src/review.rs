//! The review shown before anything privileged runs: pending repository and
//! AUR updates, unread Arch news, and for each AUR package base its PKGBUILD
//! changes since the last approval plus static security findings.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use hyprdeck_core::cmd;

use crate::apply::{self, AurJob, Plan};
use crate::aur::{self, AurInfo, Foreign};
use crate::check::{self, Tooling};
use crate::news::{self, NewsItem};
use crate::parse::Update;
use crate::pkgbuild::{self, Approval, Baseline, Changes, KnownFinding};
use crate::scan::{self, Finding, Severity};

/// News published within this window counts as unread before the first update.
const FIRST_RUN_NEWS: i64 = 30 * 86_400;

#[derive(Debug, Clone)]
pub struct NewsSection {
    /// Where the news comes from ("Arch Linux news (CachyOS is based on Arch)").
    pub label: String,
    /// Items published after this time are shown.
    pub since: i64,
    pub items: Vec<NewsItem>,
    /// Fetch problem (stale cache or no data).
    pub warning: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AurReview {
    pub pkgbase: String,
    pub updates: Vec<Update>,
    pub info: AurInfo,
    /// Reviewed commit; `None` when the repository could not be fetched.
    pub head: Option<String>,
    pub changes: Option<Changes>,
    pub findings: Vec<Finding>,
    pub source_hosts: Vec<String>,
    /// Warning/Critical findings as scanned (before carry-over), recorded on approval.
    pub risky: Vec<KnownFinding>,
    pub error: Option<String>,
}

impl AurReview {
    pub fn risk(&self) -> Option<Severity> {
        scan::max_severity(&self.findings)
    }

    pub fn approvable(&self) -> bool {
        self.head.is_some() && self.error.is_none()
    }

    /// Approved without asking: no Critical or Warning findings.
    pub fn default_approved(&self) -> bool {
        self.approvable() && self.risk().is_none_or(|r| r < Severity::Warning)
    }

    fn job(&self) -> Option<AurJob> {
        let head = self.head.clone()?;
        Some(AurJob {
            pkgbase: self.pkgbase.clone(),
            names: self.updates.iter().map(|u| u.name.clone()).collect(),
            commit: head.clone(),
            approval: Approval {
                commit: head,
                maintainer: self.info.maintainer.clone(),
                source_hosts: self.source_hosts.clone(),
                approved_at: 0,
                findings: self.risky.clone(),
            },
        })
    }
}

#[derive(Debug, Clone)]
pub struct Review {
    pub tooling: Tooling,
    pub repo: Vec<Update>,
    pub repo_error: Option<String>,
    /// Bytes to download for the repository upgrade (packages not yet cached).
    pub download_size: Option<u64>,
    /// `None` when Arch news doesn't apply to this system.
    pub news: Option<NewsSection>,
    pub aur: Vec<AurReview>,
    pub aur_error: Option<String>,
    pub not_in_aur: Vec<Foreign>,
    /// AUR packages were left out on request.
    pub skip_aur: bool,
}

impl Review {
    /// Unread news that must be acknowledged before updating.
    pub fn unread_news(&self) -> &[NewsItem] {
        self.news.as_ref().map_or(&[], |n| n.items.as_slice())
    }

    pub fn has_updates(&self) -> bool {
        !self.repo.is_empty() || !self.aur.is_empty()
    }

    /// The run for the AUR package bases in `approved` (others are skipped).
    pub fn plan(&self, approved: &BTreeSet<String>) -> Plan {
        let mut plan = Plan {
            fallback: self.tooling.upgrade_command(),
            ..Plan::default()
        };
        for a in &self.aur {
            match (a.job(), &a.error) {
                (_, Some(e)) => plan
                    .skipped
                    .push((a.pkgbase.clone(), format!("could not be reviewed: {e}"))),
                (Some(job), None) if approved.contains(&a.pkgbase) => plan.aur.push(job),
                _ => plan
                    .skipped
                    .push((a.pkgbase.clone(), "not approved in the review".into())),
            }
        }
        plan
    }

    /// Package bases approved by default.
    pub fn default_approved(&self) -> BTreeSet<String> {
        self.aur
            .iter()
            .filter(|a| a.default_approved())
            .map(|a| a.pkgbase.clone())
            .collect()
    }
}

/// Gather everything for the review. Blocking (network, git, checkupdates).
pub fn prepare(skip_aur: bool) -> Review {
    let now = check::now();
    let tooling = Tooling::detect();
    let tracked = check::load_report().map(|r| r.tracked).unwrap_or_default();
    std::thread::scope(|s| {
        let news = s.spawn(move || news_section(now));
        let aur = (!skip_aur && tooling.pacman).then(|| s.spawn(move || aur_reviews(now)));
        let (mut repo, repo_error) = if tooling.pacman {
            match check::repo_updates(&tooling) {
                Ok(r) => (r, None),
                Err(e) => (Vec::new(), Some(e)),
            }
        } else {
            (Vec::new(), Some("pacman is not available".into()))
        };
        check::mark_important(&mut repo, &tracked);
        let download_size = if repo.is_empty() {
            None
        } else {
            download_size()
        };
        let (aur, aur_error, not_in_aur) = match aur.map(|h| h.join()) {
            Some(Ok(Ok((aur, not_in_aur)))) => (aur, None, not_in_aur),
            Some(Ok(Err(e))) => (Vec::new(), Some(e), Vec::new()),
            Some(Err(_)) => (Vec::new(), Some("AUR review panicked".into()), Vec::new()),
            None => (Vec::new(), None, Vec::new()),
        };
        let mut aur: Vec<AurReview> = aur;
        for a in &mut aur {
            check::mark_important(&mut a.updates, &tracked);
        }
        Review {
            news: news.join().unwrap_or(None),
            tooling,
            repo,
            repo_error,
            download_size,
            aur,
            aur_error,
            not_in_aur,
            skip_aur,
        }
    })
}

fn news_section(now: i64) -> Option<NewsSection> {
    let label = news::applies()?;
    let since = apply::last_success().unwrap_or(now - FIRST_RUN_NEWS);
    Some(match news::fetch(now) {
        Ok(n) => NewsSection {
            label,
            since,
            items: news::unread(&n.items, since),
            warning: n.warning,
        },
        Err(e) => NewsSection {
            label,
            since,
            items: Vec::new(),
            warning: Some(format!(
                "Could not load the news ({e:#}); check {} before updating",
                news::FEED_URL.trim_end_matches("feeds/news/")
            )),
        },
    })
}

/// Total download size from the database `checkupdates` just synced (no root needed).
fn download_size() -> Option<u64> {
    let db = check::checkupdates_db()?;
    let out = cmd::output(
        "env",
        [
            "LC_ALL=C",
            "pacman",
            "--dbpath",
            db.to_str()?,
            "-Sup",
            "--print-format",
            "%s",
        ],
    )
    .ok()?;
    if !out.ok() {
        return None;
    }
    out.stdout
        .lines()
        .map(|l| l.trim().parse::<u64>().ok())
        .sum()
}

fn aur_reviews(now: i64) -> Result<(Vec<AurReview>, Vec<Foreign>), String> {
    let listing = aur::list_updates().map_err(|e| format!("{e:#}"))?;
    let mut bases: BTreeMap<String, (Vec<Update>, AurInfo)> = BTreeMap::new();
    for (u, info) in listing.updates.into_iter().zip(listing.info) {
        bases
            .entry(info.pkgbase.clone())
            .or_insert_with(|| (Vec::new(), info))
            .0
            .push(u);
    }
    let approvals = pkgbuild::approvals();
    let reviews = std::thread::scope(|s| {
        let handles: Vec<_> = bases
            .into_iter()
            .map(|(base, (updates, info))| {
                let approval = approvals.get(&base);
                s.spawn(move || review_base(base, updates, info, approval, now))
            })
            .collect();
        handles.into_iter().filter_map(|h| h.join().ok()).collect()
    });
    Ok((reviews, listing.not_in_aur))
}

/// Fetch and analyse one package base. Never executes repository code.
pub fn review_base(
    pkgbase: String,
    updates: Vec<Update>,
    info: AurInfo,
    approval: Option<&Approval>,
    now: i64,
) -> AurReview {
    let mut review = AurReview {
        pkgbase,
        updates,
        info,
        head: None,
        changes: None,
        findings: Vec::new(),
        source_hosts: Vec::new(),
        risky: Vec::new(),
        error: None,
    };
    let result = (|| -> anyhow::Result<()> {
        let head = pkgbuild::sync(&review.pkgbase)?;
        let dir = pkgbuild::repo_dir(&review.pkgbase);
        let files = pkgbuild::scan_files(&dir, &head)?;
        review.source_hosts = files
            .iter()
            .find(|f| f.path == ".SRCINFO")
            .map(|f| scan::source_hosts(&f.text))
            .unwrap_or_default();
        review.changes = Some(pkgbuild::changes(&dir, &head, approval)?);
        let previous = approval.map(Approval::previous);
        let findings = scan::scan(&files, &review.info.meta(), previous.as_ref(), now);
        review.risky = findings
            .iter()
            .filter(|f| f.severity >= Severity::Warning)
            .map(KnownFinding::of)
            .collect();
        review.findings = carry_over(findings, approval);
        review.head = Some(head);
        Ok(())
    })();
    if let Err(e) = result {
        review.error = Some(format!("{e:#}"));
    }
    review
}

/// Warning/Critical findings the user already accepted with the last approved
/// revision (same rule and line text) drop to Info, so they don't block the
/// default approval again. New findings and changed lines keep their severity.
pub fn carry_over(mut findings: Vec<Finding>, approval: Option<&Approval>) -> Vec<Finding> {
    let Some(a) = approval else {
        return findings;
    };
    for f in &mut findings {
        if f.severity >= Severity::Warning && a.findings.contains(&KnownFinding::of(f)) {
            f.severity = Severity::Info;
            f.title = format!(
                "{} — approved before ({})",
                f.title,
                crate::utc_date(a.approved_at)
            );
        }
    }
    findings.sort_by(|x, y| {
        y.severity
            .cmp(&x.severity)
            .then_with(|| x.file.cmp(&y.file))
            .then_with(|| x.line.cmp(&y.line))
    });
    findings
}

/// "12.3 MiB".
pub fn human_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} GiB", b / (1024.0 * 1024.0 * 1024.0))
    } else if b >= 1024.0 * 1024.0 {
        format!("{:.1} MiB", b / (1024.0 * 1024.0))
    } else if b >= 1024.0 {
        format!("{:.0} KiB", b / 1024.0)
    } else {
        format!("{bytes} B")
    }
}

/// "maintainer alice · 14 votes · popularity 1.02 · updated 2026-09-30".
pub fn aur_meta_line(info: &AurInfo) -> String {
    let mut s = match &info.maintainer {
        Some(m) => format!("maintainer {m}"),
        None => "orphaned (no maintainer)".to_owned(),
    };
    let _ = write!(
        s,
        " · {} votes · popularity {:.2} · updated {}",
        info.votes,
        info.popularity,
        crate::utc_date(info.last_modified)
    );
    if let Some(t) = info.out_of_date {
        let _ = write!(s, " · flagged out of date {}", crate::utc_date(t));
    }
    s
}

pub fn baseline_text(c: &Changes) -> String {
    match &c.baseline {
        Baseline::First => "First review: showing the full files".into(),
        Baseline::Since {
            commit,
            approved_at,
        } => {
            let (added, removed) = c.stat();
            format!(
                "Changes since the revision you approved on {} ({}): +{added} −{removed}",
                crate::utc_date(*approved_at),
                crate::selfupdate::short(commit)
            )
        }
        Baseline::Unchanged { commit } => format!(
            "Unchanged since the revision you approved ({})",
            crate::selfupdate::short(commit)
        ),
        Baseline::Missing { commit } => format!(
            "The revision you approved ({}) is no longer in the AUR history: showing the full files",
            crate::selfupdate::short(commit)
        ),
    }
}

/// Text rendering for `hyprdeck updates apply`.
pub fn render(review: &Review, show_diffs: bool) -> String {
    let mut out = String::new();
    if let Some(news) = &review.news {
        if let Some(w) = &news.warning {
            let _ = writeln!(out, "{}: {w}\n", news.label);
        }
        if !news.items.is_empty() {
            let _ = writeln!(out, "== Read before updating: {} ==", news.label);
            for item in &news.items {
                let _ = writeln!(
                    out,
                    "\n* {} ({})\n  {}\n",
                    item.title,
                    crate::utc_date(item.published),
                    item.link
                );
                for line in item.summary.lines() {
                    let _ = writeln!(out, "  {line}");
                }
            }
            out.push('\n');
        }
    }
    match &review.repo_error {
        Some(e) => {
            let _ = writeln!(out, "Repository updates: error: {e}");
        }
        None if review.repo.is_empty() => out.push_str("Repository updates: none\n"),
        None => {
            let size = review.download_size.map_or(String::new(), |s| {
                format!(", {} to download", human_size(s))
            });
            let _ = writeln!(out, "Repository updates ({}{size}):", review.repo.len());
            let width = review.repo.iter().map(|u| u.name.len()).max().unwrap_or(0);
            for u in &review.repo {
                let mark = if u.important { '*' } else { ' ' };
                let _ = writeln!(out, " {mark} {:<width$}  {} -> {}", u.name, u.old, u.new);
            }
        }
    }
    if review.skip_aur {
        out.push_str("AUR updates: skipped (--skip-aur)\n");
    } else if let Some(e) = &review.aur_error {
        let _ = writeln!(out, "AUR updates: error: {e}");
    } else if review.aur.is_empty() {
        out.push_str("AUR updates: none\n");
    }
    for a in &review.aur {
        let names: Vec<String> = a
            .updates
            .iter()
            .map(|u| format!("{} {} -> {}", u.name, u.old, u.new))
            .collect();
        let vcs = if a.updates.iter().any(|u| aur::is_vcs(&u.name)) {
            " (VCS package)"
        } else {
            ""
        };
        let _ = writeln!(out, "\nAUR {}{vcs}: {}", a.pkgbase, names.join(", "));
        let _ = writeln!(out, "  {}", aur_meta_line(&a.info));
        let _ = writeln!(out, "  {}", a.info.page_url());
        if let Some(e) = &a.error {
            let _ = writeln!(out, "  could not be reviewed: {e}");
            continue;
        }
        if a.findings.is_empty() {
            out.push_str("  security scan: no findings\n");
        }
        for f in &a.findings {
            let at = match f.line {
                Some(l) => format!("{}:{l}", f.file),
                None => f.file.clone(),
            };
            let _ = writeln!(
                out,
                "  [{}] {} ({at})\n      {}",
                f.severity.label(),
                f.title,
                f.excerpt
            );
        }
        if let Some(c) = &a.changes {
            let _ = writeln!(out, "  {}", baseline_text(c));
            if show_diffs && !c.text.is_empty() {
                for line in c.text.lines() {
                    let _ = writeln!(out, "    {line}");
                }
            }
        }
    }
    if !review.not_in_aur.is_empty() {
        let names: Vec<&str> = review.not_in_aur.iter().map(|f| f.name.as_str()).collect();
        let _ = writeln!(
            out,
            "\nNot from the AUR (never updated here): {}",
            names.join(", ")
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aur_review(pkgbase: &str, findings: &[Severity], error: bool) -> AurReview {
        AurReview {
            pkgbase: pkgbase.into(),
            updates: vec![Update {
                name: pkgbase.into(),
                old: "1-1".into(),
                new: "2-1".into(),
                important: false,
            }],
            info: AurInfo {
                name: pkgbase.into(),
                pkgbase: pkgbase.into(),
                version: "2-1".into(),
                maintainer: Some("m".into()),
                votes: 100,
                popularity: 2.0,
                out_of_date: None,
                first_submitted: 0,
                last_modified: 0,
                url: None,
            },
            head: (!error).then(|| "abc".into()),
            changes: None,
            findings: findings
                .iter()
                .map(|&severity| Finding {
                    severity,
                    rule: "r".into(),
                    title: "t".into(),
                    file: "PKGBUILD".into(),
                    line: Some(1),
                    excerpt: String::new(),
                })
                .collect(),
            source_hosts: vec!["example.org".into()],
            risky: Vec::new(),
            error: error.then(|| "fetch failed".into()),
        }
    }

    #[test]
    fn default_approval_and_plan() {
        let review = Review {
            tooling: Tooling {
                pacman: true,
                checkupdates: true,
                aur_helper: None,
            },
            repo: Vec::new(),
            repo_error: None,
            download_size: None,
            news: None,
            aur: vec![
                aur_review("clean", &[], false),
                aur_review("info", &[Severity::Info], false),
                aur_review("warn", &[Severity::Info, Severity::Warning], false),
                aur_review("crit", &[Severity::Critical], false),
                aur_review("broken", &[], true),
            ],
            aur_error: None,
            not_in_aur: Vec::new(),
            skip_aur: false,
        };
        let defaults = review.default_approved();
        assert_eq!(
            defaults.iter().map(String::as_str).collect::<Vec<_>>(),
            ["clean", "info"]
        );
        let mut approved = defaults;
        approved.insert("crit".into());
        approved.insert("broken".into());
        let plan = review.plan(&approved);
        assert_eq!(
            plan.aur
                .iter()
                .map(|j| j.pkgbase.as_str())
                .collect::<Vec<_>>(),
            ["clean", "info", "crit"]
        );
        assert_eq!(plan.aur[0].approval.source_hosts, ["example.org"]);
        assert_eq!(plan.aur[0].commit, "abc");
        assert_eq!(
            plan.skipped,
            [
                ("warn".to_owned(), "not approved in the review".to_owned()),
                (
                    "broken".to_owned(),
                    "could not be reviewed: fetch failed".to_owned()
                ),
            ]
        );
        assert_eq!(plan.fallback, "sudo pacman -Syu");
    }

    #[test]
    fn approved_findings_drop_to_info() {
        let finding = |rule: &str, line: usize, excerpt: &str, severity| Finding {
            severity,
            rule: rule.into(),
            title: "t".into(),
            file: "PKGBUILD".into(),
            line: Some(line),
            excerpt: excerpt.into(),
        };
        let approval = Approval {
            commit: "abc".into(),
            maintainer: Some("m".into()),
            source_hosts: Vec::new(),
            approved_at: 1_790_905_267,
            findings: vec![
                KnownFinding::of(&finding(
                    "chmod",
                    40,
                    "chmod 4755  \"$pkgdir/opt/app/chrome-sandbox\"",
                    Severity::Warning,
                )),
                KnownFinding::of(&finding("eval", 12, "eval \"$_old\"", Severity::Warning)),
            ],
        };
        let scanned = vec![
            finding(
                "network-in-build",
                60,
                "curl -o x https://h/x",
                Severity::Warning,
            ),
            // Moved to another line, whitespace differs: still the approved finding.
            finding(
                "chmod",
                52,
                "chmod 4755 \"$pkgdir/opt/app/chrome-sandbox\"",
                Severity::Warning,
            ),
            // Same rule, changed line: keeps its severity.
            finding("eval", 12, "eval \"$_new\"", Severity::Critical),
        ];
        let out = carry_over(scanned.clone(), Some(&approval));
        let summary: Vec<(&str, Severity)> =
            out.iter().map(|f| (f.rule.as_str(), f.severity)).collect();
        assert_eq!(
            summary,
            [
                ("eval", Severity::Critical),
                ("network-in-build", Severity::Warning),
                ("chmod", Severity::Info),
            ]
        );
        assert_eq!(out[2].title, "t — approved before (2026-10-02)");
        assert_eq!(carry_over(scanned.clone(), None), scanned);

        // A package whose only warning was approved before is ticked by default.
        let mut a = aur_review("app", &[], false);
        a.findings = carry_over(vec![scanned[1].clone()], Some(&approval));
        assert!(a.default_approved());
        // Old approvals without findings still load.
        let old: Approval = serde_json::from_str(
            r#"{"commit":"abc","maintainer":null,"source_hosts":[],"approved_at":5}"#,
        )
        .unwrap();
        assert!(old.findings.is_empty());
    }

    #[test]
    fn sizes() {
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(2048), "2 KiB");
        assert_eq!(human_size(5 * 1024 * 1024 + 300_000), "5.3 MiB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }
}
