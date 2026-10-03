//! Combined-sink lifecycle and routing.
//!
//! * the *primary* sink `simultaneous_output` combines the selected outputs and becomes
//!   the default, so ordinary applications play on all of them;
//! * one *route* sink `simultaneous_route_*` per multi-target routing rule; single-target
//!   rules move the application straight to that device.
//!
//! Every mutation runs under [`lock`]: an in-process mutex plus an `flock` on
//! `$XDG_RUNTIME_DIR/hyprdeck-audio.lock`, so the app daemon and `hyprdeck audio …`
//! CLI calls never interleave. Settings are re-read from disk inside the lock, so
//! changes made by another process are always seen.

use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::path::PathBuf;
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::{Mutex, MutexGuard};

use crate::config::{self, Config, Route, clean_members};
use crate::pw::{self, FanOut, PRIMARY_DESCRIPTION, PRIMARY_SINK, Sink, Stream};

/// One consistent view of the sound server plus settings.
#[derive(Debug, Clone)]
pub struct Status {
    /// The primary combined sink exists.
    pub active: bool,
    pub default_sink: String,
    pub sinks: Vec<Sink>,
    pub streams: Vec<Stream>,
    pub fanout: Vec<FanOut>,
    pub config: Config,
}

impl Status {
    /// Real outputs (no combine sinks).
    pub fn devices(&self) -> impl Iterator<Item = &Sink> {
        self.sinks.iter().filter(|s| !s.is_virtual())
    }

    pub fn sink(&self, name: &str) -> Option<&Sink> {
        self.sinks.iter().find(|s| s.name == name)
    }

    pub fn sink_at(&self, index: u32) -> Option<&Sink> {
        self.sinks.iter().find(|s| s.index == index)
    }

    pub fn label<'a>(&'a self, name: &'a str) -> &'a str {
        self.sink(name).map_or(name, |s| s.label.as_str())
    }

    /// Outputs a combine sink is currently feeding (from its live fan-out streams).
    pub fn members_of(&self, virtual_sink: &str) -> Vec<&Sink> {
        self.fanout
            .iter()
            .filter(|f| f.virtual_sink == virtual_sink)
            .filter_map(|f| self.sink_at(f.member_index))
            .collect()
    }

    /// Where a stream's audio ends up: the sink it is on, and the outputs behind it if virtual.
    pub fn destination(&self, stream: &Stream) -> Option<(&Sink, Vec<&Sink>)> {
        let sink = self.sink_at(stream.sink_index)?;
        let members = if sink.is_virtual() {
            self.members_of(&sink.name)
        } else {
            Vec::new()
        };
        Some((sink, members))
    }

    pub fn labels(&self, names: &[String]) -> String {
        names
            .iter()
            .map(|n| self.label(n))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

pub async fn status() -> Result<Status> {
    let default_sink = pw::default_or_empty().await;
    let (sinks, streams, offsets) = tokio::try_join!(
        pw::list_sinks(&default_sink),
        pw::list_streams(),
        pw::port_offsets()
    )?;
    let config = tokio::task::spawn_blocking(config::load).await??;
    let mut sinks = sinks;
    for s in &mut sinks {
        if let Some(port) = &s.port {
            s.latency_offset_us = offsets.get(port).copied();
        }
        // Route sinks carry a hyphenated slug as description; show the rule's own name.
        if s.is_route()
            && let Some(r) = config.routes.iter().find(|r| r.sink_name() == s.name)
        {
            s.label = r.display().to_owned();
        }
    }
    let (streams, fanout) = streams;
    Ok(Status {
        active: sinks.iter().any(Sink::is_primary),
        default_sink,
        sinks,
        streams,
        fanout,
        config,
    })
}

// --- locking ------------------------------------------------------------------

static LOCAL: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// Held for the duration of one engine operation.
pub struct OpGuard {
    _local: MutexGuard<'static, ()>,
    _file: File,
}

fn lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map_or_else(std::env::temp_dir, PathBuf::from)
        .join("hyprdeck-audio.lock")
}

pub async fn lock() -> Result<OpGuard> {
    let local = LOCAL.lock().await;
    let path = lock_path();
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("opening {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match file.try_lock() {
            Ok(()) => {
                return Ok(OpGuard {
                    _local: local,
                    _file: file,
                });
            }
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(TryLockError::WouldBlock) => bail!("Another audio operation is still running."),
            Err(TryLockError::Error(e)) => return Err(e).context("locking audio operations"),
        }
    }
}

