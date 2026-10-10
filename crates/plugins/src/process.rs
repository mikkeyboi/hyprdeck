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

#[cfg(test)]
mod tests {
    use super::*;
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
