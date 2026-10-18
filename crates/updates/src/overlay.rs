//! MangoHUD package status embedded in the performance overlay preferences.
//! Package writes remain exclusively in the existing reviewed system updater.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use adw::prelude::*;
use gtk::glib;
use hyprdeck_core::{cmd, rt, ui, ui::Ctx};

use crate::check::{self, Report};
use crate::state::{self, State};

const UPSTREAM: &str = "flightlessmango/MangoHud";
const PACKAGES: &[&str] = &[
    "mangohud",
    "mangohud-git",
    "mangohud-common-git",
    "mangoapp",
    "mangoapp-git",
    "lib32-mangohud",
    "lib32-mangohud-git",
    "lib32-mangoapp",
    "lib32-mangoapp-git",
];

struct Binary {
    name: &'static str,
    path: Option<String>,
    owner: Option<String>,
}

struct Local {
    pacman: bool,
    packages: Vec<(String, String)>,
    binaries: Vec<Binary>,
    error: Option<String>,
}

/// Local queries do not refresh package databases, download or install anything.
fn local_status() -> Local {
    let pacman = cmd::which("pacman").is_some();
    let mut local = Local {
        pacman,
        packages: Vec::new(),
        binaries: ["mangoapp", "mangohud"]
            .into_iter()
            .map(|name| Binary {
                name,
                path: cmd::which(name),
                owner: None,
            })
            .collect(),
        error: None,
    };
    if !pacman {
        return local;
    }
    // Querying the full local database distinguishes an absent package from a
    // failed query. Named -Q queries return exit 1 for expected missing variants.
    let installed = match cmd::output("pacman", ["-Q"]) {
        Ok(out) if out.status == 0 => out.stdout,
        Ok(out) => {
            local.error = Some(check::failure("pacman -Q", out.status, &out.stderr));
            return local;
        }
        Err(error) => {
            local.error = Some(format!("{error:#}"));
            return local;
        }
    };
    for binary in &mut local.binaries {
        let Some(path) = &binary.path else { continue };
        if let Ok(out) = cmd::output("pacman", ["-Qqo", path.as_str()])
            && out.status == 0
        {
            binary.owner = out.stdout.lines().next().map(str::to_owned);
        }
    }
    local.packages = installed
        .lines()
        .filter_map(|line| {
            let (name, version) = line.split_once(' ')?;
            (PACKAGES.contains(&name)
                || local
                    .binaries
                    .iter()
                    .any(|b| b.owner.as_deref() == Some(name)))
            .then(|| (name.to_owned(), version.trim().to_owned()))
        })
        .collect();
    local
}

struct Panel {
    ctx: Ctx,
    content: gtk::Box,
    local: RefCell<Option<Local>>,
    loading: Cell<bool>,
}

pub fn build(ctx: &Ctx) -> gtk::Widget {
    let root = gtk::Box::new(gtk::Orientation::Vertical, 0);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    root.append(&content);
    let panel = Rc::new(Panel {
        ctx: ctx.clone(),
        content,
        local: RefCell::new(None),
        loading: Cell::new(false),
    });
    panel.render(&state::current());

    let weak = Rc::downgrade(&panel);
    let mut rx = state::subscribe();
    glib::spawn_future_local(async move {
        while rx.changed().await.is_ok() {
            let Some(panel) = weak.upgrade() else { break };
            let st = rx.borrow_and_update().clone();
            panel.render(&st);
            if !st.checking {
                panel.load_local();
            }
        }
    });
    ui::on_shown(&root, move || {
        panel.load_local();
        let st = state::current();
        if !st.checking
            && st
                .report
                .as_ref()
                .is_none_or(|r| check::now() - r.checked_at > 15 * 60)
        {
            state::trigger(false);
        }
    });
    root.upcast()
}

impl Panel {
    fn load_local(self: &Rc<Self>) {
        if self.loading.replace(true) {
            return;
        }
        self.render(&state::current());
        let weak = Rc::downgrade(self);
        glib::spawn_future_local(async move {
            let local = rt::blocking(local_status).await;
            let Some(panel) = weak.upgrade() else { return };
            *panel.local.borrow_mut() = Some(local);
            panel.loading.set(false);
            panel.render(&state::current());
        });
    }