async fn load_config() -> Result<Config> {
    tokio::task::spawn_blocking(config::load).await?
}

async fn save_config(cfg: &Config) -> Result<()> {
    let cfg = cfg.clone();
    tokio::task::spawn_blocking(move || config::save(&cfg)).await?
}

// --- primary combined sink ----------------------------------------------------

/// Turn the simultaneous output on with `members` (or replace the running group).
pub async fn enable(members: &[String]) -> Result<String> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let labels = enable_locked(&mut cfg, members, true).await?;
    Ok(format!("Playing on {labels}"))
}

pub async fn disable() -> Result<String> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let dest = disable_locked(&mut cfg).await?;
    Ok(match dest {
        Some(label) => format!("Simultaneous output off. Default: {label}"),
        None => "Simultaneous output off".to_owned(),
    })
}

/// On → off; off → on with the remembered group (or the first two outputs).
pub async fn toggle() -> Result<String> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let sinks = pw::list_sinks("").await?;
    if sinks.iter().any(Sink::is_primary) {
        disable_locked(&mut cfg).await?;
        return Ok("Simultaneous output off".to_owned());
    }
    let members = if cfg.selected.len() >= 2 {
        cfg.selected.clone()
    } else {
        sinks
            .iter()
            .filter(|s| !s.is_virtual())
            .take(2)
            .map(|s| s.name.clone())
            .collect()
    };
    let labels = enable_locked(&mut cfg, &members, true).await?;
    Ok(format!("Simultaneous output on: {labels}"))
}

/// Apply a new group selection: fewer than two turns the output off (if on).
pub async fn set_selected(members: &[String]) -> Result<String> {
    if clean_members(members).len() < 2 {
        let _g = lock().await?;
        let mut cfg = load_config().await?;
        if pw::sink_exists(PRIMARY_SINK).await? {
            disable_locked(&mut cfg).await?;
            return Ok("Simultaneous output off".to_owned());
        }
        bail!("Select at least two audio outputs.");
    }
    enable(members).await
}

/// Add or remove one output from the remembered group (tray checkboxes).
pub async fn toggle_member(name: &str) -> Result<String> {
    let mut members = load_config().await?.selected;
    if let Some(pos) = members.iter().position(|m| m == name) {
        members.remove(pos);
    } else {
        members.push(name.to_owned());
    }
    set_selected(&members).await
}

