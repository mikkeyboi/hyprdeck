//! All subprocesses are argument-vector launches with bounded pipes and deadlines.
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::process::Command;

// Parallel fixture writers and fork/exec can transiently inherit each other's
// writable executable fds and fail with ETXTBSY before exec closes them.
#[cfg(test)]
pub(crate) static FIXTURE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub struct Captured {
    pub stdout: Vec<u8>,
    pub stderr: String,
}

async fn bounded(mut pipe: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    (&mut pipe)
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() <= limit,
        "subprocess output exceeded {limit} bytes"
    );
    Ok(bytes)
}

struct ProcessGroup(u32);
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}

pub async fn capture(
    mut command: Command,
    input: Option<Vec<u8>>,
    deadline: Duration,
    limit: usize,
) -> Result<Captured> {
    command
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    // Put descendants in a separate process group so a wedged child cannot keep
    // the pipes open forever after the backend itself exits.
    command.process_group(0);
    let mut child = command.spawn().context("starting subprocess")?;
    let group = child.id().map(ProcessGroup);
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().context("missing stdout pipe")?;
    let stderr = child.stderr.take().context("missing stderr pipe")?;
    let result = tokio::time::timeout(deadline, async {
        let write = async move {
            if let (Some(mut pipe), Some(bytes)) = (stdin, input) {
                pipe.write_all(&bytes).await.context("writing request")?;
                pipe.shutdown().await?;
            }
            Ok::<_, anyhow::Error>(())
        };
        let wait = async { child.wait().await.context("waiting for subprocess") };
        let ((), stdout, stderr, status) = tokio::try_join!(
            write,
            bounded(stdout, limit),
            bounded(stderr, 64 * 1024),
            wait
        )?;
        let stderr = String::from_utf8_lossy(&stderr).trim().to_owned();
        ensure!(
            status.success(),
            "subprocess exited with {status}: {stderr}"
        );
        Ok(Captured { stdout, stderr })
    })
    .await;
    // A plugin can fork; terminate its entire group on success as well, since
    // background daemons are not part of the one-request/one-process contract.
    drop(group);
    match result {
        Ok(result) => {
            if result.is_err() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
            result
        }
        Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
            bail!("subprocess timed out after {} seconds", deadline.as_secs())
        }
    }
}

/// Stream leases are registered before scheduling any async work. Shutdown closes
/// admission first, then synchronously signals every group; no Tokio/GTK polling
/// is needed. Weak entries do not keep completed streams alive.
#[derive(Default)]
pub(crate) struct StreamRegistry(std::sync::Mutex<StreamRegistryState>);

#[derive(Default)]
struct StreamRegistryState {
    closed: bool,
    streams: Vec<std::sync::Weak<StreamControl>>,
}

impl StreamRegistry {
    pub(crate) fn lease(&self) -> StreamLease {
        let mut registry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (cancel, _) = tokio::sync::watch::channel(registry.closed);
        let control = std::sync::Arc::new(StreamControl {
            state: std::sync::Mutex::new(StreamState {
                stopped: registry.closed,
                group: None,
            }),
            cancel,
        });
        registry.streams.retain(|entry| entry.strong_count() != 0);
        registry.streams.push(std::sync::Arc::downgrade(&control));
        StreamLease(control)
    }

    pub(crate) fn shutdown(&self) {
        // Keep shutdown calls serialized through the last signal. Otherwise a
        // concurrent caller could see an already-drained registry and exit the
        // app while the first caller has not yet killed its snapshot of groups.
        let mut registry = self.0.lock().unwrap_or_else(|e| e.into_inner());
        registry.closed = true;
        for stream in std::mem::take(&mut registry.streams)
            .into_iter()
            .filter_map(|entry| entry.upgrade())
        {
            stream.stop();
        }
    }
}

#[derive(Default)]
struct StreamState {
    stopped: bool,
    group: Option<u32>,
}

struct StreamControl {
    state: std::sync::Mutex<StreamState>,
    cancel: tokio::sync::watch::Sender<bool>,
}