    fn render(self: &Rc<Self>, st: &State) {
        ui::clear(&self.content);
        let group = adw::PreferencesGroup::builder()
            .title("MangoHUD packages and updates")
            .description("MangoApp powers this overlay. Updates use Hyprdeck’s reviewed full-system updater, including repository and AUR packages; upstream releases are informational, not installable updates.")
            .build();
        self.content.append(&group);

        let actions = gtk::Box::new(gtk::Orientation::Horizontal, 6);
        let check = gtk::Button::builder()
            .label(if st.checking {
                "Checking…"
            } else {
                "Check now"
            })
            .sensitive(!st.checking)
            .valign(gtk::Align::Center)
            .build();
        let weak = Rc::downgrade(self);
        check.connect_clicked(move |_| {
            state::trigger(false);
            if let Some(panel) = weak.upgrade() {
                panel.load_local();
            }
        });
        actions.append(&check);
        let review = gtk::Button::builder()
            .label("Review system updates…")
            .valign(gtk::Align::Center)
            .tooltip_text("Prepare a fresh review of all installed package updates; nothing is installed until you approve")
            .build();
        let ctx = self.ctx.clone();
        review.connect_clicked(move |_| crate::dialog::open(&ctx));
        actions.append(&review);
        group.set_header_suffix(Some(&actions));

        let local = self.local.borrow();
        let Some(local) = local.as_ref() else {
            group.add(&row(
                "Reading installed packages…",
                "Local package and executable ownership queries run in the background.",
            ));
            review.set_sensitive(false);
            return;
        };
        review.set_sensitive(local.pacman && local.error.is_none());
        if let Some(error) = &local.error {
            group.add(&row("Could not read installed package versions", error));
        }
        for binary in &local.binaries {
            let detail = match (&binary.path, &binary.owner) {
                (Some(path), Some(owner)) => format!("{path} · owned by {owner}"),
                (Some(path), None) if local.pacman && local.error.is_none() => {
                    format!(
                        "{path} · pacman ownership could not be established; this file is not automatically updated here"
                    )
                }
                (Some(path), None) => format!("{path} · package ownership unavailable"),
                (None, _) => "Not found on PATH".into(),
            };
            group.add(&row(binary.name, &detail));
        }
        for (name, version) in &local.packages {
            group.add(&row(
                name,
                &package_detail(name, version, st.report.as_deref()),
            ));
        }
        if !local.pacman {
            group.add(&row("Use your distribution’s package manager", "Installed package versions and updates require pacman here. Install or update MangoHUD and MangoApp through your distribution’s package manager; some distributions package mangoapp separately."));
        } else if local
            .binaries
            .iter()
            .any(|b| b.name == "mangoapp" && b.path.is_none())
        {
            group.add(&row("Install the missing MangoApp dependency", "On Arch, mangohud includes mangoapp: install it as part of a full upgrade with sudo pacman -Syu mangohud. Other distributions may package mangoapp separately. Hyprdeck’s updater upgrades installed packages; it does not install missing dependencies."));
        }
        group.add(&row("32-bit games (optional)", "lib32-mangohud supplies the 32-bit libraries, not the MangoApp executable. On Arch it requires the multilib repository; install it through your package manager as part of a full system upgrade only if needed."));

        if let Some(report) = &st.report {
            group.add(&row(
                "Last package check",
                &format!(
                    "{} · {}",
                    crate::ago(check::now() - report.checked_at),
                    if st.checking {
                        "refresh in progress"
                    } else {
                        "Check now refreshes repository, AUR and upstream status"
                    }
                ),
            ));
            for (name, error) in [
                ("Repository check incomplete", &report.repo_error),
                ("AUR check incomplete", &report.aur_error),
            ] {
                if let Some(error) = error {
                    group.add(&row(name, error));
                }
            }
            if let Some(tracked) = report.tracked.iter().find(|t| t.github == UPSTREAM) {
                match &tracked.upstream {
                    Some(release) => {
                        let release_row = row(
                            "Latest upstream release (not a package update)",
                            &format!(
                                "{} · Your distribution decides when this release becomes an installable package update.",
                                release.tag
                            ),
                        );
                        let link = gtk::LinkButton::with_label(&release.url, "Release notes");
                        link.set_valign(gtk::Align::Center);
                        release_row.add_suffix(&link);
                        group.add(&release_row);
                    }
                    None => group.add(&row(
                        "Latest upstream release",
                        "Could not be determined by the last check.",
                    )),
                }
                if let Some(error) = &tracked.upstream_error {
                    group.add(&row("Upstream lookup problem", error));
                }
            }
        } else {
            group.add(&row("Package updates have not been checked", "Check now lists distribution and AUR updates without installing anything. Review system updates prepares the existing news and package review before installation."));
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum PackageStatus<'a> {
    Unchecked,
    Available(&'static str, &'a crate::parse::Update),
    Incomplete,
    Untracked,
    Development,
    Current,
}

fn package_status<'a>(name: &str, version: &str, report: Option<&'a Report>) -> PackageStatus<'a> {
    let Some(report) = report else {
        return PackageStatus::Unchecked;
    };
    for (source, updates) in [("Repository", &report.repo), ("AUR", &report.aur)] {
        if let Some(update) = updates
            .iter()
            .find(|u| u.name == name && crate::vercmp::vercmp(&u.new, version).is_gt())
        {
            return PackageStatus::Available(source, update);
        }
    }
    if report.failed() {
        PackageStatus::Incomplete
    } else if report.not_in_aur.iter().any(|p| p.name == name) {
        PackageStatus::Untracked
    } else if name.ends_with("-git") {
        PackageStatus::Development
    } else {
        PackageStatus::Current
    }
}