/// Returns the labels of the enabled members. `remember` stores them as the saved group;
/// restores of a partially connected group pass `false` so the full group is kept.
async fn enable_locked(cfg: &mut Config, members: &[String], remember: bool) -> Result<String> {
    let unique = clean_members(members);
    if unique.len() < 2 {
        bail!("Select at least two audio outputs.");
    }
    let sinks = pw::list_sinks("").await?;
    let live: HashSet<&str> = sinks.iter().map(|s| s.name.as_str()).collect();
    if let Some(missing) = unique.iter().find(|n| !live.contains(n.as_str())) {
        bail!("Output disconnected before it could be enabled: {missing}");
    }
    let current = pw::default_or_empty().await;
    let usable = |n: &str| !n.is_empty() && n != PRIMARY_SINK && live.contains(n);
    let mut keep_default = true;
    let mut attached = Vec::new();
    let mut pre_existing = Vec::new();
    let restore;
    if let Some(primary) = sinks.iter().find(|s| s.is_primary()) {
        // Replacing the group: keep the default on it only if it already was.
        keep_default = current == PRIMARY_SINK;
        restore = if usable(&cfg.restore_sink) {
            cfg.restore_sink.clone()
        } else {
            first_device(&sinks)?
        };
        attached = pw::streams_on(primary.index).await?;
        let dest = fallback_destination(&current, &unique, &sinks)?;
        move_all(&attached, &dest).await;
        unload(PRIMARY_SINK).await?;
    } else {
        restore = if usable(&current) {
            current.clone()
        } else {
            unique[0].clone()
        };
        // Streams claimed by a routing rule stay where the rule put them.
        pre_existing = pw::list_streams()
            .await?
            .0
            .iter()
            .filter(|s| cfg.route_for(s).is_none())
            .map(|s| s.index)
            .collect();
    }
    load_combine(PRIMARY_SINK, PRIMARY_DESCRIPTION, &unique).await?;
    cfg.restore_sink = restore;
    cfg.enabled = true;
    if remember {
        cfg.selected = unique.clone();
    }
    save_config(cfg).await?;
    if keep_default {
        pw::try_run(&pw::default_command(PRIMARY_SINK)).await;
        move_all(
            if attached.is_empty() {
                &pre_existing
            } else {
                &attached
            },
            PRIMARY_SINK,
        )
        .await;
    }
    apply_routes_locked(cfg).await?;
    Ok(unique
        .iter()
        .map(|n| label_in(&sinks, n))
        .collect::<Vec<_>>()
        .join(", "))
}

/// Returns the label of the output that became default (if the default had to move).
async fn disable_locked(cfg: &mut Config) -> Result<Option<String>> {
    let sinks = pw::list_sinks("").await?;
    let mut new_default = None;
    if let Some(primary) = sinks.iter().find(|s| s.is_primary()) {
        let current = pw::default_or_empty().await;
        let live: HashSet<&str> = sinks.iter().map(|s| s.name.as_str()).collect();
        let usable = |n: &str| !n.is_empty() && n != PRIMARY_SINK && live.contains(n);
        let dest = if usable(&current) {
            current.clone()
        } else if usable(&cfg.restore_sink) {
            cfg.restore_sink.clone()
        } else {
            first_device(&sinks)?
        };
        let attached = pw::streams_on(primary.index).await?;
        move_all(&attached, &dest).await;
        if current == PRIMARY_SINK {
            pw::try_run(&pw::default_command(&dest)).await;
        }
        new_default = Some(label_in(&sinks, &dest).to_owned());
    }
    // Also clears a stale module whose sink never appeared.
    unload(PRIMARY_SINK).await?;
    cfg.enabled = false;
    save_config(cfg).await?;
    Ok(new_default)
}

/// Daemon reconciliation: bring the primary sink back when it should be on (login,
/// PipeWire restart) or when a group member that was missing at creation has returned.
/// Returns a message when something was (re)created.
pub async fn restore() -> Result<Option<String>> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    if !cfg.enabled {
        return Ok(None);
    }
    let sinks = pw::list_sinks("").await?;
    let available: Vec<String> = cfg
        .selected
        .iter()
        .filter(|n| sinks.iter().any(|s| &s.name == *n))
        .cloned()
        .collect();
    if available.len() < 2 {
        return Ok(None);
    }
    if sinks.iter().any(Sink::is_primary) {
        let targets = pw::combine_targets(PRIMARY_SINK).await;
        if available.iter().all(|a| targets.contains(a)) {
            return Ok(None);
        }
    }
    let labels = enable_locked(&mut cfg, &available, false).await?;
    Ok(Some(format!("Simultaneous output restored: {labels}")))
}

// --- routing ------------------------------------------------------------------

