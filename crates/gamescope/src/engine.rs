//! Blocking planning and native Wayland Steam session lifecycle.
//!
//! Gamescope 3.16.25 parses `-r` as integer Hz (src/main.cpp); fractional
//! display rates are rounded explicitly, not passed as silently truncated text.
//! Its Wayland backend sizes buffers in physical pixels, maps app_id `gamescope`,
//! and requests fullscreen without an output. Placement therefore belongs to
//! Hyprland, not the SDL-only `--display-index` or DRM-only `--prefer-output`.

use std::collections::HashMap;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyprdeck_core::{cmd, hypr::ctl};
use serde::Deserialize;

use crate::config::{Filter, Settings, Toggle, WindowMode};

const UNIT: &str = "hyprdeck-gamescope.service";
const POLL: Duration = Duration::from_millis(200);
static LAUNCH_LOCK: Mutex<()> = Mutex::new(());

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SteamState {
    Stopped,
    Desktop,
    Gamescope,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub displays: Vec<ctl::Monitor>,
    pub vrr_policy: i64,
    pub gamescope_available: bool,
    pub steam_available: bool,
    pub session: SessionStatus,
}

pub fn snapshot() -> Result<Snapshot> {
    let displays =
        ctl::monitors().context("Detecting Hyprland displays; run inside your Hyprland session")?;
    let option: serde_json::Value = ctl::query(&["getoption", "misc:vrr"])
        .context("Reading the current Hyprland VRR policy")?;
    let vrr_policy = option
        .get("int")
        .and_then(serde_json::Value::as_i64)
        .context("Hyprland did not report an integer misc:vrr policy")?;
    Ok(Snapshot {
        displays,
        vrr_policy,
        gamescope_available: cmd::which("gamescope").is_some(),
        steam_available: cmd::which("steam").is_some(),
        session: session_status()?,
    })
}

/// Process-only observation: no display, dependency, or systemd subprocess polling.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStatus {
    pub steam_pids: Vec<u32>,
    pub gamescope_pids: Vec<u32>,
    pub steam_scopes: Vec<u32>,
    pub managed_gamescope_pids: Vec<u32>,
}

impl SessionStatus {
    pub fn steam_state(&self) -> SteamState {
        if !self.steam_scopes.is_empty() {
            SteamState::Gamescope
        } else if !self.steam_pids.is_empty() {
            SteamState::Desktop
        } else {
            SteamState::Stopped
        }
    }

    pub fn managed_transition(&self) -> bool {
        !self.managed_gamescope_pids.is_empty() && self.steam_scopes.is_empty()
    }
}

pub fn session_status() -> Result<SessionStatus> {
    let mut session = processes()?.session_status();
    for &pid in &session.gamescope_pids {
        // Only classify an already-identified Gamescope process. A cgroup name
        // never establishes executable identity or makes unrelated Steam nested.
        if std::fs::read_to_string(format!("/proc/{pid}/cgroup")).is_ok_and(|groups| {
            groups
                .lines()
                .any(|line| line.split('/').any(|component| component == UNIT))
        }) {
            session.managed_gamescope_pids.push(pid);
        }
    }
    Ok(session)
}

