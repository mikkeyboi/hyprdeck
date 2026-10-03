//! Unified view over the three startup sources, shared by the page and the CLI.

use anyhow::{Result, bail};
use hyprdeck_core::cmd;

use crate::desktop;
use crate::hypr::{self, HyprExec};
use crate::units::{self, Props, Unit};
use crate::xdg::{self, Autostart};

#[derive(Debug, Clone)]
pub enum Entry {
    App(Box<Autostart>, Props),
    Service(Box<Unit>),
    Compositor(HyprExec),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Running,
    /// Socket/path/timer unit armed and waiting.
    Waiting,
    Stopped,
    Failed,
    Disabled,
    /// Never started in this session (desktop filter, missing program, …).
    Skipped,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            State::Running => "Running",
            State::Waiting => "Waiting",
            State::Stopped => "Stopped",
            State::Failed => "Failed",
            State::Disabled => "Disabled",
            State::Skipped => "Not used",
        }
    }
}

/// Which part of the page an entry belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    Apps,
    /// Apps never started on this desktop.
    AppsSkipped,
    Services,
    VendorServices,
    Compositor,
}

/// Result of a toggle request.
#[derive(Debug, PartialEq)]
pub enum Toggled {
    Done,
    /// A vendor unit enabled in /etc/systemd/user; only masking stops it.
    NeedsMask,
}

impl Entry {
    pub fn id(&self) -> String {
        match self {
            Entry::App(a, _) => a.id.clone(),
            Entry::Service(u) => u.name.clone(),
            Entry::Compositor(h) => h.id(),
        }
    }

    pub fn name(&self) -> String {
        match self {
            Entry::App(a, _) => a.name.clone(),
            Entry::Service(u) => u.name.clone(),
            Entry::Compositor(h) => h.program.clone(),
        }
    }

    /// One-line description / command.
    pub fn detail(&self) -> String {
        match self {
            Entry::App(a, _) => a.comment.clone().unwrap_or_else(|| a.command.clone()),
            Entry::Service(u) => {
                let mut s = if u.description.is_empty() {
                    u.exec.clone().unwrap_or_default()
                } else {
                    u.description.clone()
                };
                if let Some(w) = u.starts_with()
                    && u.enabled()
                {
                    s.push_str(" · starts ");
                    s.push_str(w);
                }
                s
            }
            Entry::Compositor(h) => format!("{} · {}", h.command, h.location()),
        }
    }

    pub fn section(&self) -> Section {
        match self {
            Entry::App(a, _) if a.eval.blocker.is_some() => Section::AppsSkipped,
            Entry::App(..) => Section::Apps,
            Entry::Service(u) if u.vendor => Section::VendorServices,
            Entry::Service(_) => Section::Services,
            Entry::Compositor(_) => Section::Compositor,
        }
    }