impl StreamControl {
    fn stop(&self) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.stopped = true;
        if let Some(group) = state.group.take() {
            // The supervisor has not reaped the leader yet. Signal exactly once,
            // before releasing that PID, so a late lease drop cannot kill a
            // different process after PID/PGID reuse.
            unsafe { libc::kill(-(group as i32), libc::SIGKILL) };
        }
        self.cancel.send_replace(true);
    }
}

/// Both the page and its async task own a guard: dropping either terminates the
/// process, even if cancellation cannot be polled or the task is aborted.
#[derive(Clone)]
pub(crate) struct StreamLease(std::sync::Arc<StreamControl>);

impl Drop for StreamLease {
    fn drop(&mut self) {
        self.0.stop();
    }
}

pub(crate) struct StreamChild {
    pub lease: StreamLease,
    pub stdout: std::process::ChildStdout,
    pub stderr: std::process::ChildStderr,
    pub exit: tokio::sync::oneshot::Receiver<Result<std::process::ExitStatus>>,
}

// C strings and pointer tables are constructed before fork. Moving the owner
// does not move CString allocations; the tables remain valid until exec/_exit.
struct StreamExec {
    program: std::ffi::CString,
    _args: Vec<std::ffi::CString>,
    _env: Vec<std::ffi::CString>,
    argv: Vec<*const libc::c_char>,
    envp: Vec<*const libc::c_char>,
}

// SAFETY: this immutable owner keeps every pointed-to allocation alive. It is
// used only by the launcher's pre_exec callback, never mutated concurrently.
unsafe impl Send for StreamExec {}
unsafe impl Sync for StreamExec {}

impl StreamExec {
    fn new(command: &std::process::Command) -> Result<Self> {
        use std::os::unix::ffi::OsStrExt;
        let program = std::ffi::CString::new(command.get_program().as_bytes())?;
        ensure!(
            std::path::Path::new(command.get_program()).is_absolute(),
            "input executable must be an absolute installed path"
        );
        let args: Vec<_> = std::iter::once(command.get_program())
            .chain(command.get_args())
            .map(|arg| std::ffi::CString::new(arg.as_bytes()))
            .collect::<std::result::Result<_, _>>()?;
        let mut environment: std::collections::BTreeMap<_, _> = std::env::vars_os().collect();
        for (key, value) in command.get_envs() {
            if let Some(value) = value {
                environment.insert(key.to_owned(), value.to_owned());
            } else {
                environment.remove(key);
            }
        }
        let env: Vec<_> = environment
            .into_iter()
            .map(|(key, value)| {
                let mut entry = key.as_bytes().to_vec();
                entry.push(b'=');
                entry.extend_from_slice(value.as_bytes());
                std::ffi::CString::new(entry)
            })
            .collect::<std::result::Result<_, _>>()?;
        let argv = args
            .iter()
            .map(|arg| arg.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        let envp = env
            .iter()
            .map(|entry| entry.as_ptr())
            .chain(std::iter::once(std::ptr::null()))
            .collect();
        Ok(Self {
            program,
            _args: args,
            _env: env,
            argv,
            envp,
        })
    }

    // SAFETY: called only after fork; no allocation, locks, or unwinding. A
    // failed exec exits the child normally rather than letting std::spawn reap
    // it behind our registry. The supervisor therefore always owns the PID.
    unsafe fn exec(&self) -> ! {
        unsafe {
            libc::execve(
                self.program.as_ptr(),
                self.argv.as_ptr(),
                self.envp.as_ptr(),
            );
            libc::_exit(126);
        }
    }
}

impl StreamLease {
    pub(crate) fn stop(&self) {
        self.0.stop();
    }

    pub(crate) async fn cancelled(&self) {
        let mut cancel = self.0.cancel.subscribe();
        let _ = cancel.wait_for(|stopped| *stopped).await;
    }

