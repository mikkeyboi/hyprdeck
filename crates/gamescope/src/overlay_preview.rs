//! Isolated, opt-in 3D preview. Capture full composition: PipeWire deliberately
//! excludes external overlays, so it cannot preview MangoApp faithfully.
//! Headless refresh paces the real demo; independently paced PNG readbacks are
//! limited to 60 Hz. The control protocol exposes no full-composition stream.
use std::io::{self, BufReader};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc, Mutex,
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

// All sizes preserve 16:9; smaller readbacks trade fidelity for smoother capture.
pub const SIZES: [(i32, i32); 3] = [(480, 270), (640, 360), (960, 540)];
const MAX_CAPTURE_FPS: u32 = 60;

pub enum Message {
    /// Unpremultiplied RGBA8 pixels, decoded on the worker, with width * 4 stride.
    Frame {
        width: i32,
        height: i32,
        pixels: Vec<u8>,
    },
    Error(String),
}

pub struct Preview {
    cancel: Arc<AtomicBool>,
    pending_config: Arc<Mutex<Option<String>>>,
}
impl Preview {
    pub fn update(&self, config: String) -> Result<()> {
        super::overlay::validate(&config)?;
        *self
            .pending_config
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(config);
        Ok(())
    }
}
impl Drop for Preview {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}