fn package_detail(name: &str, version: &str, report: Option<&Report>) -> String {
    let detail = match package_status(name, version, report) {
        PackageStatus::Unchecked => "update status not checked".into(),
        PackageStatus::Available(source, update) => format!("{source} update: {} → {} · Review system updates to install", update.old, update.new),
        PackageStatus::Incomplete => "package update check incomplete".into(),
        PackageStatus::Untracked => "foreign/local package not found in the AUR; update it through its package maintainer".into(),
        PackageStatus::Development => "no newer package version listed; new upstream commits require your AUR helper’s development-package update workflow".into(),
        PackageStatus::Current => "no update listed by the last package check".into(),
    };
    format!("Installed {version} · {detail}")
}

fn row(title: &str, subtitle: &str) -> adw::ActionRow {
    let row = adw::ActionRow::builder().use_markup(false).build();
    row.set_title(title);
    row.set_subtitle(subtitle);
    row.set_subtitle_lines(0);
    row
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::check::Tooling;

    fn report() -> Report {
        Report {
            checked_at: 0,
            tooling: Tooling {
                pacman: true,
                checkupdates: true,
                aur_helper: None,
            },
            repo: Vec::new(),
            repo_error: None,
            aur: Vec::new(),
            aur_error: None,
            not_in_aur: Vec::new(),
            tracked: Vec::new(),
        }
    }

    #[test]
    fn installed_version_and_distribution_update_are_distinct() {
        let mut report = report();
        report.repo = crate::parse::parse_updates("mangohud 0.8.3-1 -> 0.8.4-1");
        assert_eq!(
            package_status("mangohud", "0.8.3-1", Some(&report)),
            PackageStatus::Available("Repository", &report.repo[0])
        );
        // A package manager transaction can finish before the next shared check.
        assert_eq!(
            package_status("mangohud", "0.8.4-1", Some(&report)),
            PackageStatus::Current
        );
        assert_eq!(
            package_status("mangohud", "0.8.5-1", Some(&report)),
            PackageStatus::Current
        );
    }

    #[test]
    fn aur_and_development_updates_keep_their_limitations() {
        let mut report = report();
        report.aur = crate::parse::parse_updates("mangohud-git 0.8.4.r1-1 -> 0.8.4.r2-1");
        assert_eq!(
            package_status("mangohud-git", "0.8.4.r1-1", Some(&report)),
            PackageStatus::Available("AUR", &report.aur[0])
        );
        report.aur.clear();
        assert_eq!(
            package_status("mangohud-git", "0.8.4.r1-1", Some(&report)),
            PackageStatus::Development
        );
        report.aur_error = Some("offline".into());
        assert_eq!(
            package_status("mangohud-git", "0.8.4.r1-1", Some(&report)),
            PackageStatus::Incomplete
        );
    }

    #[test]
    fn untracked_foreign_package_does_not_claim_to_be_up_to_date() {
        let mut report = report();
        report.not_in_aur.push(crate::aur::Foreign {
            name: "mangoapp".into(),
            version: "0.8.4-1".into(),
        });
        assert_eq!(
            package_status("mangoapp", "0.8.4-1", Some(&report)),
            PackageStatus::Untracked
        );
    }
}