    pub(crate) async fn spawn(&self, mut command: std::process::Command) -> Result<StreamChild> {
        use std::os::unix::process::CommandExt;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0);
        let exec = StreamExec::new(&command)?;
        // If the startup future is abandoned while exec is in progress, stop
        // immediately rather than waiting for a dropped response receiver.
        let startup_guard = self.clone();
        let (started, startup) = tokio::sync::oneshot::channel();
        let (exited, exit) = tokio::sync::oneshot::channel();
        let control = self.0.clone();
        // A dedicated OS supervisor owns the std Child until it is reaped. It
        // survives task cancellation and runtime shutdown, unlike Tokio's orphan
        // queue. GTK never waits for process exit or joins this thread.
        std::thread::Builder::new()
            .name("hyprdeck-input-reaper".into())
            .spawn(move || {
                use std::io::{Read, Write};
                use std::os::fd::AsRawFd;
                if control
                    .state
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .stopped
                {
                    return;
                }
                // Gate exec BEFORE plugin code can run. std::Command::spawn
                // waits for exec and can block on filesystem I/O; it must never
                // hold the stop mutex. A short-lived launcher does that wait,
                // while this supervisor registers the gated child's PID first.
                let (mut parent_gate, child_gate) = match std::os::unix::net::UnixStream::pair() {
                    Ok(pair) => pair,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                let parent_fd = parent_gate.as_raw_fd();
                unsafe {
                    command.pre_exec(move || {
                        // Only async-signal-safe syscalls after fork. Close the
                        // child's copy of the parent endpoint so parent death
                        // produces EOF, not a permanently wedged gated child.
                        libc::close(parent_fd);
                        let pid = libc::getpid().to_ne_bytes();
                        let fd = child_gate.as_raw_fd();
                        if libc::write(fd, pid.as_ptr().cast(), pid.len()) != pid.len() as isize {
                            libc::_exit(125);
                        }
                        let mut release = 0u8;
                        loop {
                            match libc::read(fd, (&mut release as *mut u8).cast(), 1) {
                                1 if release == 1 => break,
                                -1 if std::io::Error::last_os_error().raw_os_error()
                                    == Some(libc::EINTR) => {}
                                _ => libc::_exit(125),
                            }
                        }
                        exec.exec();
                    });
                }
                let launcher = match std::thread::Builder::new()
                    .name("hyprdeck-input-launch".into())
                    .spawn(move || command.spawn())
                {
                    Ok(launcher) => launcher,
                    Err(error) => {
                        let _ = started.send(Err(error));
                        return;
                    }
                };
                let mut pid = [0; std::mem::size_of::<libc::pid_t>()];
                if parent_gate.read_exact(&mut pid).is_ok() {
                    let pid = libc::pid_t::from_ne_bytes(pid) as u32;
                    let mut state = control.state.lock().unwrap_or_else(|e| e.into_inner());
                    if state.stopped {
                        // No plugin code has run and the leader is not reaped.
                        unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                    } else {
                        state.group = Some(pid);
                        // Registration precedes exec permission. Shutdown can
                        // now synchronously kill this group; no spawn gap exists.
                        if parent_gate.write_all(&[1]).is_err() {
                            unsafe { libc::kill(-(pid as i32), libc::SIGKILL) };
                            state.group = None;
                            state.stopped = true;
                        }
                    }
                }
                drop(parent_gate);
                let child = match launcher.join() {
                    Ok(Ok(child)) => child,
                    Ok(Err(error)) => {
                        // Failures before pre_exec have not registered a PID.
                        // exec failures after registration exit with status 126
                        // and return an owned Child instead of this error path.
                        let _ = started.send(Err(error));
                        return;
                    }
                    Err(_) => {
                        control.stop();
                        return;
                    }
                };
                let mut child = child;
                let pipes = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
                if started.send(Ok(pipes)).is_err() {
                    control.stop();
                }
                // Observe exit WITHOUT reaping. Retained pipes cannot hide leader
                // exit; the still-unreaped leader pins its PID until group cleanup.
                let observed = loop {
                    let mut info = unsafe { std::mem::zeroed::<libc::siginfo_t>() };
                    let result = unsafe {
                        libc::waitid(
                            libc::P_PID,
                            child.id(),
                            &mut info,
                            libc::WEXITED | libc::WNOWAIT,
                        )
                    };
                    if result == 0 {
                        break Ok(());
                    }
                    let error = std::io::Error::last_os_error();
                    if error.kind() != std::io::ErrorKind::Interrupted {
                        break Err(error);
                    }
                };
                control.stop();
                let status = child.wait().context("reaping plugin input stream");
                let result = observed
                    .context("observing plugin input leader exit")
                    .and(status);
                let _ = exited.send(result);
            })
            .context("starting input process supervisor")?;
        let (stdout, stderr) = startup.await.context("input startup cancelled")??;
        Ok(StreamChild {
            lease: startup_guard,
            stdout,
            stderr,
            exit,
        })
    }
}

#[cfg(test)]
pub(crate) mod stream_fixture {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    pub(crate) struct Fixture {
        pub dir: PathBuf,
    }