#[derive(Debug, Clone)]
pub struct Plan {
    pub output: String,
    pub width: u32,
    pub height: u32,
    pub refresh_rate: f64,
    pub hdr: bool,
    pub adaptive_sync: bool,
    pub argv: Vec<String>,
    pub environment: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

impl Plan {
    /// A copyable preview only. Launch passes argv directly, never through a shell.
    pub fn command_line(&self) -> String {
        self.environment
            .iter()
            .map(|(key, value)| format!("{key}={}", cmd::shell_quote(value)))
            .chain(self.argv.iter().map(|arg| cmd::shell_quote(arg)))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Add the persisted overlay only at real preview/launch boundaries, keeping
    /// the settings planner independent of filesystem state and dependencies.
    pub fn with_overlay(mut self) -> Result<Self> {
        if let Some(environment) = crate::overlay::launch_environment()? {
            let separator = self
                .argv
                .iter()
                .position(|arg| arg == "--")
                .context("Gamescope launch plan has no command separator")?;
            self.argv.insert(separator, "--mangoapp".into());
            self.environment.push(environment);
            // Native file settings must not be shadowed by a desktop-wide
            // MANGOHUD_CONFIG override. Do not inject a second per-game HUD.
            self.environment
                .push(("MANGOHUD_CONFIG".into(), String::new()));
            self.environment.push(("MANGOHUD".into(), "0".into()));
        }
        Ok(self)
    }
}

fn dimension(value: u32, name: &str) -> Result<()> {
    if value == 0 || value > i32::MAX as u32 {
        bail!(
            "{name} must be a positive pixel count no larger than {}",
            i32::MAX
        );
    }
    Ok(())
}

fn toggle(value: Toggle, automatic: bool) -> bool {
    match value {
        Toggle::Auto => automatic,
        Toggle::On => true,
        Toggle::Off => false,
    }
}

pub fn plan(settings: &Settings, snapshot: &Snapshot) -> Result<Plan> {
    let monitor = if let Some(name) = &settings.output {
        snapshot
            .displays
            .iter()
            .find(|m| &m.name == name)
            .with_context(|| {
                format!(
                    "Display {name} is disconnected. Select a connected display or Focused display."
                )
            })?
    } else {
        snapshot.displays.iter().find(|m| m.focused && !m.disabled)
            .context("No focused display is available. Select a connected display explicitly and refresh detection.")?
    };
    if monitor.disabled {
        bail!(
            "Display {} is disabled. Enable it in Displays or select another output.",
            monitor.name
        );
    }
    if !monitor.mirror_of.is_empty() && monitor.mirror_of != "none" {
        bail!(
            "Display {} mirrors another output and cannot independently host a window. Select the source display.",
            monitor.name
        );
    }
    let (width, height, requested_rate) = if settings.auto_display {
        if !(0..=7).contains(&monitor.transform) {
            bail!(
                "Display {} reported an unsupported transform {}. Refresh detection.",
                monitor.name,
                monitor.transform
            );
        }
        let width = u32::try_from(monitor.width)
            .context("Display width is not a valid physical pixel count")?;
        let height = u32::try_from(monitor.height)
            .context("Display height is not a valid physical pixel count")?;
        // Hyprland serializes raw pixelSize; transformedSize swaps odd transforms.
        let (width, height) = if monitor.transform % 2 == 1 {
            (height, width)
        } else {
            (width, height)
        };
        (width, height, monitor.refresh_rate)
    } else {
        (settings.width, settings.height, settings.refresh_rate)
    };
    dimension(width, "Output width")?;
    dimension(height, "Output height")?;
    if !requested_rate.is_finite()
        || requested_rate < 1.0
        || requested_rate > f64::from(i32::MAX / 1000)
    {
        bail!(
            "Refresh rate must be finite and between 1 and {} Hz",
            i32::MAX / 1000
        );
    }
    let refresh_rate = requested_rate.round();
    if (settings.render_width == 0) != (settings.render_height == 0) {
        bail!(
            "Set both render width and render height, or leave both at 0 to use the output resolution."
        );
    }
    let (render_width, render_height) = if settings.render_width == 0 {
        (width, height)
    } else {
        dimension(settings.render_width, "Render width")?;
        dimension(settings.render_height, "Render height")?;
        (settings.render_width, settings.render_height)
    };
    if settings.framerate_limit > (i32::MAX / 1000) as u32 {
        bail!("Framerate limit is too large for Gamescope; use 0 to disable it.");
    }
    if settings.hide_cursor_delay > i32::MAX as u32 {
        bail!(
            "Cursor hide delay must be between 0 and {} milliseconds.",
            i32::MAX
        );
    }
    let sensitivity = settings.mouse_sensitivity as f32;
    if !settings.mouse_sensitivity.is_finite() || !sensitivity.is_finite() || sensitivity <= 0.0 {
        bail!(
            "Mouse sensitivity must be finite, positive, and representable by Gamescope (for example 1.0)."
        );
    }
    if !snapshot.gamescope_available {
        bail!(
            "Gamescope is not installed or not on PATH. Install gamescope, then refresh detection."
        );
    }
    if !snapshot.steam_available {
        bail!(
            "Steam is not installed or not on PATH. Install the native Steam package, then refresh detection."
        );
    }

    let display_hdr = matches!(monitor.color_management_preset.as_str(), "hdr" | "hdredid");
    let hdr = toggle(settings.hdr, display_hdr);
    let adaptive_sync = toggle(settings.adaptive_sync, snapshot.vrr_policy > 0);
    let mut warnings = Vec::new();
    if (requested_rate - refresh_rate).abs() > 0.000_001 {
        warnings.push(format!("Gamescope accepts whole-Hz refresh rates: {requested_rate:.5} Hz will use {refresh_rate:.0} Hz. Your desktop display mode is unchanged."));
    }
    if hdr && !display_hdr {
        warnings.push("HDR is requested, but this display's current Hyprland color-management preset is not HDR. Gamescope cannot enable desktop HDR; configure it in Displays first.".into());
    }
    if adaptive_sync && snapshot.vrr_policy == 0 {
        warnings.push("Adaptive sync is requested, but Hyprland's VRR policy is Off. Gamescope cannot enable desktop VRR; change the policy in Displays first.".into());
    } else if adaptive_sync
        && snapshot.vrr_policy >= 2
        && settings.window_mode != WindowMode::Fullscreen
    {
        warnings.push("Hyprland's VRR policy requires fullscreen content; borderless/windowed mode may not activate adaptive sync.".into());
    }
    if !monitor.dpms_status {
        warnings.push(format!(
            "Display {} is asleep; wake it before using this session.",
            monitor.name
        ));
    }
    if snapshot.session.steam_state() == SteamState::Desktop {
        warnings.push("Desktop Steam is running. Launch requires permission to exit it normally, closing games and interrupting downloads.".into());
    }
    if settings.framerate_limit != 0 {
        warnings.push("Gamescope's framerate cap uses refresh-rate divisors; the actual cap can differ from the requested FPS.".into());
    }
    if settings.grab_keyboard {
        warnings.push("Gamescope 3.16.25's native Wayland backend does not inhibit host keyboard shortcuts. The -g request is passed through, but Hyprland shortcuts remain active.".into());
    }
    if settings.force_grab_cursor {
        warnings.push("Forced cursor capture always uses relative mouse mode, including Steam menus. It can make desktop-style pointing awkward; leave it off for automatic switching unless a game needs it. This does not fix upstream focus or input bugs.".into());
    }
    if settings.window_mode == WindowMode::Fullscreen && !settings.auto_display {
        warnings.push("Native Wayland fullscreen uses the compositor's physical output size; output-size overrides are initial window dimensions, not a desktop modeset.".into());
    }
    warnings.push(format!("Hyprdeck places the nested window on {}. A copied command alone follows your compositor's window-placement rules.", monitor.name));

    let mut argv = vec![
        "gamescope".into(),
        "--backend".into(),
        "wayland".into(),
        "-e".into(),
        "-W".into(),
        width.to_string(),
        "-H".into(),
        height.to_string(),
        "-w".into(),
        render_width.to_string(),
        "-h".into(),
        render_height.to_string(),
        "-r".into(),
        format!("{refresh_rate:.0}"),
        "-F".into(),
        match settings.filter {
            Filter::Linear => "linear",
            Filter::Nearest => "nearest",
            Filter::Fsr => "fsr",
            Filter::Nis => "nis",
        }
        .into(),
    ];
    match settings.window_mode {
        WindowMode::Fullscreen => argv.push("-f".into()),
        WindowMode::Borderless => argv.push("-b".into()),
        WindowMode::Windowed => {}
    }
    if hdr {
        argv.push("--hdr-enabled".into());
    }
    if adaptive_sync {
        argv.push("--adaptive-sync".into());
    }
    if settings.grab_keyboard {
        argv.push("-g".into());
    }
    if settings.force_grab_cursor {
        argv.push("--force-grab-cursor".into());
    }
    argv.extend([
        "--hide-cursor-delay".into(),
        settings.hide_cursor_delay.to_string(),
        "--mouse-sensitivity".into(),
        sensitivity.to_string(),
    ]);
    if settings.framerate_limit != 0 {
        argv.extend([
            "--framerate-limit".into(),
            settings.framerate_limit.to_string(),
        ]);
    }
    argv.extend(["--".into(), "steam".into(), "-gamepadui".into()]);
    Ok(Plan {
        output: monitor.name.clone(),
        width,
        height,
        refresh_rate,
        hdr,
        adaptive_sync,
        argv,
        environment: Vec::new(),
        warnings,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LaunchOutcome {
    Launched,
    Focused,
}

#[derive(Debug)]
struct Process {
    parent: u32,
    executable: String,
}

struct Processes(HashMap<u32, Process>);

impl Processes {
    fn session_status(&self) -> SessionStatus {
        let mut session = SessionStatus {
            steam_scopes: self.steam_scopes(),
            ..SessionStatus::default()
        };
        for (&pid, process) in &self.0 {
            match process.executable.as_str() {
                "steam" => session.steam_pids.push(pid),
                "gamescope" => session.gamescope_pids.push(pid),
                _ => {}
            }
        }
        session.steam_pids.sort_unstable();
        session.gamescope_pids.sort_unstable();
        session
    }

    fn gamescope_parent(&self, pid: u32) -> Option<u32> {
        let mut current = pid;
        for _ in 0..self.0.len() {
            let process = self.0.get(&current)?;
            if process.executable == "gamescope" {
                return Some(current);
            }
            if current == process.parent {
                break;
            }
            current = process.parent;
        }
        None
    }

    fn steam_scopes(&self) -> Vec<u32> {
        let mut scopes: Vec<_> = self
            .0
            .iter()
            .filter(|(_, p)| p.executable == "steam")
            .filter_map(|(&pid, _)| self.gamescope_parent(pid))
            .collect();
        scopes.sort_unstable();
        scopes.dedup();
        scopes
    }

    fn steam_state(&self) -> SteamState {
        let mut desktop = false;
        for (&pid, process) in &self.0 {
            if process.executable != "steam" {
                continue;
            }
            if self.gamescope_parent(pid).is_some() {
                return SteamState::Gamescope;
            }
            desktop = true;
        }
        if desktop {
            SteamState::Desktop
        } else {
            SteamState::Stopped
        }
    }
}

fn processes() -> Result<Processes> {
    let uid = std::fs::metadata("/proc/self")
        .context("Reading current process ownership")?
        .uid();
    let mut result = HashMap::new();
    let mut mapped_windows = None;
    for entry in std::fs::read_dir("/proc").context("Inspecting Steam processes in /proc")? {
        let entry = entry?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = std::fs::metadata(&path) else {
            continue;
        };
        if metadata.uid() != uid {
            // Capability-bearing processes can have root-owned /proc entries
            // despite still running as this user. Check their effective UID.
            let Ok(status) = std::fs::read_to_string(path.join("status")) else {
                continue;
            };
            let effective_uid = status.lines().find_map(|line| {
                line.strip_prefix("Uid:")?
                    .split_whitespace()
                    .nth(1)?
                    .parse::<u32>()
                    .ok()
            });
            if effective_uid != Some(uid) {
                continue;
            }
        }
        let stat = match std::fs::read_to_string(path.join("stat")) {
            Ok(stat) => stat,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => {
                return Err(e)
                    .context("Reading owned process state; Steam detection is unavailable");
            }
        };
        let Some((identity, fields)) = stat.rsplit_once(") ") else {
            continue;
        };
        let mut fields = fields.split_whitespace();
        if fields.next() == Some("Z") {
            continue;
        }
        let Some(parent) = fields.next().and_then(|s| s.parse().ok()) else {
            continue;
        };
        // Non-dumpable unrelated processes may deny exe access. Read identity
        // only for candidates, while retaining every parent for ancestry.
        let name = identity
            .split_once('(')
            .map(|(_, name)| name)
            .unwrap_or_default();
        // Gamescope renames its main thread to gamescope-wl in native Wayland
        // mode. comm is only a candidate filter; exe/argv/window PID establish identity.
        let gamescope_candidate = name.starts_with("gamescope");
        let executable = if name == "steam" || gamescope_candidate {
            match std::fs::read_link(path.join("exe")) {
                Ok(exe) => exe
                    .file_name()
                    .map(|s| {
                        s.to_string_lossy()
                            .trim_end_matches(" (deleted)")
                            .to_owned()
                    })
                    .unwrap_or_default(),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e)
                    if e.kind() == std::io::ErrorKind::PermissionDenied && gamescope_candidate =>
                {
                    // CAP_SYS_NICE may prevent exe inspection. Validate argv0
                    // while the native surface is still starting; a mapped
                    // surface with the same peer PID is the other safe signal.
                    let argv = std::fs::read(path.join("cmdline")).unwrap_or_default();
                    let program = argv
                        .split(|b| *b == 0)
                        .next()
                        .and_then(|s| std::str::from_utf8(s).ok());
                    let identified = program.is_some_and(|p| {
                        Path::new(p).file_name().is_some_and(|n| n == "gamescope")
                    });
                    if !identified {
                        if mapped_windows.is_none() {
                            mapped_windows = Some(clients()?);
                        }
                        if scope_window(mapped_windows.as_ref().unwrap(), &[pid]).is_none() {
                            bail!(
                                "Gamescope PID {pid} cannot be identified through /proc or a native window in this Hyprland session. No new session was started; open or exit it normally and retry."
                            );
                        }
                    }
                    "gamescope".into()
                }
                Err(e) => {
                    return Err(e).context(
                        "Reading Steam/Gamescope executable identity; exit it normally and retry",
                    );
                }
            }
        } else {
            String::new()
        };
        result.insert(pid, Process { parent, executable });
    }
    Ok(Processes(result))
}

#[derive(Debug, Clone, Deserialize)]
struct Client {
    address: String,
    pid: u32,
    class: String,
    monitor: i64,
    mapped: bool,
    #[serde(default, rename = "xwayland")]
    xwayland: bool,
    floating: bool,
    fullscreen: i64,
    #[serde(rename = "fullscreenClient")]
    fullscreen_client: i64,
    size: [i64; 2],
}

fn clients() -> Result<Vec<Client>> {
    ctl::query(&["clients"]).context("Finding the Gamescope window in Hyprland")
}

fn scope_window<'a>(clients: &'a [Client], scopes: &[u32]) -> Option<&'a Client> {
    clients
        .iter()
        .find(|c| c.mapped && !c.xwayland && c.class == "gamescope" && scopes.contains(&c.pid))
}

fn selector(client: &Client) -> Result<String> {
    let address = client.address.strip_prefix("0x").unwrap_or(&client.address);
    if address.is_empty() || !address.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Hyprland returned an invalid Gamescope window address; refresh detection.");
    }
    Ok(format!("address:0x{address}"))
}

fn focus(client: &Client) -> Result<()> {
    ctl::dispatch(&format!(
        "hl.dsp.focus({{ window = {} }})",
        ctl::lua_str(&selector(client)?)
    ))
    .context("Focusing the existing Steam Gamescope window")
}

fn focus_existing(processes: &Processes) -> Result<LaunchOutcome> {
    let scopes = processes.steam_scopes();
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(client) = scope_window(&clients()?, &scopes) {
            focus(client)?;
            return Ok(LaunchOutcome::Focused);
        }
        if Instant::now() >= deadline {
            bail!(
                "Steam is already inside Gamescope, but its native Wayland window is not mapped in this Hyprland session. No duplicate was launched. Open the existing session or exit it normally and retry."
            );
        }
        std::thread::sleep(POLL);
    }
}