/// Make live route sinks match the active multi-target rules exactly.
async fn sync_routes(cfg: &Config) -> Result<()> {
    let wanted: Vec<(String, &Route)> = cfg
        .active_routes()
        .filter(|r| r.needs_combine())
        .map(|r| (r.sink_name(), r))
        .collect();
    let sinks = pw::list_sinks("").await?;
    let live: Vec<String> = sinks
        .iter()
        .filter(|s| s.is_route())
        .map(|s| s.name.clone())
        .collect();
    for stale in live.iter().filter(|l| !wanted.iter().any(|(w, _)| w == *l)) {
        retire_route_sink(stale, "").await?;
    }
    let available: HashSet<&str> = sinks.iter().map(|s| s.name.as_str()).collect();
    for (name, route) in &wanted {
        let targets: Vec<&String> = route
            .sinks
            .iter()
            .filter(|s| available.contains(s.as_str()))
            .collect();
        let is_live = live.contains(name);
        // Compare against the *connected* targets so a missing member does not rebuild every pass.
        if is_live
            && pw::combine_targets(name)
                .await
                .iter()
                .eq(targets.iter().copied())
        {
            continue;
        }
        if is_live {
            retire_route_sink(name, "").await?;
        }
        if targets.len() >= 2 {
            load_combine(name, &route.description(), &targets).await?;
        }
    }
    Ok(())
}

/// Move every matching stream onto its rule's target; returns how many moved.
async fn apply_routes_locked(cfg: &Config) -> Result<usize> {
    if cfg.active_routes().next().is_none() {
        return Ok(0);
    }
    sync_routes(cfg).await?;
    let (streams, _) = pw::list_streams().await?;
    let sinks = pw::list_sinks("").await?;
    let mut moved = 0;
    for stream in &streams {
        let Some(route) = cfg.route_for(stream) else {
            continue;
        };
        let target = route.sink_name();
        let Some(dest) = sinks.iter().find(|s| s.name == target) else {
            continue;
        };
        if dest.index == stream.sink_index {
            continue;
        }
        if pw::try_run(&pw::move_command(stream.index, &target)).await {
            moved += 1;
            if let Some(v) = route.volume {
                pw::try_run(&pw::stream_volume_command(stream.index, v as f64)).await;
            }
        }
    }
    Ok(moved)
}

/// Daemon entry point for stream/sink events.
pub async fn apply_routes() -> Result<usize> {
    let _g = lock().await?;
    let cfg = load_config().await?;
    apply_routes_locked(&cfg).await
}

/// Startup/settings change: sync helper sinks and route everything.
pub async fn reconcile_routes() -> Result<usize> {
    let _g = lock().await?;
    let cfg = load_config().await?;
    sync_routes(&cfg).await?;
    apply_routes_locked(&cfg).await
}

/// Send a rule's streams back to the default output and drop its helper sink.
async fn release_route(route: &Route) -> Result<()> {
    if route.needs_combine() {
        return retire_route_sink(&route.sink_name(), "").await;
    }
    let target = pw::default_or_empty().await;
    let sinks = pw::list_sinks("").await?;
    let (Some(on), Some(dest)) = (
        sinks.iter().find(|s| s.name == route.sink_name()),
        sinks.iter().find(|s| s.name == target),
    ) else {
        return Ok(());
    };
    if on.index == dest.index {
        return Ok(());
    }
    for s in pw::list_streams()
        .await?
        .0
        .iter()
        .filter(|s| s.sink_index == on.index && route.matches_ignoring_state(s))
    {
        pw::try_run(&pw::move_command(s.index, &target)).await;
    }
    Ok(())
}

/// Create or replace (by id) a routing rule, then apply it.
pub async fn save_route(route: Route) -> Result<String> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let msg = format!("Saved {}", route.display());
    if let Some(slot) = cfg.routes.iter_mut().find(|r| r.id == route.id) {
        let old = std::mem::replace(slot, route);
        if old.active()
            && (old.match_kind, &old.match_value, &old.sinks, &old.label)
                != (slot.match_kind, &slot.match_value, &slot.sinks, &slot.label)
        {
            release_route(&old).await?;
        }
    } else {
        cfg.routes.push(route);
    }
    save_config(&cfg).await?;
    sync_routes(&cfg).await?;
    apply_routes_locked(&cfg).await?;
    Ok(msg)
}