    impl Fixture {
        pub fn new() -> Self {
            static NEXT: AtomicU64 = AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "hd-stream-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&dir).unwrap();
            Self { dir }
        }

        pub fn command(&self, frame: &str) -> std::process::Command {
            let mut command = std::process::Command::new("/usr/bin/python3");
            command.args([
                "-I",
                "-c",
                r#"
import json, os, sys, time
from pathlib import Path
root = Path(sys.argv[1])
child = os.fork()
if child == 0:
    while True: time.sleep(60)
(root / 'pids').write_text(json.dumps([os.getpid(), child]))
print(sys.argv[2], flush=True)
while not (root / 'exit').exists():
    if (root / 'flood').exists(): print(sys.argv[2], flush=True)
    else: time.sleep(.005)
print('retained stderr diagnostic', file=sys.stderr, flush=True)
os._exit(0)
"#,
            ]);
            command.arg(&self.dir).arg(frame);
            command
        }

        pub async fn pids(&self) -> [u32; 2] {
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    if let Ok(bytes) = std::fs::read(self.dir.join("pids"))
                        && let Ok(pids) = serde_json::from_slice(&bytes)
                    {
                        return pids;
                    }
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("fixture did not start")
        }

        pub fn trigger(&self, name: &str) {
            std::fs::write(self.dir.join(name), b"").unwrap();
        }

        pub fn alive(pid: u32) -> bool {
            std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
                !stat
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .trim_start()
                    .starts_with('Z')
            })
        }

        pub fn assert_reaped_and_killed(pids: [u32; 2]) {
            let deadline = std::time::Instant::now() + Duration::from_secs(3);
            while Self::alive(pids[0])
                || Self::alive(pids[1])
                || std::path::Path::new(&format!("/proc/{}", pids[0])).exists()
            {
                assert!(
                    std::time::Instant::now() < deadline,
                    "processes survived: {pids:?}"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // The leader belongs to the supervisor; no zombie is left for the
            // caller (descendants are reparented to the system's child reaper).
            let mut status = 0;
            assert_eq!(
                unsafe { libc::waitpid(pids[0] as i32, &mut status, libc::WNOHANG) },
                -1
            );
            assert_eq!(
                std::io::Error::last_os_error().raw_os_error(),
                Some(libc::ECHILD)
            );
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use stream_fixture::Fixture;

    #[tokio::test]
    async fn leader_exit_kills_pipe_retaining_descendant_and_reaps() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let fixture = Fixture::new();
        let child = lease.spawn(fixture.command("frame")).await.unwrap();
        let pids = fixture.pids().await;
        fixture.trigger("exit");
        let status = tokio::time::timeout(Duration::from_secs(3), child.exit)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(status.success());
        Fixture::assert_reaped_and_killed(pids);
        // A still-live page lease cannot target a reused PID after leader exit.
        assert!(lease.0.state.lock().unwrap().group.is_none());
    }

    #[tokio::test]
    async fn lease_drop_kills_and_reaps_without_async_cancellation_poll() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let fixture = Fixture::new();
        let child = lease.spawn(fixture.command("frame")).await.unwrap();
        let pids = fixture.pids().await;
        drop(lease);
        // Block this single-thread Tokio test executor intentionally. Only the
        // synchronous guard and independent reaper can make progress here.
        Fixture::assert_reaped_and_killed(pids);
        assert!(!child.exit.await.unwrap().unwrap().success());
    }

    #[tokio::test]
    async fn app_shutdown_kills_active_groups_and_rejects_queued_startup() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let queued = registry.lease();
        let fixture = Fixture::new();
        let child = lease.spawn(fixture.command("frame")).await.unwrap();
        let pids = fixture.pids().await;
        registry.shutdown();
        registry.shutdown();
        Fixture::assert_reaped_and_killed(pids);
        assert!(queued.spawn(fixture.command("frame")).await.is_err());
        assert!(
            registry
                .lease()
                .spawn(fixture.command("frame"))
                .await
                .is_err()
        );
        assert!(!child.exit.await.unwrap().unwrap().success());
    }