fn lock() -> Result<File> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .context("XDG_RUNTIME_DIR is missing; launch inside your graphical user session")?;
    let path = Path::new(&runtime).join("hyprdeck-gamescope.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("Opening launch lock {}", path.display()))?;
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::WouldBlock) if Instant::now() < deadline => std::thread::sleep(POLL),
            Err(TryLockError::WouldBlock) => bail!(
                "Another Steam Gamescope launch is still in progress. Wait for it, then choose Open existing."
            ),
            Err(TryLockError::Error(e)) => return Err(e).context("Locking Steam Gamescope launch"),
        }
    }
}

#[derive(Default)]
struct UnitState {
    active: String,
    load: String,
    result: String,
    status: String,
    pid: u32,
}

fn unit_state() -> Result<UnitState> {
    let out = cmd::output(
        "systemctl",
        [
            "--user",
            "show",
            UNIT,
            "--property=LoadState,ActiveState,Result,ExecMainStatus,MainPID",
        ],
    )
    .context("Querying the managed Gamescope user service")?;
    let mut state = UnitState::default();
    for line in out.stdout.lines() {
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        match name {
            "LoadState" => state.load = value.into(),
            "ActiveState" => state.active = value.into(),
            "Result" => state.result = value.into(),
            "ExecMainStatus" => state.status = value.into(),
            "MainPID" => state.pid = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    if state.load != "not-found" && !out.ok() || state.load.is_empty() {
        bail!(
            "Cannot contact your systemd user manager: {}. Launch inside the graphical login session.",
            out.stderr.trim()
        );
    }
    Ok(state)
}

fn launch_error(reason: &str, state: &UnitState) -> anyhow::Error {
    let log = cmd::run(
        "journalctl",
        [
            "--user",
            "-u",
            UNIT,
            "-b",
            "-n",
            "25",
            "--no-pager",
            "-o",
            "cat",
        ],
    )
    .unwrap_or_default();
    anyhow::anyhow!(
        "{reason} (service {}, result {}, exit {}). Inspect journalctl --user -u {UNIT} -b.\n{}",
        state.active,
        state.result,
        state.status,
        log.trim()
    )
}

fn shutdown_steam() -> Result<()> {
    // This is a request to Steam, not a signal to a process or a process-name kill.
    let program = cmd::which("steam")
        .context("Steam's shutdown command is unavailable; exit Steam normally and retry")?;
    let mut child = Command::new(program)
        .arg("-shutdown")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Requesting normal Steam shutdown")?;
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut command_finished = false;
    loop {
        if !command_finished
            && let Some(status) = child
                .try_wait()
                .context("Waiting for Steam's shutdown request")?
        {
            if !status.success() {
                bail!(
                    "Steam's normal shutdown request failed ({status}). Exit Steam using Steam → Exit, then retry. No processes were killed."
                );
            }
            command_finished = true;
        }
        match processes()?.steam_state() {
            SteamState::Stopped if command_finished => return Ok(()),
            SteamState::Gamescope => bail!(
                "Steam entered another Gamescope session during shutdown. No additional session was launched; choose Open existing."
            ),
            _ => {}
        }
        if Instant::now() >= deadline {
            bail!(
                "Steam did not exit normally within 30 seconds. Save/close games and use Steam → Exit, then retry. No processes were killed."
            );
        }
        std::thread::sleep(POLL);
    }
}

fn start_service(plan: &Plan) -> Result<()> {
    let gamescope =
        cmd::which("gamescope").context("Gamescope disappeared from PATH; refresh detection")?;
    let steam = cmd::which("steam").context("Steam disappeared from PATH; refresh detection")?;
    let mut args = vec![
        "--user".to_owned(),
        format!("--unit={UNIT}"),
        "--property=Type=exec".into(),
        "--property=KillMode=control-group".into(),
        "--description=Hyprdeck Steam Gamescope".into(),
        "--expand-environment=no".into(),
    ];
    // Preserve the caller's graphical/GPU environment, not the manager's stale
    // login environment. Do not inherit another service's notification/fd state.
    for (key, _) in std::env::vars_os() {
        let Some(key) = key.to_str() else {
            continue;
        };
        if matches!(
            key,
            "INVOCATION_ID"
                | "JOURNAL_STREAM"
                | "NOTIFY_SOCKET"
                | "SYSTEMD_EXEC_PID"
                | "MANAGERPID"
        ) || key.starts_with("WATCHDOG_")
            || key.starts_with("LISTEN_")
        {
            continue;
        }
        if plan
            .environment
            .iter()
            .any(|(override_key, _)| override_key == key)
        {
            continue;
        }
        args.push(format!("--setenv={key}"));
    }
    for (key, value) in &plan.environment {
        args.push(format!("--setenv={key}={value}"));
    }
    args.push("--".into());
    args.push(gamescope);
    args.extend_from_slice(&plan.argv[1..plan.argv.len() - 2]);
    args.extend([steam, "-gamepadui".into()]);
    cmd::run("systemd-run", &args).context(
        "Starting the managed Gamescope service; ensure your systemd user manager is running",
    )?;
    Ok(())
}

fn window_mode_matches(client: &Client, mode: WindowMode) -> bool {
    match mode {
        WindowMode::Fullscreen => client.fullscreen == 2 && client.fullscreen_client == 2,
        WindowMode::Borderless | WindowMode::Windowed => {
            client.fullscreen == 0 && client.fullscreen_client == 0 && client.floating
        }
    }
}

fn apply_window_mode(client: &Client, mode: WindowMode, size: [i64; 2]) -> Result<()> {
    let window = ctl::lua_str(&selector(client)?);
    let fullscreen = if mode == WindowMode::Fullscreen { 2 } else { 0 };
    ctl::dispatch(&format!("hl.dsp.window.fullscreen_state({{ internal = {fullscreen}, client = {fullscreen}, action = \"set\", window = {window} }})"))
        .context("Applying the selected Gamescope fullscreen state; the session was left running")?;
    if mode != WindowMode::Fullscreen {
        ctl::dispatch(&format!("hl.dsp.window.float({{ action = \"on\", window = {window} }})"))
            .context("Floating the Gamescope window for windowed/borderless mode; the session was left running")?;
        ctl::dispatch(&format!(
            "hl.dsp.window.resize({{ x = {}, y = {}, relative = false, window = {window} }})",
            size[0], size[1]
        ))
        .context(
            "Applying the selected Gamescope window dimensions; the session was left running",
        )?;
    }
    Ok(())
}

fn observe_launch(plan: &Plan, mode: WindowMode) -> Result<LaunchOutcome> {
    let deadline = Instant::now() + Duration::from_secs(45);
    let mut moved = None;
    let mut configured = None;
    let mut ready_since = None;
    loop {
        let state = unit_state()?;
        if !matches!(state.active.as_str(), "active" | "activating") {
            return Err(launch_error(
                "Gamescope exited before its Steam window was ready",
                &state,
            ));
        }
        let running = processes()?;
        let windows = clients()?;
        if state.pid != 0
            && let Some(client) = scope_window(&windows, &[state.pid])
        {
            let monitors = ctl::monitors().context("Checking Gamescope's target display")?;
            let target = monitors.iter().find(|m| m.name == plan.output && !m.disabled)
                    .with_context(|| format!("Display {} disconnected while Gamescope was starting. The session was left running; open it or exit Steam normally.", plan.output))?;
            if client.monitor != target.id && moved.as_deref() != Some(client.address.as_str()) {
                ctl::dispatch(&format!("hl.dsp.window.move({{ monitor = {}, window = {}, follow = true }})",
                        ctl::lua_str(&plan.output), ctl::lua_str(&selector(client)?)))
                        .context("Moving the Steam Gamescope window to the selected display; the session was left running")?;
                moved = Some(client.address.clone());
            }
            if !target.scale.is_finite() || target.scale <= 0.0 {
                bail!(
                    "Display {} reported an invalid scale. The Gamescope session was left running; refresh display detection.",
                    plan.output
                );
            }
            let logical_size = [
                (f64::from(plan.width) / target.scale).round().max(1.0) as i64,
                (f64::from(plan.height) / target.scale).round().max(1.0) as i64,
            ];
            if client.monitor == target.id && configured.as_deref() != Some(client.address.as_str())
            {
                // Override this window's initial gaming-app rule, not the
                // user's permanent rules or any other Steam/game surface.
                apply_window_mode(client, mode, logical_size)?;
                configured = Some(client.address.clone());
            }
            let size_matches = mode == WindowMode::Fullscreen || client.size == logical_size;
            if client.monitor == target.id
                && window_mode_matches(client, mode)
                && size_matches
                && running.steam_scopes().contains(&state.pid)
            {
                let since = ready_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= Duration::from_millis(800) {
                    focus(client)?;
                    return Ok(LaunchOutcome::Launched);
                }
            } else {
                ready_since = None;
            }
        }
        if Instant::now() >= deadline {
            return Err(launch_error(
                "Gamescope is still running, but Steam's associated native window, selected output, or window mode/dimensions could not be verified within 45 seconds. No duplicate was started. Check Steam/update dialogs and conflicting window rules; choose Open existing after it finishes starting",
                &state,
            ));
        }
        std::thread::sleep(POLL);
    }
}

/// Opening is deliberately separate from launching: if the live banner races
/// with an external exit, never replace that session with a new defaults launch.
pub fn open_existing() -> Result<LaunchOutcome> {
    let _local = LAUNCH_LOCK.lock().map_err(|_| {
        anyhow::anyhow!("Steam Gamescope launch lock was interrupted; restart Hyprdeck and retry")
    })?;
    let _file = lock()?;
    let running = processes()?;
    if running.steam_state() != SteamState::Gamescope {
        bail!(
            "The existing Steam Gamescope session has exited. No new session was started; review your launch settings and choose Launch Big Picture."
        );
    }
    focus_existing(&running)
}

pub fn launch(settings: &Settings, restart_steam: bool) -> Result<LaunchOutcome> {
    let _local = LAUNCH_LOCK.lock().map_err(|_| {
        anyhow::anyhow!("Steam Gamescope launch lock was interrupted; restart Hyprdeck and retry")
    })?;
    let _file = lock()?;
    let running = processes()?;
    let steam_state = running.steam_state();
    if steam_state == SteamState::Gamescope {
        return focus_existing(&running);
    }
    if steam_state == SteamState::Desktop && !restart_steam {
        bail!(
            "Steam is already running outside Gamescope. Exit it normally, or confirm Restart Steam; this closes games and interrupts downloads. No processes were changed."
        );
    }
    for program in ["systemd-run", "systemctl"] {
        if cmd::which(program).is_none() {
            bail!(
                "{program} is unavailable. Install systemd user-session tools before launching Gamescope."
            );
        }
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() {
        bail!(
            "WAYLAND_DISPLAY is missing. Launch inside your Hyprland Wayland session; standalone DRM launch is not supported."
        );
    }
    let state = unit_state()?;
    if matches!(
        state.active.as_str(),
        "active" | "activating" | "deactivating"
    ) {
        bail!(
            "The managed Gamescope session is already running or stopping, but associated Steam is not ready. Inspect journalctl --user -u {UNIT} -b and wait; no duplicate was launched."
        );
    }
    let mut launch_plan = plan(settings, &snapshot()?)?.with_overlay()?;
    settings
        .save()
        .context("Saving Gamescope choices before launch")?;
    if steam_state == SteamState::Desktop {
        shutdown_steam()?;
        // Output focus and policy may have changed while Steam closed.
        launch_plan = plan(settings, &snapshot()?)?.with_overlay()?;
    }
    let running = processes()?;
    match running.steam_state() {
        SteamState::Gamescope => return focus_existing(&running),
        SteamState::Desktop => bail!(
            "Desktop Steam started during launch. Exit it normally and retry; no additional session was started."
        ),
        SteamState::Stopped => {}
    }
    if state.active == "failed" {
        cmd::systemctl_user(["reset-failed", UNIT])
            .context("Clearing the previous failed Gamescope launch state")?;
    }
    start_service(&launch_plan)?;
    observe_launch(&launch_plan, settings.window_mode)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn monitor() -> ctl::Monitor {
        serde_json::from_value(serde_json::json!({
            "id": 1, "name": "DP-1", "description": "Test", "make": "", "model": "", "serial": "",
            "width": 3840, "height": 2160, "physicalWidth": 600, "physicalHeight": 340,
            "refreshRate": 119.88, "x": 0, "y": 0, "scale": 1.5, "transform": 0, "focused": true,
            "dpmsStatus": true, "vrr": false, "disabled": false, "currentFormat": "XRGB2101010",
            "mirrorOf": "none", "availableModes": [], "colorManagementPreset": "hdr"
        }))
        .unwrap()
    }

    fn detected() -> Snapshot {
        Snapshot {
            displays: vec![monitor()],
            vrr_policy: 2,
            gamescope_available: true,
            steam_available: true,
            session: SessionStatus::default(),
        }
    }

    #[test]
    fn fractional_refresh_is_explicitly_rounded_without_scaling_physical_pixels() {
        let plan = plan(&Settings::default(), &detected()).unwrap();
        assert_eq!((plan.width, plan.height), (3840, 2160));
        assert_eq!(plan.refresh_rate, 120.0);
        assert!(plan.argv.windows(2).any(|p| p == ["-r", "120"]));
    }

    #[test]
    fn rotated_and_reflected_outputs_swap_only_odd_transforms() {
        let mut snapshot = detected();
        for transform in 0..=7 {
            snapshot.displays[0].transform = transform;
            let plan = plan(&Settings::default(), &snapshot).unwrap();
            let expected = if transform % 2 == 1 {
                (2160, 3840)
            } else {
                (3840, 2160)
            };
            assert_eq!((plan.width, plan.height), expected);
        }
    }

    #[test]
    fn missing_disabled_and_mirrored_explicit_output_never_falls_back() {
        let mut settings = Settings {
            output: Some("HDMI-A-1".into()),
            ..Settings::default()
        };
        let mut snapshot = detected();
        assert!(plan(&settings, &snapshot).is_err());
        settings.output = Some("DP-1".into());
        snapshot.displays[0].disabled = true;
        assert!(plan(&settings, &snapshot).is_err());
        snapshot.displays[0].disabled = false;
        snapshot.displays[0].mirror_of = "0".into();
        assert!(plan(&settings, &snapshot).is_err());
    }

    #[test]
    fn manual_output_and_toggle_overrides_take_precedence_over_detected_settings() {
        let settings = Settings {
            auto_display: false,
            width: 2560,
            height: 1440,
            refresh_rate: 59.94,
            render_width: 1920,
            render_height: 1080,
            window_mode: WindowMode::Borderless,
            filter: Filter::Fsr,
            hdr: Toggle::Off,
            adaptive_sync: Toggle::Off,
            grab_keyboard: true,
            framerate_limit: 30,
            ..Settings::default()
        };
        let plan = plan(&settings, &detected()).unwrap();
        assert_eq!(
            (plan.width, plan.height, plan.refresh_rate),
            (2560, 1440, 60.0)
        );
        assert!(!plan.hdr && !plan.adaptive_sync);
    }

    #[test]
    fn auto_uses_hdr_preset_and_vrr_policy_not_ten_bit_format_or_active_vrr() {
        let mut snapshot = detected();
        let automatic = plan(&Settings::default(), &snapshot).unwrap();
        assert!(automatic.hdr && automatic.adaptive_sync);
        snapshot.displays[0].color_management_preset = "srgb".into();
        snapshot.vrr_policy = 0;
        snapshot.displays[0].vrr = true;
        let automatic = plan(&Settings::default(), &snapshot).unwrap();
        assert!(!automatic.hdr && !automatic.adaptive_sync);
        let forced = plan(
            &Settings {
                hdr: Toggle::On,
                adaptive_sync: Toggle::On,
                ..Settings::default()
            },
            &snapshot,
        )
        .unwrap();
        assert!(forced.hdr && forced.adaptive_sync);
    }

    #[test]
    fn invalid_refresh_and_half_render_override_are_not_passed_to_gamescope() {
        let mut settings = Settings {
            auto_display: false,
            ..Settings::default()
        };
        for rate in [f64::NAN, f64::INFINITY, 0.0, -60.0] {
            settings.refresh_rate = rate;
            assert!(plan(&settings, &detected()).is_err());
        }
        settings.refresh_rate = 60.0;
        settings.render_width = 1920;
        assert!(plan(&settings, &detected()).is_err());
    }

    #[test]
    fn compositor_fullscreen_rule_cannot_count_as_success_for_windowed_modes() {
        let mut client = Client {
            address: "0x1234".into(),
            pid: 20,
            class: "gamescope".into(),
            monitor: 1,
            mapped: true,
            xwayland: false,
            floating: false,
            fullscreen: 2,
            fullscreen_client: 2,
            size: [3840, 2160],
        };
        assert!(window_mode_matches(&client, WindowMode::Fullscreen));
        assert!(!window_mode_matches(&client, WindowMode::Windowed));
        assert!(!window_mode_matches(&client, WindowMode::Borderless));
        client.fullscreen = 0;
        client.floating = true;
        assert!(!window_mode_matches(&client, WindowMode::Windowed));
        client.fullscreen_client = 0;
        assert!(window_mode_matches(&client, WindowMode::Windowed));
        assert!(window_mode_matches(&client, WindowMode::Borderless));
        client.floating = false;
        assert!(!window_mode_matches(&client, WindowMode::Windowed));
    }

    #[test]
    fn unrelated_gamescope_does_not_turn_desktop_steam_into_a_nested_session() {
        let mut processes = Processes(HashMap::from([
            (
                10,
                Process {
                    parent: 1,
                    executable: "steam".into(),
                },
            ),
            (
                20,
                Process {
                    parent: 1,
                    executable: "gamescope".into(),
                },
            ),
            (
                21,
                Process {
                    parent: 20,
                    executable: "game".into(),
                },
            ),
        ]));
        assert_eq!(processes.steam_state(), SteamState::Desktop);
        processes.0.get_mut(&10).unwrap().parent = 21;
        assert_eq!(processes.steam_state(), SteamState::Gamescope);
    }

    #[test]
    fn live_session_tracks_external_start_exit_and_multiple_steam_processes() {
        let mut processes = Processes(HashMap::new());
        assert_eq!(
            processes.session_status().steam_state(),
            SteamState::Stopped
        );
        processes.0.insert(
            30,
            Process {
                parent: 1,
                executable: "steam".into(),
            },
        );
        assert_eq!(
            processes.session_status().steam_state(),
            SteamState::Desktop
        );
        processes.0.insert(
            10,
            Process {
                parent: 1,
                executable: "gamescope".into(),
            },
        );
        processes.0.insert(
            11,
            Process {
                parent: 10,
                executable: String::new(),
            },
        );
        processes.0.insert(
            20,
            Process {
                parent: 11,
                executable: "steam".into(),
            },
        );
        processes.0.insert(
            21,
            Process {
                parent: 20,
                executable: "steam".into(),
            },
        );
        let session = processes.session_status();
        assert_eq!(session.steam_state(), SteamState::Gamescope);
        assert_eq!(session.steam_pids, [20, 21, 30]);
        assert_eq!(session.gamescope_pids, [10]);
        assert_eq!(session.steam_scopes, [10]);
        processes.0.remove(&20);
        processes.0.remove(&21);
        assert_eq!(
            processes.session_status().steam_state(),
            SteamState::Desktop
        );
        processes.0.remove(&30);
        assert_eq!(
            processes.session_status().steam_state(),
            SteamState::Stopped
        );
    }

    #[test]
    fn managed_gamescope_without_associated_steam_is_a_transition_not_openable() {
        let mut session = SessionStatus {
            gamescope_pids: vec![10],
            managed_gamescope_pids: vec![10],
            ..SessionStatus::default()
        };
        assert!(session.managed_transition());
        assert_eq!(session.steam_state(), SteamState::Stopped);
        session.steam_pids.push(20);
        assert!(session.managed_transition());
        assert_eq!(session.steam_state(), SteamState::Desktop);
        session.steam_scopes.push(10);
        assert!(!session.managed_transition());
        assert_eq!(session.steam_state(), SteamState::Gamescope);
    }

    #[test]
    fn broken_or_cyclic_ancestry_never_identifies_a_steam_host() {
        let processes = Processes(HashMap::from([
            (
                10,
                Process {
                    parent: 11,
                    executable: "steam".into(),
                },
            ),
            (
                11,
                Process {
                    parent: 10,
                    executable: String::new(),
                },
            ),
            (
                20,
                Process {
                    parent: 99,
                    executable: "steam".into(),
                },
            ),
            (
                30,
                Process {
                    parent: 1,
                    executable: "gamescope".into(),
                },
            ),
        ]));
        let session = processes.session_status();
        assert_eq!(session.steam_state(), SteamState::Desktop);
        assert!(session.steam_scopes.is_empty());
        assert_eq!(session.steam_pids, [10, 20]);
    }

    #[test]
    fn invalid_mouse_values_and_integer_overflow_never_reach_gamescope() {
        let mut settings = Settings::default();
        for sensitivity in [
            0.0,
            -1.0,
            f64::NAN,
            f64::INFINITY,
            f64::MAX,
            f64::MIN_POSITIVE,
        ] {
            settings.mouse_sensitivity = sensitivity;
            assert!(plan(&settings, &detected()).is_err());
        }
        settings.mouse_sensitivity = 1.0;
        settings.hide_cursor_delay = i32::MAX as u32;
        assert!(plan(&settings, &detected()).is_ok());
        settings.hide_cursor_delay += 1;
        assert!(plan(&settings, &detected()).is_err());
    }

    #[test]
    fn preview_environment_quotes_only_the_assignment_value() {
        let mut plan = plan(&Settings::default(), &detected()).unwrap();
        plan.environment.push((
            "MANGOHUD_CONFIGFILE".into(),
            "/a profile/overlay.conf".into(),
        ));
        assert!(
            plan.command_line()
                .starts_with("MANGOHUD_CONFIGFILE='/a profile/overlay.conf' gamescope ")
        );
    }
}
