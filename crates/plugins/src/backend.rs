//! Installation, explicit trust, activation, and transactional replacement.
use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use hyprdeck_core::{rt, store};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::process;
use crate::protocol::{
    API_VERSION, MAX_DOCUMENT, Manifest, Request, Response, State, action_name, valid_id,
};

static OPERATIONS: Mutex<()> = Mutex::const_new(());
static NEXT: AtomicU64 = AtomicU64::new(0);
const MAX_EXECUTABLE: u64 = 128 * 1024 * 1024;
pub const TRUST: &str = "Plugins are unsandboxed programs running as your user. They can read and change your files, access your devices and network, and run other programs. Only enable code and update repositories you trust. Installing or checking a release does not execute it. Enabling executes a state handshake; explicitly applying an update to an enabled plugin executes the new version and retains your trust.";

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub enabled: BTreeSet<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Installed {
    pub id: String,
    pub manifest: Option<Manifest>,
    pub enabled: bool,
    pub error: Option<String>,
}

pub fn root() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| store::home().join(".local/share"))
        .join("hyprdeck/plugins")
}

pub struct Lock {
    _memory: tokio::sync::MutexGuard<'static, ()>,
    file: File,
}
impl Drop for Lock {
    fn drop(&mut self) {
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

pub async fn lock() -> Result<Lock> {
    let memory = OPERATIONS.lock().await;
    let file = rt::blocking(|| {
        std::fs::create_dir_all(store::state_dir())?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(store::state_dir().join("plugins.lock"))?;
        ensure!(
            file.metadata()?.is_file(),
            "plugin lock is not a regular file"
        );
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok::<_, anyhow::Error>(file)
    })
    .await?;
    Ok(Lock {
        _memory: memory,
        file,
    })
}

fn directory(path: &Path) -> Result<()> {
    let meta =
        std::fs::symlink_metadata(path).with_context(|| format!("reading {}", path.display()))?;
    ensure!(
        meta.is_dir() && !meta.file_type().is_symlink(),
        "{} must be a directory, not a symlink",
        path.display()
    );
    Ok(())
}

fn regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| {
            format!(
                "opening regular file {} (symlinks are forbidden)",
                path.display()
            )
        })?;
    ensure!(
        file.metadata()?.is_file(),
        "{} is not a regular file",
        path.display()
    );
    Ok(file)
}

pub fn read_manifest(dir: &Path) -> Result<Manifest> {
    directory(dir)?;
    let file = regular(&dir.join("plugin.json"))?;
    ensure!(
        file.metadata()?.len() <= MAX_DOCUMENT as u64,
        "manifest is too large"
    );
    let manifest: Manifest = serde_json::from_reader(file).context("invalid plugin.json")?;
    manifest.validate()?;
    Ok(manifest)
}

fn check_executable(dir: &Path, manifest: &Manifest) -> Result<PathBuf> {
    directory(dir)?;
    let path = dir.join(&manifest.executable);
    let file = regular(&path)?;
    ensure!(
        file.metadata()?.permissions().mode() & 0o111 != 0,
        "{} is not executable",
        path.display()
    );
    ensure!(
        file.metadata()?.len() <= MAX_EXECUTABLE,
        "executable is too large"
    );
    Ok(path)
}

pub fn installed(id: &str) -> Result<Manifest> {
    ensure!(valid_id(id), "invalid plugin id");
    directory(&root())?;
    let dir = root().join(id);
    let manifest = read_manifest(&dir)?;
    ensure!(
        manifest.id == id,
        "plugin directory and manifest identity mismatch"
    );
    check_executable(&dir, &manifest)?;
    Ok(manifest)
}

pub fn settings() -> Result<Settings> {
    let settings: Settings = store::load("plugins")?;
    ensure!(
        settings.enabled.iter().all(|id| valid_id(id)),
        "plugins.toml contains an invalid enabled id"
    );
    Ok(settings)
}