    #[tokio::test]
    async fn dropped_startup_future_prevents_late_spawn() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let fixture = Fixture::new();
        // Poll exactly once to schedule the supervisor, then abandon startup.
        // The guard's stop synchronizes with registration whether spawn won
        // the race or not. A later attempt is rejected in either case.
        let mut startup = Box::pin(lease.spawn(fixture.command("frame")));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(startup.as_mut(), &mut context).is_pending());
        drop(startup);
        assert!(lease.0.state.lock().unwrap().stopped);
        assert!(lease.spawn(fixture.command("frame")).await.is_err());
    }

    #[tokio::test]
    async fn abandoned_startup_response_kills_already_spawned_group() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let fixture = Fixture::new();
        let mut startup = Box::pin(lease.spawn(fixture.command("frame")));
        let waker = std::task::Waker::noop();
        let mut context = std::task::Context::from_waker(waker);
        assert!(std::future::Future::poll(startup.as_mut(), &mut context).is_pending());
        // Do not poll startup again: the OS supervisor can launch while its
        // completed startup response is still unconsumed by the async task.
        let pids = fixture.pids().await;
        drop(startup);
        Fixture::assert_reaped_and_killed(pids);
    }

    #[tokio::test]
    async fn task_abort_and_runtime_drop_do_not_abandon_stream() {
        let registry = StreamRegistry::default();
        let fixture = Fixture::new();
        let lease = registry.lease();
        let command = fixture.command("frame");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (ready, receive) = tokio::sync::oneshot::channel();
        let pid_file = fixture.dir.join("pids");
        let task = runtime.spawn(async move {
            let child = lease.spawn(command).await.unwrap();
            loop {
                if let Ok(bytes) = std::fs::read(&pid_file)
                    && let Ok(pids) = serde_json::from_slice::<[u32; 2]>(&bytes)
                {
                    ready.send(pids).unwrap();
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            std::future::pending::<()>().await;
            drop(child);
        });
        // Drop the runtime on a separate thread, while the spawned task and
        // process are still active; no Tokio orphan queue can reap afterward.
        let pids = std::thread::spawn(move || {
            let pids = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(3), receive)
                    .await
                    .unwrap()
                    .unwrap()
            });
            task.abort();
            drop(runtime);
            pids
        })
        .join()
        .unwrap();
        Fixture::assert_reaped_and_killed(pids);
    }

    #[tokio::test]
    async fn failed_exec_is_reaped_before_retiring_group_identity() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let fixture = Fixture::new();
        let command = std::process::Command::new(fixture.dir.join("does-not-exist"));
        let child = lease.spawn(command).await.unwrap();
        assert_eq!(child.exit.await.unwrap().unwrap().code(), Some(126));
        assert!(lease.0.state.lock().unwrap().group.is_none());
        lease.stop();
    }

    #[tokio::test]
    async fn already_cancelled_startup_never_executes_fixture() {
        let registry = StreamRegistry::default();
        let lease = registry.lease();
        let fixture = Fixture::new();
        lease.stop();
        assert!(lease.spawn(fixture.command("frame")).await.is_err());
        assert!(!fixture.dir.join("pids").exists());
    }

    #[tokio::test]
    async fn timeout_cancels_real_sleep_process() {
        let _fixture = FIXTURE_LOCK.lock().await;
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = std::time::Instant::now();
        assert!(
            capture(command, None, Duration::from_millis(25), 1024)
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(3));
    }
    #[tokio::test]
    async fn unbounded_stdout_is_rejected_before_deadline() {
        let _fixture = FIXTURE_LOCK.lock().await;
        let mut command = Command::new("/usr/bin/yes");
        command.arg("plugin-output");
        let started = std::time::Instant::now();
        assert!(
            capture(command, None, Duration::from_secs(10), 1024)
                .await
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