pub async fn remove_route(id: &str) -> Result<String> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let pos = cfg
        .routes
        .iter()
        .position(|r| r.id == id)
        .ok_or_else(|| anyhow!("No routing rule {id}"))?;
    let route = cfg.routes.remove(pos);
    save_config(&cfg).await?;
    release_route(&route).await?;
    sync_routes(&cfg).await?;
    apply_routes_locked(&cfg).await?;
    Ok(format!("Removed {}", route.display()))
}

/// Enable or disable a rule. Re-enabling routes matching streams immediately.
pub async fn set_route_enabled(id: &str, enabled: bool) -> Result<String> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let route = cfg
        .routes
        .iter_mut()
        .find(|r| r.id == id)
        .ok_or_else(|| anyhow!("No routing rule {id}"))?;
    route.enabled = enabled;
    let route = route.clone();
    save_config(&cfg).await?;
    if enabled {
        sync_routes(&cfg).await?;
        let moved = apply_routes_locked(&cfg).await?;
        Ok(format!("{} on ({moved} stream(s) moved)", route.display()))
    } else {
        release_route(&route).await?;
        sync_routes(&cfg).await?;
        apply_routes_locked(&cfg).await?;
        Ok(format!("{} released", route.display()))
    }
}

pub async fn toggle_route(id: &str) -> Result<String> {
    let enabled = load_config()
        .await?
        .route(id)
        .map(|r| r.enabled)
        .ok_or_else(|| anyhow!("No routing rule {id}"))?;
    set_route_enabled(id, !enabled).await
}

/// CLI `route`: replace any rule with the same match, then apply. Returns (rule, moved).
pub async fn add_cli_route(route: Route) -> Result<(Route, usize)> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let mut removed = Vec::new();
    cfg.routes.retain(|r| {
        let same = r.match_kind == route.match_kind && r.match_value == route.match_value;
        if same {
            removed.push(r.clone());
        }
        !same
    });
    cfg.routes.push(route.clone());
    save_config(&cfg).await?;
    for old in removed
        .iter()
        .filter(|o| o.active() && o.sinks != route.sinks)
    {
        release_route(old).await?;
    }
    sync_routes(&cfg).await?;
    let moved = apply_routes_locked(&cfg).await?;
    Ok((route, moved))
}

/// CLI `unroute`: remove rules whose match value, id or label equals `key`.
pub async fn remove_cli_routes(key: &str) -> Result<Vec<Route>> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    let (removed, kept): (Vec<Route>, Vec<Route>) = std::mem::take(&mut cfg.routes)
        .into_iter()
        .partition(|r| r.match_value == key || r.id == key || r.label == key);
    if removed.is_empty() {
        bail!("No routing rule matches '{key}'");
    }
    cfg.routes = kept;
    save_config(&cfg).await?;
    for r in &removed {
        release_route(r).await?;
    }
    sync_routes(&cfg).await?;
    Ok(removed)
}

// --- volume, default, streams ---------------------------------------------------

pub async fn set_volume(sink: &str, percent: f64) -> Result<()> {
    let ceiling = load_config().await?.max_volume;
    pw::run(&pw::volume_command(sink, percent, ceiling))
        .await
        .map(drop)
}

pub async fn set_mute(sink: &str, muted: bool) -> Result<()> {
    pw::run(&pw::mute_command(sink, muted)).await.map(drop)
}

pub async fn set_default(sink: &str) -> Result<String> {
    pw::run(&pw::default_command(sink)).await?;
    let sinks = pw::list_sinks("").await?;
    Ok(format!("Default output: {}", label_in(&sinks, sink)))
}