pub fn list() -> Result<Vec<Installed>> {
    let settings = settings()?;
    let root = root();
    if !root.try_exists()? {
        return Ok(Vec::new());
    }
    directory(&root)?;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(&root)? {
        let entry = entry?;
        let id = entry.file_name().to_string_lossy().into_owned();
        if id.starts_with('.') {
            continue;
        }
        let enabled = settings.enabled.contains(&id);
        match installed(&id) {
            Ok(manifest) => entries.push(Installed {
                id,
                manifest: Some(manifest),
                enabled,
                error: None,
            }),
            Err(error) => entries.push(Installed {
                id,
                manifest: None,
                enabled,
                error: Some(format!("{error:#}")),
            }),
        }
    }
    for id in &settings.enabled {
        if !entries.iter().any(|entry| &entry.id == id) {
            entries.push(Installed {
                id: id.clone(),
                manifest: None,
                enabled: true,
                error: Some("Enabled plugin is missing; disable it or reinstall it.".into()),
            });
        }
    }
    entries.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(entries)
}

pub async fn request_in(
    dir: &Path,
    manifest: &Manifest,
    action: Option<&str>,
    args: Map<String, Value>,
) -> Result<State> {
    manifest.validate()?;
    if let Some(action) = action {
        action_name(action)?;
    }
    let executable = check_executable(dir, manifest)?;
    let request = Request {
        api_version: API_VERSION,
        method: if action.is_some() { "action" } else { "state" },
        action,
        args,
    };
    let input = serde_json::to_vec(&request)?;
    ensure!(input.len() <= MAX_DOCUMENT, "request is too large");
    let mut command = Command::new(executable);
    command.arg("--request").current_dir(dir);
    let captured = process::capture(command, Some(input), Duration::from_secs(15), MAX_DOCUMENT)
        .await
        .with_context(|| format!("plugin {} request", manifest.id))?;
    if !captured.stderr.is_empty() {
        tracing::debug!(plugin = %manifest.id, diagnostics = %captured.stderr, "plugin diagnostics");
    }
    let response: Response = serde_json::from_slice(&captured.stdout).context(
        "plugin must return one valid JSON document (unknown control kinds are unsupported)",
    )?;
    ensure!(
        response.api_version == API_VERSION,
        "plugin response API {} is unsupported",
        response.api_version
    );
    if let Some(error) = response.error {
        bail!("plugin {}: {error}", manifest.id);
    }
    let state = response
        .state
        .context("successful plugin response has no state")?;
    state.validate()?;
    Ok(state)
}

pub async fn request(id: &str, action: Option<&str>, args: Map<String, Value>) -> Result<State> {
    let _lock = lock().await?;
    ensure!(
        settings()?.enabled.contains(id),
        "plugin {id} is disabled; explicitly enable it to trust and execute it"
    );
    let manifest = installed(id)?;
    request_in(&root().join(id), &manifest, action, args).await
}

pub async fn enable(id: &str) -> Result<()> {
    let _lock = lock().await?;
    let manifest = installed(id)?;
    request_in(&root().join(id), &manifest, None, Map::new())
        .await
        .context("enable handshake failed; activation was not changed")?;
    let mut settings = settings()?;
    settings.enabled.insert(id.to_owned());
    store::save("plugins", &settings)
}

pub async fn disable(id: &str) -> Result<()> {
    ensure!(valid_id(id), "invalid plugin id");
    let _lock = lock().await?;
    let mut settings = settings()?;
    settings.enabled.remove(id);
    store::save("plugins", &settings)
}

