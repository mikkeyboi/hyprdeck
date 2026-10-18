//! Isolated, opt-in 3D preview. Capture full composition: PipeWire deliberately
//! excludes external overlays, so it cannot preview MangoApp faithfully.
use std::io;
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use hyprdeck_core::{cmd, rt, store};
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle, protocol::wl_registry};

#[allow(clippy::all)]
mod protocol {
    use wayland_client;
    pub mod __interfaces {
        wayland_scanner::generate_interfaces!("protocol/gamescope-control.xml");
    }
    use self::__interfaces::*;
    wayland_scanner::generate_client_code!("protocol/gamescope-control.xml");
}
use protocol::gamescope_control::{self, GamescopeControl};

pub enum Message {
    Frame(Vec<u8>),
    Error(String),
}

pub struct Preview {
    cancel: Arc<AtomicBool>,
    pub config_path: PathBuf,
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub fn start(config: String) -> Result<(Preview, async_channel::Receiver<Message>)> {
    super::overlay::validate(&config)?;
    super::overlay::require_mangoapp()?;
    for program in ["gamescope", "glxgears"] {
        if cmd::which(program).is_none() {
            bail!("The live preview needs {program} on PATH. Install gamescope and mesa-utils.");
        }
    }
    let dir = tempfile::Builder::new()
        .prefix("hyprdeck-overlay-")
        .tempdir()?;
    let config_path = dir.path().join("MangoHud.conf");
    store::write_atomic(&config_path, config.as_bytes())?;
    let cancel = Arc::new(AtomicBool::new(false));
    let (tx, rx) = async_channel::bounded(1);
    let stop = cancel.clone();
    rt::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(error) = run(&dir, stop, &tx) {
                let _ = tx.send_blocking(Message::Error(format!("{error:#}")));
            }
        })
        .await;
    });
    Ok((
        Preview {
            cancel,
            config_path,
        },
        rx,
    ))
}

struct ProcessGroup(Child);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // This group was created exclusively for our preview, never Steam.
        // Gamescope's watchdog also cleans up its own Xwayland/MangoApp clients.
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGTERM);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while self.0.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        unsafe {
            libc::kill(-(self.0.id() as i32), libc::SIGKILL);
        }
        let _ = self.0.wait();
    }
}

#[derive(Default)]
struct State {
    control: Option<GamescopeControl>,
    screenshot: bool,
}
impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
            && interface == "gamescope_control"
            && version >= 3
        {
            state.control = Some(registry.bind(name, 3, qh, ()));
        }
    }
}
impl Dispatch<GamescopeControl, ()> for State {
    fn event(
        state: &mut Self,
        _: &GamescopeControl,
        event: gamescope_control::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let gamescope_control::Event::ScreenshotTaken { .. } = event {
            state.screenshot = true;
        }
    }
}

fn dispatch_until(
    queue: &mut EventQueue<State>,
    state: &mut State,
    stop: &AtomicBool,
    done: impl Fn(&State) -> bool,
) -> Result<bool> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        queue.dispatch_pending(state)?;
        if done(state) {
            return Ok(true);
        }
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        if Instant::now() >= deadline {
            bail!("Gamescope did not respond to a preview capture request within 5 seconds.");
        }
        queue.flush()?;
        if let Some(guard) = queue.prepare_read() {
            use std::os::fd::AsRawFd;
            let mut fd = libc::pollfd {
                fd: guard.connection_fd().as_fd().as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let result = unsafe { libc::poll(&mut fd, 1, 100) };
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error.into());
                }
            } else if result > 0 {
                guard.read()?;
            }
        }
    }
}