pub async fn move_stream(stream: u32, sink: &str) -> Result<String> {
    tracing::debug!("moving stream {stream} to {sink}");
    pw::run(&pw::move_command(stream, sink)).await?;
    let sinks = pw::list_sinks("").await?;
    Ok(format!("Moved to {}", label_in(&sinks, sink)))
}

/// Set every real output's volume; reports outputs that refused.
pub async fn apply_to_all(percent: f64) -> Result<String> {
    let ceiling = load_config().await?.max_volume;
    let mut failed = Vec::new();
    for s in pw::list_sinks("").await?.iter().filter(|s| !s.is_virtual()) {
        if !pw::try_run(&pw::volume_command(&s.name, percent, ceiling)).await {
            failed.push(s.label.clone());
        }
    }
    if !failed.is_empty() {
        bail!("Could not set: {}", failed.join(", "));
    }
    Ok(format!(
        "All outputs set to {}%",
        pw::clamp(percent, ceiling)
    ))
}

pub async fn mute_all(muted: bool) -> Result<String> {
    let mut failed = Vec::new();
    for s in pw::list_sinks("").await?.iter().filter(|s| !s.is_virtual()) {
        if !pw::try_run(&pw::mute_command(&s.name, muted)).await {
            failed.push(s.label.clone());
        }
    }
    if !failed.is_empty() {
        bail!("Could not update: {}", failed.join(", "));
    }
    Ok(if muted {
        "All outputs muted"
    } else {
        "All outputs unmuted"
    }
    .to_owned())
}

/// Port latency offset of an output. `module-combine-sink` runs with
/// `latency_compensate=true`, so raising one member's offset delays the others to match.
pub async fn set_latency_offset(sink: &str, usec: i64) -> Result<()> {
    let sinks = pw::list_sinks("").await?;
    let (card, port) = sinks
        .iter()
        .find(|s| s.name == sink)
        .and_then(|s| s.port.clone())
        .ok_or_else(|| anyhow!("{sink} has no card port to adjust"))?;
    pw::run(&pw::latency_offset_command(&card, &port, usec))
        .await
        .map(drop)
}

/// Update persisted preferences under the lock.
pub async fn update_settings(f: impl FnOnce(&mut Config) + Send + 'static) -> Result<()> {
    let _g = lock().await?;
    let mut cfg = load_config().await?;
    f(&mut cfg);
    cfg.normalize();
    save_config(&cfg).await
}

/// Create and remove a temporary two-output combined sink.
pub async fn self_test() -> Result<String> {
    const NAME: &str = "simultaneous_output_self_test";
    let _g = lock().await?;
    let devices: Vec<String> = pw::list_sinks("")
        .await?
        .into_iter()
        .filter(|s| !s.is_virtual())
        .take(2)
        .map(|s| s.name)
        .collect();
    if devices.len() < 2 {
        bail!("Self-test needs at least two audio outputs.");
    }
    pw::run(&pw::combine_command(
        NAME,
        "Simultaneous-Output-Self-Test",
        &devices,
    ))
    .await?;
    let appeared = wait_until(|| pw::sink_exists(NAME)).await;
    for id in pw::module_ids(NAME).await? {
        pw::try_run(&["pactl".into(), "unload-module".into(), id.to_string()]).await;
    }
    let gone = wait_until(|| async { Ok(!pw::sink_exists(NAME).await?) }).await;
    match (appeared, gone) {
        (true, true) => Ok(
            "PASS: PipeWire created and removed a temporary two-output combined sink.".to_owned(),
        ),
        (false, _) => bail!("FAIL: PipeWire loaded the module but never exposed the test output."),
        (true, false) => bail!("FAIL: the test output did not go away after unloading."),
    }
}

// --- helpers --------------------------------------------------------------------

fn label_in<'a>(sinks: &'a [Sink], name: &'a str) -> &'a str {
    sinks
        .iter()
        .find(|s| s.name == name)
        .map_or(name, |s| s.label.as_str())
}