    pub fn source_label(&self) -> &'static str {
        match self.section() {
            Section::Apps | Section::AppsSkipped => "app",
            Section::Services => "service",
            Section::VendorServices => "system-service",
            Section::Compositor => "compositor",
        }
    }

    pub fn enabled(&self) -> bool {
        match self {
            Entry::App(a, _) => a.eval.enabled,
            Entry::Service(u) => u.enabled(),
            Entry::Compositor(h) => h.enabled,
        }
    }

    /// Why the switch is locked (None = can toggle).
    pub fn toggle_blocker(&self) -> Option<String> {
        match self {
            Entry::App(a, _) => a.eval.blocker.clone(),
            Entry::Service(u) => u.toggle_blocker(),
            Entry::Compositor(h) => h.readonly.clone(),
        }
    }

    pub fn state(&self) -> State {
        match self {
            Entry::App(a, props) => match props.active() {
                "active" | "activating" | "reloading" | "deactivating" => State::Running,
                "failed" => State::Failed,
                _ if a.eval.blocker.is_some() => State::Skipped,
                _ if !a.eval.enabled => State::Disabled,
                _ => State::Stopped,
            },
            Entry::Service(u) => match u.props.active() {
                "active"
                    if matches!(u.props.get("SubState"), "waiting" | "listening" | "elapsed") =>
                {
                    State::Waiting
                }
                "active" | "activating" | "reloading" | "deactivating" => State::Running,
                "failed" => State::Failed,
                _ if !u.enabled() && u.toggle_blocker().is_none() => State::Disabled,
                _ => State::Stopped,
            },
            Entry::Compositor(h) if h.running => State::Running,
            Entry::Compositor(h) if !h.enabled => State::Disabled,
            Entry::Compositor(_) => State::Stopped,
        }
    }

    /// systemd unit controlling the running instance (for Start/Stop/log).
    pub fn unit(&self) -> Option<&str> {
        match self {
            Entry::App(a, _) => Some(&a.unit),
            Entry::Service(u) => Some(&u.name),
            Entry::Compositor(_) => None,
        }
    }

    pub fn path(&self) -> &std::path::Path {
        match self {
            Entry::App(a, _) => &a.path,
            Entry::Service(u) => &u.path,
            Entry::Compositor(h) => &h.file,
        }
    }

    pub fn deletable(&self) -> bool {
        match self {
            Entry::App(a, _) => a.deletable(),
            Entry::Service(u) => !u.vendor,
            Entry::Compositor(_) => false,
        }
    }

    pub fn essential(&self) -> Option<String> {
        match self {
            Entry::Compositor(h) => h.essential(),
            _ => None,
        }
    }

    pub fn icon(&self) -> Option<&str> {
        match self {
            Entry::App(a, _) => a.icon.as_deref(),
            _ => None,
        }
    }

    pub fn matches(&self, query: &str) -> bool {
        query.is_empty()
            || [self.name(), self.detail(), self.id()]
                .iter()
                .any(|s| s.to_lowercase().contains(query))
    }

    /// Enable/disable for the next login.
    pub fn set_enabled(&self, on: bool) -> Result<Toggled> {
        match self {
            Entry::App(a, _) => xdg::set_enabled(&a.id, on).map(|()| Toggled::Done),
            Entry::Service(u) if on => units::enable(&u.name).map(|()| Toggled::Done),
            Entry::Service(u) => Ok(match units::disable(&u.name)? {
                units::Disabled::Done => Toggled::Done,
                units::Disabled::EnabledSystemWide => Toggled::NeedsMask,
            }),
            Entry::Compositor(h) => {
                if let Some(why) = &h.readonly {
                    bail!("{} can't be toggled: {why}", h.location());
                }
                hypr::set_enabled(&h.file, h.line, on).map(|()| Toggled::Done)
            }
        }
    }

    /// `start` | `stop` | `restart` for the current session.
    pub fn control(&self, verb: &str) -> Result<()> {
        match self {
            Entry::App(a, props) if !props.loaded() => {
                if verb == "stop" {
                    bail!("{} is not running as {}", a.name, a.unit);
                }
                // No generated unit yet (added/enabled this session): launch it directly.
                if !cmd::launch_prefix().is_empty() {
                    return cmd::spawn_detached("uwsm", ["app", "--", &a.path.to_string_lossy()]);
                }
                let exec = a.file.get_string("Exec").unwrap_or_default();
                let args: Vec<String> = desktop::exec_args(&exec)
                    .into_iter()
                    .filter(|w| !(w.len() == 2 && w.starts_with('%') && w != "%%"))
                    .map(|w| w.replace("%%", "%"))
                    .collect();
                let Some((program, rest)) = args.split_first() else {
                    bail!("{} has no Exec command", a.name)
                };
                cmd::spawn_detached(program, rest)
            }
            Entry::App(..) | Entry::Service(_) => {
                units::control(verb, self.unit().unwrap_or_default())
            }
            Entry::Compositor(h) => match verb {
                "start" => hypr::start(&h.command),
                "stop" => hypr::stop(&h.program),
                _ => {
                    if h.running {
                        hypr::stop(&h.program)?;
                    }
                    hypr::start(&h.command)
                }
            },
        }
    }
}

#[derive(Debug, Default)]
pub struct Snapshot {
    pub entries: Vec<Entry>,
    /// Problems reading a source (shown, not fatal).
    pub errors: Vec<String>,
}

/// Read every source (blocking; ~100 ms).
pub fn load() -> Snapshot {
    let mut snap = Snapshot::default();
    let apps = xdg::scan();
    let names: Vec<&str> = apps.iter().map(|a| a.unit.as_str()).collect();
    let mut props = match units::show(&names) {
        Ok(p) => p,
        Err(e) => {
            snap.errors.push(format!("Reading app status: {e:#}"));
            Default::default()
        }
    };
    for a in apps {
        let p = props.remove(&a.unit).unwrap_or_default();
        snap.entries.push(Entry::App(Box::new(a), p));
    }
    match units::scan() {
        Ok(us) => snap
            .entries
            .extend(us.into_iter().map(|u| Entry::Service(Box::new(u)))),
        Err(e) => snap
            .errors
            .push(format!("Reading systemd user units: {e:#}")),
    }
    let (execs, errors) = hypr::scan();
    snap.entries
        .extend(execs.into_iter().map(Entry::Compositor));
    snap.errors
        .extend(errors.into_iter().map(|e| format!("Hyprland config: {e}")));
    snap
}

/// Look up an entry by CLI id (desktop basename with or without `.desktop`,
/// unit name with or without `.service`, or `hypr:<program>`).
pub fn find<'a>(snap: &'a Snapshot, id: &str) -> Result<&'a Entry> {
    let hits: Vec<&Entry> = snap
        .entries
        .iter()
        .filter(|e| {
            let eid = e.id();
            eid == id
                || eid.strip_suffix(".desktop") == Some(id)
                || (matches!(e, Entry::Service(_)) && eid.strip_suffix(".service") == Some(id))
        })
        .collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => bail!("no startup entry {id:?} (see `hyprdeck startup list`)"),
        _ => bail!(
            "{id:?} is ambiguous: {}",
            hits.iter().map(|e| e.id()).collect::<Vec<_>>().join(", ")
        ),
    }
}