/// Gamescope 3.16 uses ftok("mangoapp", 65). Without a real, private key
/// file, simultaneous sessions share a queue and report each other's timings.
struct TimingQueue(i32);
impl TimingQueue {
    fn new(dir: &std::path::Path) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let path = dir.join("mangoapp");
        std::fs::File::create(&path)?;
        let path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
        let key = unsafe { libc::ftok(path.as_ptr(), 65) };
        if key == -1 {
            return Err(io::Error::last_os_error()).context("Creating the preview timing key");
        }
        let id = unsafe { libc::msgget(key, libc::IPC_CREAT | libc::IPC_EXCL | 0o600) };
        if id == -1 {
            return Err(io::Error::last_os_error())
                .context("Creating an isolated preview timing queue");
        }
        Ok(Self(id))
    }
}
impl Drop for TimingQueue {
    fn drop(&mut self) {
        unsafe {
            libc::msgctl(self.0, libc::IPC_RMID, std::ptr::null_mut());
        }
    }
}

fn run(
    dir: &tempfile::TempDir,
    stop: Arc<AtomicBool>,
    tx: &async_channel::Sender<Message>,
) -> Result<()> {
    let log_path = dir.path().join("gamescope.log");
    let log = std::fs::File::create(&log_path)?;
    let _timing_queue = TimingQueue::new(dir.path())?;
    let parent = unsafe { libc::getpid() };
    let mut command = Command::new("gamescope");
    command
        .args([
            "--backend",
            "headless",
            "-W",
            "960",
            "-H",
            "540",
            "-w",
            "960",
            "-h",
            "540",
            "-r",
            "30",
            "--framerate-limit",
            "30",
            "--mangoapp",
            "--",
            "glxgears",
            "-geometry",
            "960x540",
        ])
        .env_remove("MANGOHUD_CONFIG")
        .env_remove("MANGOHUD")
        .env("MANGOHUD_CONFIGFILE", dir.path().join("MangoHud.conf"))
        .env_remove("MANGOAPP_MSG_TYPE")
        .current_dir(dir.path())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(log)
        .process_group(0);
    // Ensure the isolated compositor cannot outlive an abrupt Hyprdeck exit.
    unsafe {
        command.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::getppid() != parent {
                return Err(io::Error::from_raw_os_error(libc::ECHILD));
            }
            Ok(())
        });
    }
    let child = command
        .spawn()
        .context("Starting the isolated 3D overlay preview")?;
    let mut process = ProcessGroup(child);
    let deadline = Instant::now() + Duration::from_secs(10);
    let socket = loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        let log = std::fs::read_to_string(&log_path)?;
        let display = log.lines().find_map(|line| {
            line.split_once("Running compositor on wayland display '")
                .and_then(|(_, value)| value.strip_suffix('\''))
        });
        if let Some(display) = display
            && log.contains("vblank:")
        {
            break PathBuf::from(
                std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is missing")?,
            )
            .join(display);
        }
        if process.0.try_wait()?.is_some() || Instant::now() >= deadline {
            bail!("The Gamescope preview could not start.\n{log}");
        }
        std::thread::sleep(Duration::from_millis(100));
    };
    let connection = Connection::from_socket(UnixStream::connect(socket)?)?;
    let mut queue = connection.new_event_queue();
    let _registry = connection.display().get_registry(&queue.handle(), ());
    let mut state = State::default();
    if !dispatch_until(&mut queue, &mut state, &stop, |s| s.control.is_some())? {
        return Ok(());
    }
    let control = state
        .control
        .clone()
        .context("Gamescope screenshot protocol v3 is unavailable")?;
    let frame = dir.path().join("frame.png");
    while !stop.load(Ordering::Relaxed) {
        state.screenshot = false;
        control.take_screenshot(
            frame.to_string_lossy().into_owned(),
            gamescope_control::ScreenshotType::FullComposition,
            gamescope_control::ScreenshotFlags::empty(),
        );
        if !dispatch_until(&mut queue, &mut state, &stop, |s| s.screenshot)? {
            break;
        }
        let bytes = std::fs::read(&frame)?;
        if tx.try_send(Message::Frame(bytes)).is_err() && tx.is_closed() {
            break;
        }
        // Five captures/sec is sufficient for layout editing. The 3D demo itself
        // runs at 30 FPS; this is not a benchmark or a promise about game FPS.
        for _ in 0..10 {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    Ok(())
}