fn first_device(sinks: &[Sink]) -> Result<String> {
    sinks
        .iter()
        .find(|s| !s.is_virtual())
        .map(|s| s.name.clone())
        .ok_or_else(|| anyhow!("No physical audio output is currently available."))
}

fn fallback_destination(current: &str, candidates: &[String], sinks: &[Sink]) -> Result<String> {
    let live = |n: &str| sinks.iter().any(|s| s.name == n);
    if !current.is_empty() && current != PRIMARY_SINK && live(current) {
        return Ok(current.to_owned());
    }
    match candidates.iter().find(|c| live(c)) {
        Some(c) => Ok(c.clone()),
        None => first_device(sinks),
    }
}

/// Move streams to `dest` and make sure they arrive: pipewire-pulse sometimes
/// acknowledges a move and then drops it (observed right after a combine sink is
/// created), so stragglers are re-moved a few times.
async fn move_all(indices: &[u32], dest: &str) {
    if indices.is_empty() {
        return;
    }
    tracing::debug!("moving streams {indices:?} to {dest}");
    for &i in indices {
        pw::try_run(&pw::move_command(i, dest)).await;
    }
    for attempt in 1..=4u64 {
        tokio::time::sleep(Duration::from_millis(120 * attempt)).await;
        let (Ok(sinks), Ok((streams, _))) = (pw::list_sinks("").await, pw::list_streams().await)
        else {
            return;
        };
        let Some(target) = sinks.iter().find(|s| s.name == dest).map(|s| s.index) else {
            return;
        };
        let stray: Vec<u32> = streams
            .iter()
            .filter(|s| s.sink_index != target && indices.contains(&s.index))
            .map(|s| s.index)
            .collect();
        if stray.is_empty() {
            return;
        }
        tracing::debug!("re-moving streams {stray:?} to {dest}");
        for i in stray {
            pw::try_run(&pw::move_command(i, dest)).await;
        }
    }
}

/// Poll `check` every 50 ms for up to 4 s.
async fn wait_until<F, Fut>(mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<bool>>,
{
    let deadline = Instant::now() + Duration::from_secs(4);
    while Instant::now() < deadline {
        if matches!(check().await, Ok(true)) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    false
}

async fn load_combine<S: AsRef<str>>(name: &str, description: &str, members: &[S]) -> Result<()> {
    if let Err(e) = pw::run(&pw::combine_command(name, description, members)).await {
        bail!(
            "PipeWire could not create {description}: {e:#}. Your previous default was left intact."
        );
    }
    if !wait_until(|| pw::sink_exists(name)).await {
        unload(name).await?;
        bail!("PipeWire loaded the module but never exposed the output.");
    }
    Ok(())
}

/// Move a route sink's streams to `dest` (or the default) and unload it.
async fn retire_route_sink(name: &str, dest: &str) -> Result<()> {
    if let Some(sink) = pw::list_sinks("")
        .await?
        .into_iter()
        .find(|s| s.name == name)
    {
        let target = if dest.is_empty() {
            pw::default_or_empty().await
        } else {
            dest.to_owned()
        };
        if !target.is_empty() && target != name {
            move_all(&pw::streams_on(sink.index).await?, &target).await;
        }
    }
    unload(name).await
}

/// Unload every combine module owning `name` and wait until PipeWire has torn down
/// the sink *and* its fan-out streams (Pulse reuses module ids immediately while the
/// combine stream is destroyed asynchronously).
async fn unload(name: &str) -> Result<()> {
    for id in pw::module_ids(name).await? {
        pw::try_run(&["pactl".into(), "unload-module".into(), id.to_string()]).await;
    }
    let gone =
        wait_until(|| async { Ok(!pw::sink_exists(name).await? && !pw::has_fanout(name).await?) })
            .await;
    if !gone {
        bail!("Timed out while removing the previous {name} routes.");
    }
    Ok(())
}