pub struct Stage(pub PathBuf);
impl Drop for Stage {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

pub fn stage() -> Result<Stage> {
    let root = root();
    std::fs::create_dir_all(&root)?;
    directory(&root)?;
    for _ in 0..100 {
        let path = root.join(format!(
            ".stage-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match std::fs::create_dir(&path) {
            Ok(()) => {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
                return Ok(Stage(path));
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    bail!("unable to create plugin staging directory")
}

fn write_new(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

pub fn stage_bytes(manifest: &Manifest, executable: &[u8]) -> Result<Stage> {
    manifest.validate()?;
    ensure!(
        !executable.is_empty() && executable.len() as u64 <= MAX_EXECUTABLE,
        "empty or oversized executable"
    );
    let stage = stage()?;
    write_new(
        &stage.0.join("plugin.json"),
        &serde_json::to_vec_pretty(manifest)?,
        0o600,
    )?;
    write_new(&stage.0.join(&manifest.executable), executable, 0o700)?;
    File::open(&stage.0)?.sync_all()?;
    Ok(stage)
}

pub async fn install_local(folder: PathBuf) -> Result<Manifest> {
    let _lock = lock().await?;
    let manifest = read_manifest(&folder)?;
    let mut executable = regular(&folder.join(&manifest.executable))?;
    ensure!(
        executable.metadata()?.len() <= MAX_EXECUTABLE,
        "executable is too large"
    );
    let mut bytes = Vec::new();
    (&mut executable)
        .take(MAX_EXECUTABLE + 1)
        .read_to_end(&mut bytes)?;
    let stage = stage_bytes(&manifest, &bytes)?;
    install_stage(stage, &manifest)?;
    Ok(manifest)
}

pub fn install_stage(stage: Stage, manifest: &Manifest) -> Result<()> {
    let target = root().join(&manifest.id);
    ensure!(
        !target.try_exists()? && std::fs::symlink_metadata(&target).is_err(),
        "plugin {} is already installed; use update (local replacement is not allowed)",
        manifest.id
    );
    // A stale enabled id must never cause a fresh install to execute implicitly.
    let mut settings = settings()?;
    settings.enabled.remove(&manifest.id);
    store::save("plugins", &settings)?;
    std::fs::rename(&stage.0, &target).context("atomically installing plugin")?;
    File::open(root())?.sync_all()?;
    Ok(())
}

fn exchange(a: &Path, b: &Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let a = CString::new(a.as_os_str().as_bytes())?;
    let b = CString::new(b.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            a.as_ptr(),
            libc::AT_FDCWD,
            b.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("atomic directory exchange failed; existing plugin is unchanged");
    }
    Ok(())
}

// A guard retains the old directory until the new enabled runtime has passed
// its handshake. Cancellation/panic also restores the old installation.
struct Replacement {
    stage: Stage,
    target: PathBuf,
    committed: bool,
}
impl Replacement {
    fn begin(stage: Stage, target: PathBuf) -> Result<Self> {
        exchange(&stage.0, &target)?;
        Ok(Self {
            stage,
            target,
            committed: false,
        })
    }
    fn rollback(&mut self) -> Result<()> {
        exchange(&self.stage.0, &self.target)?;
        self.committed = true;
        Ok(())
    }
}
impl Drop for Replacement {
    fn drop(&mut self) {
        if !self.committed
            && let Err(error) = self.rollback()
        {
            // Do not discard the only backup if the filesystem prevents rollback.
            tracing::error!(backup = %self.stage.0.display(), "plugin rollback failed: {error:#}");
            self.stage.0 = PathBuf::new();
        }
    }
}

pub async fn replace(stage: Stage, manifest: &Manifest, old: &Manifest) -> Result<()> {
    manifest.validate_update(old)?;
    let enabled = settings()?.enabled.contains(&old.id);
    replace_at(stage, manifest, root().join(&old.id), enabled).await?;
    File::open(root())?.sync_all()?;
    Ok(())
}

async fn replace_at(
    stage: Stage,
    manifest: &Manifest,
    target: PathBuf,
    enabled: bool,
) -> Result<()> {
    let mut replacement = Replacement::begin(stage, target)?;
    if enabled && let Err(error) = request_in(&replacement.target, manifest, None, Map::new()).await
    {
        if let Err(rollback) = replacement.rollback() {
            let backup = replacement.stage.0.display().to_string();
            replacement.stage.0 = PathBuf::new();
            replacement.committed = true;
            bail!(
                "new plugin handshake failed: {error:#}; rollback failed: {rollback:#}; previous version retained at {backup}"
            );
        }
        bail!("new plugin handshake failed; previous version restored: {error:#}");
    }
    // Disabled plugins remain unexecuted; handshake is deferred to explicit enable.
    replacement.committed = true;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "hd-plugins-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn failed_exchange_keeps_previous_and_guard_drop_restores_previous() {
        let temp = Temp::new();
        let target = temp.0.join("installed");
        std::fs::create_dir(&target).unwrap();
        std::fs::write(target.join("version"), b"old").unwrap();
        let missing = temp.0.join("missing");
        assert!(exchange(&missing, &target).is_err());
        assert_eq!(std::fs::read(target.join("version")).unwrap(), b"old");
        let stage = temp.0.join("stage");
        std::fs::create_dir(&stage).unwrap();
        std::fs::write(stage.join("version"), b"new").unwrap();
        {
            let _rollback = Replacement::begin(Stage(stage), target.clone()).unwrap();
            assert_eq!(std::fs::read(target.join("version")).unwrap(), b"new");
        }
        assert_eq!(std::fs::read(target.join("version")).unwrap(), b"old");
    }
    #[test]
    fn refuses_symlink_executable_and_manifest() {
        let temp = Temp::new();
        let target = temp.0.join("target");
        std::fs::write(&target, b"anything").unwrap();
        let link = temp.0.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(regular(&link).is_err());
    }

    fn fixture(dir: &Path, version: &str, response: &str) -> Manifest {
        std::fs::create_dir(dir).unwrap();
        let manifest = Manifest {
            api_version: 1,
            id: "sample".into(),
            name: "Sample".into(),
            description: "".into(),
            version: version.into(),
            executable: "hyprdeck-sample".into(),
            update_repo: "owner/sample".into(),
            asset: format!("hyprdeck-sample-linux-{}", std::env::consts::ARCH),
        };
        let script = format!(
            "#!/bin/sh\n[ \"$1\" = --request ] || exit 7\nIFS= read -r request || [ -n \"$request\" ] || exit 8\nprintf '%s' '{}'\n",
            response
        );
        write_new(&dir.join(&manifest.executable), script.as_bytes(), 0o700).unwrap();
        write_new(
            &dir.join("plugin.json"),
            &serde_json::to_vec(&manifest).unwrap(),
            0o600,
        )
        .unwrap();
        manifest
    }

    #[tokio::test]
    async fn invalid_candidate_handshake_restores_runnable_manifest_and_binary() {
        let _fixture = process::FIXTURE_LOCK.lock().await;
        let temp = Temp::new();
        let target = temp.0.join("installed");
        let old = fixture(
            &target,
            "1.0.0",
            r#"{"api_version":1,"error":null,"state":{"title":"Previous runtime","description":"","groups":[]}}"#,
        );
        let old_manifest = std::fs::read(target.join("plugin.json")).unwrap();
        let old_binary = std::fs::read(target.join(&old.executable)).unwrap();
        assert_eq!(
            request_in(&target, &old, None, Map::new())
                .await
                .unwrap()
                .title,
            "Previous runtime"
        );
        let candidate = temp.0.join("candidate");
        let new = fixture(
            &candidate,
            "1.1.0",
            r#"{"api_version":2,"state":{"title":"Wrong API","groups":[]}}"#,
        );
        new.validate_update(&old).unwrap();
        assert!(
            replace_at(Stage(candidate), &new, target.clone(), true)
                .await
                .is_err()
        );
        assert_eq!(
            std::fs::read(target.join("plugin.json")).unwrap(),
            old_manifest
        );
        assert_eq!(
            std::fs::read(target.join(&old.executable)).unwrap(),
            old_binary
        );
        assert_eq!(read_manifest(&target).unwrap().version, "1.0.0");
        assert_eq!(
            request_in(&target, &old, None, Map::new())
                .await
                .unwrap()
                .title,
            "Previous runtime"
        );
    }

    #[tokio::test]
    async fn api_and_backend_error_responses_are_not_valid_states() {
        let _fixture = process::FIXTURE_LOCK.lock().await;
        let temp = Temp::new();
        let wrong = temp.0.join("wrong");
        let manifest = fixture(
            &wrong,
            "1.0.0",
            r#"{"api_version":7,"state":{"title":"Bad","groups":[]}}"#,
        );
        assert!(
            request_in(&wrong, &manifest, None, Map::new())
                .await
                .is_err()
        );
        let failed = temp.0.join("failed");
        let manifest = fixture(
            &failed,
            "1.0.0",
            r#"{"api_version":1,"error":"device unavailable","state":null}"#,
        );
        assert!(
            request_in(&failed, &manifest, None, Map::new())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn disabled_update_never_executes_candidate() {
        let _fixture = process::FIXTURE_LOCK.lock().await;
        let temp = Temp::new();
        let target = temp.0.join("installed");
        fixture(
            &target,
            "1.0.0",
            r#"{"api_version":1,"state":{"title":"Old","groups":[]}}"#,
        );
        let candidate = temp.0.join("candidate");
        let new = fixture(
            &candidate,
            "1.1.0",
            "invalid JSON; candidate must not execute",
        );
        std::fs::write(
            candidate.join(&new.executable),
            b"#!/bin/sh\ntouch executed\nexit 1\n",
        )
        .unwrap();
        replace_at(Stage(candidate), &new, target.clone(), false)
            .await
            .unwrap();
        assert_eq!(read_manifest(&target).unwrap().version, "1.1.0");
        assert!(!target.join("executed").exists());
    }
}