pub fn start(
    config: String,
    refresh_rate: u32,
    quality: usize,
) -> Result<(Preview, async_channel::Receiver<Message>)> {
    let refresh_rate = refresh_rate.clamp(30, 360);
    let dimensions = *SIZES.get(quality).context("Unknown preview quality")?;
    super::overlay::validate(&config)?;
    super::overlay::require_mangoapp()?;
    for program in ["gamescope", "glxgears"] {
        if cmd::which(program).is_none() {
            bail!("The live preview needs {program} on PATH. Install gamescope and mesa-utils.");
        }
    }
    // Runtime directories are normally tmpfs: repeated screenshot writes should
    // not hit persistent storage. Keep the config and private IPC key here too.
    let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR").context("XDG_RUNTIME_DIR is missing")?;
    let dir = tempfile::Builder::new()
        .prefix("hyprdeck-overlay-")
        .tempdir_in(runtime_dir)?;
    let config_path = dir.path().join("MangoHud.conf");
    store::write_atomic(&config_path, config.as_bytes())?;
    let cancel = Arc::new(AtomicBool::new(false));
    let pending_config = Arc::new(Mutex::new(None));
    let updates = pending_config.clone();
    let (tx, rx) = async_channel::bounded(1);
    let stop = cancel.clone();
    rt::spawn(async move {
        let _ = tokio::task::spawn_blocking(move || {
            if let Err(error) = run(&dir, refresh_rate, &stop, &tx, &updates, dimensions)
                && !stop.load(Ordering::Relaxed)
            {
                // An error must not block shutdown behind a queued image.
                let _ = tx.force_send(Message::Error(format!("{error:#}")));
            }
        })
        .await;
    });
    Ok((
        Preview {
            cancel,
            pending_config,
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
        if stop.load(Ordering::Relaxed) {
            return Ok(false);
        }
        queue.dispatch_pending(state)?;
        if done(state) {
            return Ok(true);
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
    refresh_rate: u32,
    stop: &AtomicBool,
    tx: &async_channel::Sender<Message>,
    updates: &Mutex<Option<String>>,
    dimensions: (i32, i32),
) -> Result<()> {
    if stop.load(Ordering::Relaxed) || tx.is_closed() {
        return Ok(());
    }
    let log_path = dir.path().join("gamescope.log");
    let log = std::fs::File::create(&log_path)?;
    let _timing_queue = TimingQueue::new(dir.path())?;
    let parent = unsafe { libc::getpid() };
    let width = dimensions.0.to_string();
    let height = dimensions.1.to_string();
    let mut command = Command::new("gamescope");
    command
        .args([
            "--backend",
            "headless",
            "-W",
            &width,
            "-H",
            &height,
            "-w",
            &width,
            "-h",
            &height,
            // A virtual display refresh, not a separate application FPS cap.
            // Keep glxgears' normal vblank pacing rather than disabling vsync
            // and needlessly saturating the GPU with this tiny demo.
            "-r",
            &refresh_rate.to_string(),
            "--mangoapp",
            "--",
            "glxgears",
            "-geometry",
            &format!("{width}x{height}"),
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
    if !dispatch_until(&mut queue, &mut state, stop, |s| s.control.is_some())? {
        return Ok(());
    }
    let control = state
        .control
        .clone()
        .context("Gamescope screenshot protocol v3 is unavailable")?;
    let frame = dir.path().join("frame.png");
    let frame_path = frame.to_string_lossy().into_owned();
    let config_path = dir.path().join("MangoHud.conf");
    let interval = Duration::from_secs_f64(1.0 / refresh_rate.min(MAX_CAPTURE_FPS) as f64);
    let mut next_capture = Instant::now();
    while !stop.load(Ordering::Relaxed) && !tx.is_closed() {
        // Count readback and decoding against the interval, not on top of it.
        // Rescheduling from now skips missed deadlines without catch-up bursts.
        while let Some(delay) = next_capture.checked_duration_since(Instant::now()) {
            if stop.load(Ordering::Relaxed) || tx.is_closed() {
                return Ok(());
            }
            std::thread::sleep(delay.min(Duration::from_millis(20)));
        }
        next_capture = Instant::now() + interval;
        // The renderer worker owns the directory and all config writes. Updates
        // coalesce to the newest draft and cannot recreate a stopped preview.
        let config = updates
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        if let Some(config) = config {
            store::write_atomic(&config_path, config.as_bytes())?;
        }
        // One in-flight request and one delivered frame at most. Do not pay for
        // composition, PNG encoding or decoding while GTK is still catching up.
        if tx.is_full() {
            continue;
        }
        if process.0.try_wait()?.is_some() {
            bail!("The Gamescope preview exited unexpectedly.");
        }
        state.screenshot = false;
        control.take_screenshot(
            frame_path.clone(),
            gamescope_control::ScreenshotType::FullComposition,
            gamescope_control::ScreenshotFlags::empty(),
        );
        if !dispatch_until(&mut queue, &mut state, stop, |s| s.screenshot)? {
            break;
        }
        let pixels = decode_frame(&frame, dimensions)?;
        if tx
            .try_send(Message::Frame {
                width: dimensions.0,
                height: dimensions.1,
                pixels,
            })
            .is_err()
            && tx.is_closed()
        {
            break;
        }
    }
    Ok(())
}

fn decode_frame(path: &std::path::Path, dimensions: (i32, i32)) -> Result<Vec<u8>> {
    // Gamescope's PNG writer emits RGBA8. Read directly through a buffered file
    // instead of allocating a second, compressed image for GTK to decode later.
    let decoder = png::Decoder::new(BufReader::new(std::fs::File::open(path)?));
    let mut reader = decoder.read_info().context("Reading the preview frame")?;
    let info = reader.info();
    if info.width != dimensions.0 as u32
        || info.height != dimensions.1 as u32
        || info.color_type != png::ColorType::Rgba
        || info.bit_depth != png::BitDepth::Eight
    {
        bail!("Gamescope returned an unsupported preview frame format.");
    }
    let mut pixels = vec![0; dimensions.0 as usize * dimensions.1 as usize * 4];
    reader
        .next_frame(&mut pixels)
        .context("Decoding the preview frame")?;
    Ok(pixels)
}

#[cfg(test)]
mod tests {
    use super::*;
    const WIDTH: i32 = 640;
    const HEIGHT: i32 = 360;

    fn write_png(path: &std::path::Path, width: u32, height: u32, rgba: bool, pixels: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(file, width, height);
        encoder.set_color(if rgba {
            png::ColorType::Rgba
        } else {
            png::ColorType::Rgb
        });
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        writer.write_image_data(pixels).unwrap();
    }

    #[test]
    fn decodes_unpremultiplied_rgba_without_changing_channels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.png");
        let pixels = [0x12, 0x34, 0x56, 0x78].repeat(WIDTH as usize * HEIGHT as usize);
        write_png(&path, WIDTH as u32, HEIGHT as u32, true, &pixels);
        assert_eq!(decode_frame(&path, (WIDTH, HEIGHT)).unwrap(), pixels);
    }

    #[test]
    fn rejects_frames_that_do_not_match_the_texture_contract() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("frame.png");
        write_png(&path, 1, 1, true, &[0, 0, 0, 255]);
        assert!(decode_frame(&path, (WIDTH, HEIGHT)).is_err());
        write_png(
            &path,
            WIDTH as u32,
            HEIGHT as u32,
            false,
            &vec![0; WIDTH as usize * HEIGHT as usize * 3],
        );
        assert!(decode_frame(&path, (WIDTH, HEIGHT)).is_err());
    }
}
