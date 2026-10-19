//! Compatibility-tool inventory and explicit, staged installation. Merely listing
//! targets/releases never changes a launcher, game mapping, prefix, or tool.
use std::collections::BTreeSet;
use std::ffi::CString;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub use crate::proton_cpu::{CpuProfile, cpu_profile};

#[path = "proton_archive.rs"]
mod archive;
#[path = "proton_manage.rs"]
mod manage;
#[path = "steam_catalog.rs"]
mod steam_catalog;
pub use manage::{Capabilities, capabilities, remove, rename};

const RECEIPT: &str = ".hyprdeck-tool.json";
const MAX_DOWNLOAD: u64 = 3 * 1024 * 1024 * 1024;
const MAX_METADATA: u64 = 16 * 1024 * 1024;
const ALL_TARGETS: &[TargetKind] = &[
    TargetKind::Steam,
    TargetKind::Lutris,
    TargetKind::Heroic,
    TargetKind::Bottles,
];
const STEAM_HEROIC: &[TargetKind] = &[TargetKind::Steam, TargetKind::Heroic];
const STEAM: &[TargetKind] = &[TargetKind::Steam];

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetKind {
    Steam,
    Lutris,
    Heroic,
    Bottles,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Target {
    pub id: String,
    pub name: String,
    /// Directory containing individual compatibility tools, not the launcher root.
    pub root: PathBuf,
    pub kind: TargetKind,
    /// Detected launcher owner, kept separately when its tools directory is a symlink.
    #[serde(default)]
    pub launcher_root: Option<PathBuf>,
    /// Actual detected Steam packaging, never inferred from the display label.
    #[serde(default)]
    pub steam_packaging: Option<SteamPackaging>,
}

#[derive(Clone, Debug)]
pub struct InstalledTool {
    /// Internal compatibilitytool.vdf ID (the key Steam stores in game mappings).
    pub name: String,
    pub display_name: String,
    pub path: PathBuf,
    pub target_id: String,
    pub managed: bool,
}

#[derive(Clone, Debug)]
pub struct Provider {
    pub id: &'static str,
    pub name: &'static str,
    pub repo: &'static str,
    pub description: &'static str,
    pub targets: &'static [TargetKind],
}

#[derive(Clone, Debug)]
pub struct Artifact {
    pub url: String,
    pub name: String,
    /// Bytes advertised by GitHub; zero only when upstream provides no size.
    pub size: u64,
}

#[derive(Clone, Debug)]
pub struct Release {
    /// Provider ID followed by ':' and the upstream tag or CI run identifier.
    pub id: String,
    pub version: String,
    pub notes: String,
    pub url: String,
    pub published: String,
    pub prerelease: bool,
    pub artifact: Option<Artifact>,
    pub steam_app: Option<u32>,
    /// Human-readable installation method, unsupported reason, or prerequisites.
    pub availability: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct InstallOptions {
    /// Explicit opt-in: repair only a recognized parser in private staging.
    pub repair_prefix_parser: bool,
}

#[derive(Clone, Debug)]
pub struct Progress {
    pub message: String,
    pub fraction: Option<f64>,
}

pub fn providers() -> Vec<Provider> {
    vec![
        Provider {
            id: "ge-proton",
            name: "GE-Proton",
            repo: "GloriousEggroll/proton-ge-custom",
            description: "GE's prebuilt Proton. Non-Steam launchers need their Proton/UMU support enabled.",
            targets: ALL_TARGETS,
        },
        Provider {
            id: "valve-proton",
            name: "Valve Proton (official)",
            repo: "ValveSoftware/Proton",
            description: "Official builds are installed and updated by Steam, not GitHub source archives.",
            targets: STEAM,
        },
        Provider {
            id: "proton-tkg",
            name: "Proton-TKG",
            repo: "Frogging-Family/wine-tkg-git",
            description: "Prebuilt Proton-TKG, including upstream's Steam Runtime CI builds via nightly.link.",
            targets: STEAM_HEROIC,
        },
        Provider {
            id: "proton-cachyos",
            name: "Proton-CachyOS",
            repo: "CachyOS/proton-cachyos",
            description: "Portable Steam Linux Runtime builds; selects the highest CPU-compatible ISA tier, never native distribution packages.",
            targets: ALL_TARGETS,
        },
        Provider {
            id: "steamtinkerlaunch",
            name: "SteamTinkerLaunch",
            repo: "sonic2kk/steamtinkerlaunch",
            description: "Interpreted Steam wrapper. Installs release script/data without executing an installer or changing shell files. Requires native Steam and upstream runtime dependencies.",
            targets: STEAM,
        },
        Provider {
            id: "proton-wineland",
            name: "Proton-Wineland",
            repo: "nanomatters/proton-salkim",
            description: "Wineland Wayland-focused Proton binaries (upstream renamed proton-cachyos to proton-salkim).",
            targets: ALL_TARGETS,
        },
    ]
}

fn provider(id: &str) -> Result<Provider> {
    providers()
        .into_iter()
        .find(|p| p.id == id)
        .context("Unknown compatibility-tool provider")
}

fn environment_path(key: &str, fallback: PathBuf) -> PathBuf {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .unwrap_or(fallback)
}

/// Native and sandboxed launcher locations follow ProtonUp-Qt's launcher layout.
/// An explicit HYPRDECK_COMPAT_TOOLS_DIR adds a custom Steam-compatible root;
/// callers can also construct and persist a Target for a launcher chosen by users.
pub fn targets() -> Result<Vec<Target>> {
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    ensure!(home.is_absolute(), "HOME must be absolute");
    let data = environment_path("XDG_DATA_HOME", home.join(".local/share"));
    let config = environment_path("XDG_CONFIG_HOME", home.join(".config"));
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    let mut add = |name: &str,
                   base: PathBuf,
                   suffix: &str,
                   kind: TargetKind,
                   packaging: Option<SteamPackaging>|
     -> Result<()> {
        if !base.is_dir() {
            return Ok(());
        }
        let base = fs::canonicalize(base)?;
        let root = base.join(suffix);
        if seen.insert((
            fs::canonicalize(&root).unwrap_or_else(|_| root.clone()),
            Some(base.clone()),
        )) {
            result.push(Target {
                id: format!("{kind:?}:{}", root.display()),
                name: name.into(),
                root,
                kind,
                launcher_root: Some(base),
                steam_packaging: packaging,
            });
        }
        Ok(())
    };
    for base in [
        data.join("Steam"),
        home.join(".steam/root"),
        home.join(".steam/steam"),
        home.join(".steam/debian-installation"),
    ] {
        add(
            "Steam",
            base,
            "compatibilitytools.d",
            TargetKind::Steam,
            Some(SteamPackaging::Native),
        )?;
    }
    add(
        "Steam (Flatpak)",
        home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
        "compatibilitytools.d",
        TargetKind::Steam,
        Some(SteamPackaging::Flatpak),
    )?;
    add(
        "Steam (Flatpak)",
        home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        "compatibilitytools.d",
        TargetKind::Steam,
        Some(SteamPackaging::Flatpak),
    )?;
    add(
        "Steam (Snap)",
        home.join("snap/steam/common/.steam/root"),
        "compatibilitytools.d",
        TargetKind::Steam,
        Some(SteamPackaging::Snap),
    )?;
    add(
        "Lutris",
        data.join("lutris"),
        "runners/wine",
        TargetKind::Lutris,
        None,
    )?;
    add(
        "Lutris (Flatpak)",
        home.join(".var/app/net.lutris.Lutris/data/lutris"),
        "runners/wine",
        TargetKind::Lutris,
        None,
    )?;
    add(
        "Heroic Proton",
        config.join("heroic"),
        "tools/proton",
        TargetKind::Heroic,
        None,
    )?;
    add(
        "Heroic Proton (Flatpak)",
        home.join(".var/app/com.heroicgameslauncher.hgl/config/heroic"),
        "tools/proton",
        TargetKind::Heroic,
        None,
    )?;
    add(
        "Bottles",
        data.join("bottles"),
        "runners",
        TargetKind::Bottles,
        None,
    )?;
    add(
        "Bottles (Flatpak)",
        home.join(".var/app/com.usebottles.bottles/data/bottles"),
        "runners",
        TargetKind::Bottles,
        None,
    )?;
    if let Some(root) = std::env::var_os("HYPRDECK_COMPAT_TOOLS_DIR") {
        let root = PathBuf::from(root);
        ensure!(
            root.is_absolute(),
            "HYPRDECK_COMPAT_TOOLS_DIR must be absolute"
        );
        let root = fs::canonicalize(&root).unwrap_or(root);
        if !seen.iter().any(|(known, _)| known == &root) {
            result.push(Target {
                id: format!("Steam:{}", root.display()),
                name: "Custom compatibility tools".into(),
                root,
                kind: TargetKind::Steam,
                launcher_root: None,
                steam_packaging: None,
            });
        }
    }
    Ok(result)
}

#[derive(Serialize, Deserialize)]
struct Receipt {
    owner: String,
    schema: u32,
    target_id: String,
    root: PathBuf,
    directory: String,
    tool_id: String,
    release_id: String,
    source: String,
}

fn read_limited(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let file = File::open(path)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "File exceeds the safety limit"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(bytes.len() as u64 <= limit, "File exceeds the safety limit");
    Ok(bytes)
}

fn valid_receipt(target: &Target, directory: &Path, tool_id: &str) -> Result<Receipt> {
    let root = fs::canonicalize(&target.root)?;
    ensure!(
        directory.parent() == Some(root.as_path()),
        "Tool is not an immediate child of this target"
    );
    let metadata = fs::symlink_metadata(directory)?;
    ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "Refusing a symlinked tool directory"
    );
    ensure!(
        metadata.uid() == unsafe { libc::geteuid() },
        "Tool directory is owned by another user"
    );
    let file = directory.join(RECEIPT);
    let metadata = fs::symlink_metadata(&file)?;
    ensure!(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() },
        "Installation receipt is not an owned regular file"
    );
    let receipt: Receipt = serde_json::from_slice(&read_limited(&file, 64 * 1024)?)?;
    ensure!(
        receipt.owner == "Hyprdeck"
            && receipt.schema == 1
            && receipt.target_id == target.id
            && receipt.root == root
            && receipt.tool_id == tool_id
            && directory.file_name().and_then(|n| n.to_str()) == Some(receipt.directory.as_str()),
        "Installation receipt does not match this tool and target"
    );
    Ok(receipt)
}

/// Internal tool IDs come from the VDF, not from directory/display-name guesses.
fn describe(target: &Target, directory: &Path) -> Result<Vec<InstalledTool>> {
    let directory = fs::canonicalize(directory)?;
    let manifest = directory.join("compatibilitytool.vdf");
    if manifest.is_file() {
        let text = String::from_utf8(read_limited(&manifest, 1024 * 1024)?)?;
        let vdf = crate::steam::parse_vdf(&text)?;
        let compatibility = vdf
            .get("compatibilitytools")
            .context("Missing compatibilitytools section")?;
        let tools = compatibility
            .get("compat_tools")
            .unwrap_or(compatibility)
            .entries()
            .context("Missing compatibility tool definitions")?;
        let mut result = Vec::new();
        for (name, values) in tools {
            if values.entries().is_none() {
                continue;
            }
            let install_path = values
                .get("install_path")
                .and_then(|v| v.text())
                .unwrap_or(".");
            let path = fs::canonicalize(directory.join(install_path))
                .context("Compatibility tool install_path is missing")?;
            let display_name = values
                .get("display_name")
                .and_then(|v| v.text())
                .unwrap_or(name)
                .to_owned();
            let managed = path == directory && valid_receipt(target, &directory, name).is_ok();
            result.push(InstalledTool {
                name: name.clone(),
                display_name,
                path,
                target_id: target.id.clone(),
                managed,
            });
        }
        ensure!(
            !result.is_empty(),
            "No compatibility tools declared in manifest"
        );
        return Ok(result);
    }
    // Existing Wine runners from launchers need not carry a Steam VDF.
    if target.kind != TargetKind::Steam
        && ["bin/wine", "files/bin/wine", "dist/bin/wine", "proton"]
            .iter()
            .any(|p| directory.join(p).is_file())
    {
        let name = directory
            .file_name()
            .context("Runner has no directory name")?
            .to_string_lossy()
            .into_owned();
        return Ok(vec![InstalledTool {
            display_name: name.clone(),
            managed: valid_receipt(target, &directory, &name).is_ok(),
            name,
            path: directory,
            target_id: target.id.clone(),
        }]);
    }
    Ok(Vec::new())
}

pub fn installed(target: &Target) -> Result<Vec<InstalledTool>> {
    if !target.root.exists() {
        return Ok(Vec::new());
    }
    let mut result = Vec::new();
    for entry in
        fs::read_dir(&target.root).with_context(|| format!("Reading {}", target.root.display()))?
    {
        let entry = entry?;
        if entry.file_name().as_bytes().starts_with(b".") || !entry.path().is_dir() {
            continue;
        }
        match describe(target, &entry.path()) {
            Ok(tools) => result.extend(tools),
            Err(error) => {
                tracing::warn!(path = %entry.path().display(), "Skipping invalid compatibility tool: {error:#}")
            }
        }
    }
    result.sort_by(|a, b| {
        a.display_name
            .cmp(&b.display_name)
            .then(a.path.cmp(&b.path))
    });
    result.dedup_by(|a, b| a.name == b.name && a.path == b.path);
    Ok(result)
}

fn curl(url: &str, output: &Path, limit: u64, api: bool) -> Result<Command> {
    ensure!(
        url.starts_with("https://") && !url.chars().any(|c| c.is_whitespace() || c.is_control()),
        "Download URL must use HTTPS without whitespace"
    );
    let mut command = Command::new("curl");
    command.args([
        "--fail",
        "--silent",
        "--show-error",
        "--location",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--connect-timeout",
        "10",
        "--max-time",
        if api { "90" } else { "3600" },
        "--max-filesize",
        &limit.to_string(),
        "--user-agent",
        "Hyprdeck-compatibility-tools/1",
        "--output",
    ]);
    command.arg(output);
    if api {
        command.args([
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
        ]);
    }
    command
        .arg("--")
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null());
    Ok(command)
}

struct DownloadChild(Child);
impl Drop for DownloadChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn download(
    url: &str,
    output: &Path,
    size: u64,
    api: bool,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<()> {
    archive::cancelled(cancel)?;
    let limit = if api { MAX_METADATA } else { MAX_DOWNLOAD };
    ensure!(size <= limit, "Download exceeds the safety limit");
    let error = tempfile::tempfile()?;
    let mut command = curl(url, output, limit, api)?;
    command.stderr(error.try_clone()?);
    let mut child = DownloadChild(
        command
            .spawn()
            .context("Starting download; curl is required")?,
    );
    let mut last = u64::MAX;
    loop {
        archive::cancelled(cancel)?;
        let downloaded = fs::metadata(output).map(|m| m.len()).unwrap_or(0);
        ensure!(downloaded <= limit, "Download exceeds the safety limit");
        if !api && downloaded != last {
            progress(Progress {
                message: format!(
                    "Downloading: {} MiB{}",
                    downloaded / (1024 * 1024),
                    if size > 0 {
                        format!(" / {} MiB", size / (1024 * 1024))
                    } else {
                        String::new()
                    }
                ),
                fraction: (size > 0).then(|| (downloaded as f64 / size as f64).min(1.0) * 0.8),
            });
            last = downloaded;
        }
        if let Some(status) = child.0.try_wait()? {
            if !status.success() {
                use std::io::{Seek, SeekFrom};
                let mut error = error;
                error.seek(SeekFrom::Start(0))?;
                let mut message = String::new();
                error.take(8192).read_to_string(&mut message)?;
                bail!(
                    "Download failed (check network/GitHub rate limit): {}",
                    message.trim()
                );
            }
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    let actual = fs::metadata(output)?.len();
    ensure!(
        actual > 0 && actual <= limit,
        "Downloaded file is empty or exceeds the safety limit"
    );
    if size > 0 {
        ensure!(
            actual == size,
            "Downloaded file size differs from upstream metadata"
        );
    }
    Ok(())
}

fn github(path: &str) -> Result<Value> {
    let temp = tempfile::tempdir()?;
    let output = temp.path().join("metadata.json");
    download(
        &format!("https://api.github.com/repos/{path}"),
        &output,
        0,
        true,
        &AtomicBool::new(false),
        &mut |_| {},
    )?;
    let value: Value = serde_json::from_slice(&read_limited(&output, MAX_METADATA)?)?;
    if let Some(message) = value.get("message").and_then(Value::as_str) {
        bail!("GitHub: {message}");
    }
    Ok(value)
}

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or("")
}

fn archive_name(name: &str) -> bool {
    [".tar.gz", ".tar.xz", ".tar.zst", ".tar"]
        .iter()
        .any(|suffix| name.ends_with(suffix))
}

#[derive(Clone, Copy, Debug)]
struct BinaryBuild {
    architecture: &'static str,
    x86_64_level: Option<u8>,
}

impl BinaryBuild {
    fn compatible(self, cpu: &CpuProfile) -> bool {
        self.architecture == cpu.architecture
            && match self.x86_64_level {
                Some(required) => cpu.x86_64_level.is_some_and(|usable| required <= usable),
                None => true,
            }
    }

    fn rank(self) -> u8 {
        self.x86_64_level.unwrap_or(0)
    }

    fn description(self) -> String {
        match self.x86_64_level {
            Some(1) => "x86-64 baseline".into(),
            Some(level) => format!("x86-64-v{level}"),
            None => self.architecture.into(),
        }
    }
}

fn binary_build(provider: &str, name: &str) -> Option<BinaryBuild> {
    let lower = name.to_ascii_lowercase();
    if !archive_name(&lower)
        || !lower
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-_+".contains(&byte))
        || ["source", "debug", ".pkg.", "native", "wow64"]
            .iter()
            .any(|&part| lower.contains(part))
    {
        return None;
    }
    let stem = binary_stem(&lower);
    let mut explicit_architecture = true;
    let build = if stem.ends_with("-aarch64") || stem.ends_with("-arm64") {
        BinaryBuild {
            architecture: "aarch64",
            x86_64_level: None,
        }
    } else if let Some((_, tier)) = stem
        .rsplit_once("-x86_64_v")
        .or_else(|| stem.rsplit_once("-x86_64-v"))
        .or_else(|| stem.rsplit_once("-x86-64-v"))
    {
        let level = match tier {
            "1" => 1,
            "2" => 2,
            "3" => 3,
            "4" => 4,
            _ => return None,
        };
        BinaryBuild {
            architecture: "x86_64",
            x86_64_level: Some(level),
        }
    } else if stem.ends_with("-x86_64") || stem.ends_with("-x86-64") {
        BinaryBuild {
            architecture: "x86_64",
            x86_64_level: Some(1),
        }
    } else {
        // Older GE and TKG assets had no architecture suffix. Never treat an
        // unrecognized architecture or ISA suffix as one of those legacy builds.
        if ["x86", "aarch", "arm", "i386", "i686", "riscv", "ppc"]
            .iter()
            .any(|&part| stem.contains(part))
            || stem.as_bytes().windows(3).any(|part| {
                matches!(part[0], b'_' | b'-') && part[1] == b'v' && part[2].is_ascii_digit()
            })
        {
            return None;
        }
        explicit_architecture = false;
        BinaryBuild {
            architecture: "x86_64",
            x86_64_level: Some(1),
        }
    };
    let supported = match provider {
        "ge-proton" => lower.starts_with("ge-proton") || lower.starts_with("proton-ge"),
        "proton-cachyos" => {
            lower.starts_with("proton-cachyos-") && lower.contains("-slr-") && explicit_architecture
        }
        "proton-wineland" => lower.starts_with("proton-wineland-") && explicit_architecture,
        "proton-tkg" => {
            build.architecture == "x86_64"
                && (lower.starts_with("proton_tkg_") || lower.starts_with("proton-tkg"))
        }
        _ => false,
    };
    supported.then_some(build)
}

fn binary_matches(provider: &str, name: &str, cpu: &CpuProfile) -> bool {
    binary_build(provider, name).is_some_and(|build| build.compatible(cpu))
}

fn binary_stem(name: &str) -> &str {
    [".tar.zst", ".tar.xz", ".tar.gz", ".tgz", ".tar"]
        .iter()
        .find_map(|suffix| name.strip_suffix(suffix))
        .unwrap_or(name)
}

fn compression_rank(name: &str) -> u8 {
    if name.ends_with(".tar.zst") {
        0
    } else if name.ends_with(".tar.xz") {
        1
    } else {
        2
    }
}

fn asset(provider: &Provider, release: &Value, cpu: &CpuProfile) -> Result<Option<Artifact>> {
    let assets = release
        .get("assets")
        .and_then(Value::as_array)
        .context("GitHub release has no asset list")?;
    let mut selected: Option<(BinaryBuild, &Value)> = None;
    let mut ambiguous = false;
    for candidate in assets {
        let Some(build) = binary_build(provider.id, text(candidate, "name")) else {
            continue;
        };
        if !build.compatible(cpu) {
            continue;
        }
        match selected {
            None => selected = Some((build, candidate)),
            Some((previous, _)) if build.rank() > previous.rank() => {
                selected = Some((build, candidate));
                ambiguous = false;
            }
            Some((previous, chosen)) if build.rank() == previous.rank() => {
                ambiguous |=
                    binary_stem(text(candidate, "name")) != binary_stem(text(chosen, "name"));
                if compression_rank(text(candidate, "name"))
                    < compression_rank(text(chosen, "name"))
                {
                    selected = Some((build, candidate));
                }
            }
            _ => {}
        }
    }
    let Some((_, chosen)) = selected else {
        return Ok(None);
    };
    ensure!(
        !ambiguous,
        "Release has multiple distinct binary builds in the highest compatible ISA tier; refusing an ambiguous choice"
    );
    let url = text(chosen, "browser_download_url");
    ensure!(
        standard_asset_url(provider, text(chosen, "name"), url),
        "Release asset URL does not match its provider and advertised filename"
    );
    let size = chosen
        .get("size")
        .and_then(Value::as_u64)
        .context("Release artifact size is missing")?;
    ensure!(
        size > 0 && size <= MAX_DOWNLOAD,
        "Release artifact size is unsafe"
    );
    Ok(Some(Artifact {
        url: url.into(),
        name: text(chosen, "name").into(),
        size,
    }))
}

fn standard_asset_url(provider: &Provider, name: &str, url: &str) -> bool {
    url.strip_prefix(&format!(
        "https://github.com/{}/releases/download/",
        provider.repo
    ))
    .and_then(|path| path.split_once('/'))
    .is_some_and(|(tag, filename)| {
        !tag.is_empty()
            && tag != "."
            && tag != ".."
            && !tag.contains('?')
            && !tag.contains('#')
            && filename == name
    })
}

/// Detect the running host, not the architecture used to compile Hyprdeck.
fn host_arch() -> &'static str {
    &crate::proton_cpu::host_profile().architecture
}

fn release_from(provider: &Provider, target: &Target, value: &Value) -> Result<Release> {
    release_from_for_cpu(provider, target, value, crate::proton_cpu::host_profile())
}

fn release_from_for_cpu(
    provider: &Provider,
    target: &Target,
    value: &Value,
    cpu: &CpuProfile,
) -> Result<Release> {
    let version = text(value, "tag_name");
    ensure!(!version.is_empty(), "Release is missing its version tag");
    let mut release = Release {
        id: format!("{}:{version}", provider.id),
        version: version.into(),
        notes: text(value, "body").into(),
        url: text(value, "html_url").into(),
        published: text(value, "published_at").into(),
        prerelease: value
            .get("prerelease")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        artifact: None,
        steam_app: None,
        availability: String::new(),
    };
    if !provider.targets.contains(&target.kind) {
        release.availability = format!(
            "{} is not supported for {:?} targets",
            provider.name, target.kind
        );
    } else if provider.id == "valve-proton" {
        release.availability = "Official Steam action unavailable until verified against the selected client's current Tools metadata. GitHub archives contain source, not an installable tool.".into();
    } else if provider.id == "steamtinkerlaunch" {
        if flatpak(target) {
            release.url =
                "https://github.com/sonic2kk/steamtinkerlaunch/wiki/Installation#flatpak".into();
            release.availability = "Flatpak Steam requires com.valvesoftware.Steam.Utility.steamtinkerlaunch from Flathub; use the upstream Flatpak installation instructions. Native host scripts cannot satisfy the sandbox dependencies.".into();
        } else {
            let url = text(value, "tarball_url");
            ensure!(
                url.starts_with("https://api.github.com/repos/sonic2kk/steamtinkerlaunch/tarball/"),
                "Unexpected SteamTinkerLaunch archive URL"
            );
            release.artifact = Some(Artifact {
                url: url.into(),
                name: format!("steamtinkerlaunch-{}.tar.gz", safe_name(version)?),
                size: 0,
            });
            release.availability = "Install interpreted release script/data (not a compiled Proton build). No downloaded code executes during installation. Requires bash, awk, git, pgrep, pidof, pkill, tar, unzip, wget, xdotool, xprop, xrandr, xwininfo, xxd and Yad ≥ 7.2. SteamTinkerLaunch may download optional components when you explicitly run it.".into();
        }
    } else {
        release.artifact = asset(provider, value, cpu)?;
        release.availability = match release.artifact.as_ref().and_then(|artifact| {
            binary_build(provider.id, &artifact.name).map(|build| (artifact, build))
        }) {
            Some((artifact, build)) => format!(
                "Selected {}: prebuilt {} {} archive, CPU/OS-compatible with {}. Restart the launcher to discover it; non-Steam Proton requires the launcher's UMU/Proton support.",
                artifact.name,
                provider.name,
                build.description(),
                cpu.description()
            ),
            None => format!(
                "No CPU/OS-compatible binary for {} in this release. Only supported architecture/ISA tiers are selected; source archives, alternate ABIs and native distribution packages are not installed.",
                cpu.description()
            ),
        };
    }
    Ok(release)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum SteamPackaging {
    Native,
    Flatpak,
    Snap,
}

fn canonical_tool_root(path: &Path) -> Result<PathBuf> {
    match fs::canonicalize(path) {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path
                .parent()
                .context("Compatibility-tool directory has no parent")?;
            Ok(fs::canonicalize(parent)?.join(
                path.file_name()
                    .context("Compatibility-tool directory has no name")?,
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Resolve and revalidate the Steam owner, not the parent of a symlink's target.
pub fn steam_root(target: &Target) -> Result<PathBuf> {
    ensure!(
        target.kind == TargetKind::Steam,
        "Official builds require a Steam target"
    );
    let canonical_tools = canonical_tool_root(&target.root)?;
    let owner = target
        .launcher_root
        .as_deref()
        .or_else(|| {
            target
                .root
                .file_name()
                .filter(|name| *name == "compatibilitytools.d")
                .and_then(|_| target.root.parent())
        })
        .or_else(|| {
            canonical_tools
                .file_name()
                .filter(|name| *name == "compatibilitytools.d")
                .and_then(|_| canonical_tools.parent())
        })
        .context("Custom tool directory has no verified Steam owner")?;
    let owner = fs::canonicalize(owner).context("Cannot resolve the selected Steam owner")?;
    ensure!(
        owner.join("steamapps").is_dir(),
        "Selected Steam owner has no steamapps directory"
    );
    ensure!(
        canonical_tool_root(&owner.join("compatibilitytools.d"))?
            == canonical_tool_root(&target.root)?,
        "Selected Steam owner's compatibility-tool directory no longer matches this target"
    );
    Ok(owner)
}

fn same_steam_root(target: &Target, base: &Path) -> bool {
    steam_root(target)
        .ok()
        .zip(fs::canonicalize(base).ok())
        .is_some_and(|(root, expected)| root == expected)
}

fn steam_packaging(target: &Target) -> Result<SteamPackaging> {
    steam_root(target)?;
    if let Some(packaging) = target.steam_packaging {
        return Ok(packaging);
    }
    let home = PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?);
    ensure!(home.is_absolute(), "HOME must be absolute");
    for (packaging, base) in [
        (
            SteamPackaging::Flatpak,
            home.join(".var/app/com.valvesoftware.Steam/data/Steam"),
        ),
        (
            SteamPackaging::Flatpak,
            home.join(".var/app/com.valvesoftware.Steam/.local/share/Steam"),
        ),
        (
            SteamPackaging::Snap,
            home.join("snap/steam/common/.steam/root"),
        ),
        (
            SteamPackaging::Native,
            environment_path("XDG_DATA_HOME", home.join(".local/share")).join("Steam"),
        ),
        (SteamPackaging::Native, home.join(".steam/root")),
        (SteamPackaging::Native, home.join(".steam/steam")),
        (
            SteamPackaging::Native,
            home.join(".steam/debian-installation"),
        ),
    ] {
        if same_steam_root(target, &base) {
            return Ok(packaging);
        }
    }
    bail!(
        "This custom root does not identify a detected Steam client; select a detected Steam target for official actions"
    )
}

fn flatpak(target: &Target) -> bool {
    matches!(steam_packaging(target), Ok(SteamPackaging::Flatpak))
}

/// Open the official install/update action in the selected Steam packaging, not
/// whichever Steam happens to own the desktop URI handler. Custom targets must
/// point at a detected Steam compatibilitytools.d to identify that installation.
pub fn steam_install(target: &Target, app: u32) -> Result<()> {
    ensure!(
        target.kind == TargetKind::Steam,
        "Official Proton builds require a Steam target"
    );
    ensure!(app != 0, "Invalid Steam tool application ID");
    steam_root(target)?;
    steam_catalog::Catalog::load(target)?.validate_app(app, host_arch())?;
    steam_uri(target, &format!("steam://install/{app}"))
}

/// Steam owns these package files; its client performs the uninstall confirmation.
pub fn steam_uninstall(target: &Target, app: u32) -> Result<()> {
    let inventory = crate::steam::inventory(std::slice::from_ref(target))?;
    ensure!(
        inventory.warnings.is_empty()
            && inventory
                .steam_tools
                .iter()
                .filter(|tool| tool.app_id == app)
                .count()
                == 1,
        "This application is not an unambiguous installed Steam-managed tool"
    );
    steam_uri(target, &format!("steam://uninstall/{app}"))
}

fn steam_uri(target: &Target, uri: &str) -> Result<()> {
    let mut command = match steam_packaging(target)? {
        SteamPackaging::Flatpak => {
            let mut command = Command::new("flatpak");
            command.args(["run", "com.valvesoftware.Steam"]);
            command
        }
        SteamPackaging::Snap => {
            let mut command = Command::new("snap");
            command.args(["run", "steam"]);
            command
        }
        SteamPackaging::Native => Command::new("steam"),
    };
    let mut child = command
        .arg(uri)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("Opening the selected Steam installation")?;
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

fn tkg_ci(target: &Target) -> Result<Vec<Release>> {
    if !STEAM_HEROIC.contains(&target.kind) || host_arch() != "x86_64" {
        return Ok(Vec::new());
    }
    // Current upstream Steam Runtime build. The old arch-nopackage workflow used
    // by older ProtonUp versions is disabled; never offer expired CI artifacts.
    let repo = "Frogging-Family/wine-tkg-git";
    let data = github(&format!(
        "{repo}/actions/workflows/proton-valvexbe-sniper.yml/runs?status=success&per_page=5"
    ))?;
    let mut releases = Vec::new();
    for run in data
        .get("workflow_runs")
        .and_then(Value::as_array)
        .context("Missing TKG workflow runs")?
    {
        let id = run
            .get("id")
            .and_then(Value::as_u64)
            .context("Missing TKG run ID")?;
        let data = github(&format!("{repo}/actions/runs/{id}/artifacts?per_page=100"))?;
        for artifact in data
            .get("artifacts")
            .and_then(Value::as_array)
            .context("Missing TKG artifacts")?
        {
            if artifact.get("expired").and_then(Value::as_bool) != Some(false)
                || text(artifact, "name") != "proton-tkg-build"
            {
                continue;
            }
            let size = artifact
                .get("size_in_bytes")
                .and_then(Value::as_u64)
                .context("Missing TKG artifact size")?;
            if size == 0 || size > MAX_DOWNLOAD {
                continue;
            }
            releases.push(Release { id: format!("proton-tkg:ci-{id}"), version: format!("TKG Steam Runtime CI #{id}"), notes: format!("Upstream Valve experimental-bleeding-edge Steam Runtime build. Prerelease CI artifacts may regress and expire.\n\n{}", run.get("head_commit").map(|c| text(c, "message")).unwrap_or("")), url: text(run, "html_url").into(), published: text(artifact, "created_at").into(), prerelease: true, artifact: Some(Artifact { url: format!("https://nightly.link/{repo}/actions/runs/{id}/proton-tkg-build.zip"), name: "proton-tkg-build.zip".into(), size }), steam_app: None, availability: format!("Selected proton-tkg-build.zip: upstream CI binary (nightly.link download of GitHub Actions artifact), x86-64 baseline; CPU/OS-compatible with {}. Experimental.", crate::proton_cpu::host_profile().description()) });
        }
    }
    Ok(releases)
}

fn official_action(release: &mut Release, key: &str, catalog: &Result<steam_catalog::Catalog>) {
    let action = catalog
        .as_ref()
        .map_err(|error| format!("{error:#}"))
        .and_then(|catalog| {
            catalog
                .app_for(key, host_arch())
                .map_err(|error| format!("{error:#}"))
        });
    match action {
        Ok(app) => {
            release.steam_app = Some(app);
            release.availability = "Steam-managed: opens the selected client's install/update action for this series; Steam selects the current point release and checks compatibility. The app ID comes from that client's current Tools metadata, not a built-in catalog. No game mapping is changed.".into();
        }
        Err(reason) => {
            release.steam_app = None;
            release.availability = format!(
                "Official Steam action unavailable: {reason}. GitHub source archives are not substituted for a binary."
            );
        }
    }
}

fn official_releases(provider: &Provider, target: &Target) -> Result<Vec<Release>> {
    let catalog = steam_packaging(target).and_then(|_| steam_catalog::Catalog::load(target));
    let mut result = Vec::new();
    let mut experimental = Release {
        id: "valve-proton:experimental".into(),
        version: "Proton Experimental".into(),
        notes: "Valve's continuously updated experimental Proton branch. Steam manages the binaries, runtime dependencies, updates, and game integration. The install action is available only when the selected client's metadata advertises this tool.\n\nChangelog: https://github.com/ValveSoftware/Proton/wiki/Changelog".into(),
        url: "https://github.com/ValveSoftware/Proton/wiki/Changelog".into(),
        published: String::new(),
        prerelease: true,
        artifact: None,
        steam_app: None,
        availability: String::new(),
    };
    official_action(&mut experimental, "experimental", &catalog);
    result.push(experimental);
    let metadata = github(&format!("{}/releases?per_page=30", provider.repo)).and_then(|data| {
        ensure!(data.is_array(), "GitHub did not return a release list");
        Ok(data)
    });
    let notes_error = match metadata {
        Ok(data) => {
            for value in data
                .as_array()
                .context("GitHub did not return a release list")?
            {
                if value.get("draft").and_then(Value::as_bool) == Some(true) {
                    continue;
                }
                let mut release = release_from(provider, target, value)?;
                official_action(&mut release, text(value, "tag_name"), &catalog);
                result.push(release);
            }
            None
        }
        Err(error) => Some(format!("GitHub release notes are unavailable: {error:#}")),
    };
    // Steam may advertise a current/future series before GitHub publishes a tag,
    // or an older series outside GitHub's current page. Keep its real action.
    if let Ok(available) = &catalog {
        for series in available.series() {
            let Ok(app) = available.app_for(series, host_arch()) else {
                continue;
            };
            if result.iter().any(|release| release.steam_app == Some(app)) {
                continue;
            }
            let mut release = Release {
                id: format!("valve-proton:steam-{app}"),
                version: available
                    .name_for(app)
                    .context("Steam catalog tool has no name")?
                    .into(),
                notes: format!(
                    "This official Proton series is advertised by the selected Steam client's Tools metadata. Steam chooses and maintains its current build. No matching GitHub release notes were retrieved.{}",
                    notes_error
                        .as_ref()
                        .map(|error| format!("\n\n{error}"))
                        .unwrap_or_default()
                ),
                url: "https://github.com/ValveSoftware/Proton/wiki/Changelog".into(),
                published: String::new(),
                prerelease: series == "experimental",
                artifact: None,
                steam_app: None,
                availability: String::new(),
            };
            official_action(&mut release, series, &catalog);
            result.push(release);
        }
    }
    if let Some(error) = notes_error {
        result[0].notes.push_str(&format!("\n\n{error}"));
    }
    Ok(result)
}

pub fn releases(id: &str, target: &Target) -> Result<Vec<Release>> {
    let provider = provider(id)?;
    if id == "valve-proton" {
        return official_releases(&provider, target);
    }
    let data = github(&format!("{}/releases?per_page=30", provider.repo))?;
    let mut result = Vec::new();
    if id == "proton-tkg" {
        match tkg_ci(target) {
            Ok(releases) => result.extend(releases),
            Err(error) => result.push(Release { id: "proton-tkg:ci-unavailable".into(), version: "Current Proton-TKG CI builds".into(), notes: format!("Could not retrieve the current upstream CI artifacts: {error:#}\nLegacy binary releases, when available, are listed separately."), url: "https://github.com/Frogging-Family/wine-tkg-git/actions/workflows/proton-valvexbe-sniper.yml".into(), published: String::new(), prerelease: true, artifact: None, steam_app: None, availability: "Current CI metadata unavailable; open upstream Actions for build status. No source-code archive is substituted.".into() }),
        }
    }
    for value in data
        .as_array()
        .context("GitHub did not return a release list")?
    {
        if value.get("draft").and_then(Value::as_bool) == Some(true) {
            continue;
        }
        result.push(release_from(&provider, target, value)?);
    }
    if id == "steamtinkerlaunch"
        && !flatpak(target)
        && let Err(error) = stl_dependencies()
    {
        for release in &mut result {
            release.availability.push_str(&format!(
                "\nUnavailable until prerequisites are installed separately: {error:#}"
            ));
        }
    }
    Ok(result)
}

fn safe_name(value: &str) -> Result<String> {
    let name: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                c
            } else {
                '-'
            }
        })
        .collect();
    ensure!(
        !name.is_empty() && !name.starts_with('.') && name.len() <= 180,
        "Unsafe installation directory name"
    );
    Ok(name)
}

fn check_artifact(target: &Target, release: &Release) -> Result<Provider> {
    check_artifact_for_cpu(target, release, crate::proton_cpu::host_profile())
}

fn check_artifact_for_cpu(
    target: &Target,
    release: &Release,
    cpu: &CpuProfile,
) -> Result<Provider> {
    let (id, _) = release
        .id
        .split_once(':')
        .context("Release has no provider ID")?;
    let provider = provider(id)?;
    ensure!(
        provider.targets.contains(&target.kind),
        "Provider does not support this launcher"
    );
    let artifact = release.artifact.as_ref().context("This release requires its advertised Steam or upstream installation action, not a standalone download")?;
    ensure!(artifact.size <= MAX_DOWNLOAD, "Artifact is too large");
    ensure!(
        Path::new(&artifact.name).components().count() == 1
            && artifact.name != "."
            && artifact.name != "..",
        "Unsafe artifact filename"
    );
    let standard = standard_asset_url(&provider, &artifact.name, &artifact.url);
    if id == "steamtinkerlaunch" {
        ensure!(
            !flatpak(target),
            "Use the official SteamTinkerLaunch Flatpak extension for Flatpak Steam"
        );
        ensure!(
            artifact
                .url
                .starts_with("https://api.github.com/repos/sonic2kk/steamtinkerlaunch/tarball/")
                && artifact.name.ends_with(".tar.gz"),
            "Unexpected interpreted-tool source archive"
        );
    } else if id == "proton-tkg" && artifact.name == "proton-tkg-build.zip" {
        let run = release
            .id
            .strip_prefix("proton-tkg:ci-")
            .context("Invalid TKG CI release ID")?;
        ensure!(
            cpu.architecture == "x86_64"
                && !run.is_empty()
                && run.bytes().all(|c| c.is_ascii_digit())
                && artifact.url
                    == format!(
                        "https://nightly.link/Frogging-Family/wine-tkg-git/actions/runs/{run}/proton-tkg-build.zip"
                    ),
            "Unexpected TKG CI artifact URL or unsupported architecture"
        );
    } else {
        ensure!(
            standard && binary_matches(id, &artifact.name, cpu),
            "Artifact is not a CPU/OS-compatible binary from this provider for {}",
            cpu.description()
        );
    }
    Ok(provider)
}

fn executable(name: &str) -> bool {
    std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()).any(|dir| {
        fs::metadata(dir.join(name))
            .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

fn stl_dependencies() -> Result<()> {
    let missing: Vec<_> = [
        "bash", "awk", "git", "pgrep", "pidof", "pkill", "tar", "unzip", "wget", "xdotool",
        "xprop", "xrandr", "xwininfo", "xxd", "yad",
    ]
    .into_iter()
    .filter(|name| !executable(name))
    .collect();
    ensure!(
        missing.is_empty(),
        "SteamTinkerLaunch requires these upstream runtime dependencies (install separately): {}. See https://github.com/sonic2kk/steamtinkerlaunch/wiki/Installation",
        missing.join(", ")
    );
    let output = Command::new("yad")
        .arg("--version")
        .stdin(Stdio::null())
        .output()
        .context("Reading installed Yad version")?;
    ensure!(
        output.status.success(),
        "Could not determine installed Yad version; SteamTinkerLaunch requires Yad ≥ 7.2"
    );
    let version = String::from_utf8_lossy(&output.stdout);
    let mut pieces = version.trim().split('.');
    let major = pieces
        .next()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    let minor = pieces
        .next()
        .and_then(|s| s.split_whitespace().next())
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    ensure!(
        (major, minor) >= (7, 2),
        "SteamTinkerLaunch requires Yad ≥ 7.2; found {}",
        version.trim()
    );
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn setup_stl(tool: &Path, destination: &Path) -> Result<()> {
    // Reproduce the reviewed data-only Makefile layout and CompatTool manifests.
    // No `make`, shell, or downloaded executable is invoked during installation.
    let script_path = tool.join("steamtinkerlaunch");
    let script = String::from_utf8(read_limited(&script_path, 8 * 1024 * 1024)?)?;
    ensure!(
        script.lines().any(|line| line == "PREFIX=\"/usr\"")
            && script
                .lines()
                .any(|line| line == "PROGNAME=\"SteamTinkerLaunch\"")
            && script.lines().any(|line| line == "SHOSTL=\"stl\""),
        "Upstream SteamTinkerLaunch layout changed; refusing an unreviewed installer layout"
    );
    let prefix = destination
        .to_str()
        .context("SteamTinkerLaunch path must be UTF-8")?;
    ensure!(
        !prefix.chars().any(char::is_control),
        "SteamTinkerLaunch path contains control characters"
    );
    let script = script.replacen(
        "PREFIX=\"/usr\"",
        &format!("PREFIX={}", shell_quote(prefix)),
        1,
    );
    fs::write(&script_path, script)?;
    fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;
    let data = tool.join("share/steamtinkerlaunch");
    fs::create_dir_all(&data)?;
    for name in ["collections", "eval", "guicfgs", "lang", "misc"] {
        let source = tool.join(name);
        ensure!(
            fs::symlink_metadata(&source)?.is_dir(),
            "SteamTinkerLaunch release is missing its {name} data directory"
        );
        fs::rename(source, data.join(name))?;
    }
    fs::write(
        tool.join("compatibilitytool.vdf"),
        "\"compatibilitytools\" { \"compat_tools\" { \"Proton-stl\" { \"install_path\" \".\" \"display_name\" \"Steam Tinker Launch\" \"from_oslist\" \"windows\" \"to_oslist\" \"linux\" } } }\n",
    )?;
    fs::write(
        tool.join("toolmanifest.vdf"),
        "\"manifest\" { \"commandline\" \"/steamtinkerlaunch run\" \"commandline_waitforexitandrun\" \"/steamtinkerlaunch waitforexitandrun\" }\n",
    )?;
    Ok(())
}

/// Linux no-replace rename makes the final cutover atomic even against another
/// installer. A preliminary exists() check alone would race and overwrite dirs.
fn rename_noreplace(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    let result = unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result != 0 {
        return Err(std::io::Error::last_os_error())
            .context("Atomically installing tool without overwriting an existing path");
    }
    Ok(())
}

pub fn install(
    target: &Target,
    release: &Release,
    options: &InstallOptions,
    cancel: &AtomicBool,
    progress: &mut dyn FnMut(Progress),
) -> Result<InstalledTool> {
    archive::cancelled(cancel)?;
    let provider = check_artifact(target, release)?;
    let artifact = release
        .artifact
        .as_ref()
        .context("Missing release artifact")?;
    ensure!(
        target.root.is_absolute()
            && !target
                .root
                .components()
                .any(|c| matches!(c, Component::ParentDir)),
        "Tool root must be an absolute path without parent traversal"
    );
    if provider.id == "steamtinkerlaunch" {
        stl_dependencies()?;
    }
    fs::create_dir_all(&target.root)
        .with_context(|| format!("Creating tool root {}", target.root.display()))?;
    let root = fs::canonicalize(&target.root)?;
    ensure!(
        root.parent().is_some(),
        "Filesystem root cannot be a tool target"
    );
    for tool in installed(target)? {
        if tool.managed && valid_receipt(target, &tool.path, &tool.name)?.release_id == release.id {
            bail!(
                "This release is already installed at {}",
                tool.path.display()
            );
        }
    }
    let stage = tempfile::Builder::new()
        .prefix(".hyprdeck-install-")
        .tempdir_in(&root)?;
    let downloaded = stage.path().join("download");
    progress(Progress {
        message: format!("Downloading {}", artifact.name),
        fraction: Some(0.0),
    });
    download(
        &artifact.url,
        &downloaded,
        artifact.size,
        false,
        cancel,
        progress,
    )?;
    progress(Progress {
        message: "Validating and extracting archive (no installer code is executed)".into(),
        fraction: Some(0.8),
    });
    let tool = archive::extract(
        &downloaded,
        &artifact.name,
        &stage.path().join("unpacked"),
        cancel,
    )?;
    let name = if provider.id == "steamtinkerlaunch" {
        "SteamTinkerLaunch".to_owned()
    } else {
        safe_name(
            tool.file_name()
                .and_then(|n| n.to_str())
                .context("Tool archive directory is not UTF-8")?,
        )?
    };
    let destination = root.join(&name);
    ensure!(
        !destination.try_exists()? && fs::symlink_metadata(&destination).is_err(),
        "An existing tool/path is already installed at {}",
        destination.display()
    );
    if provider.id == "steamtinkerlaunch" {
        setup_stl(&tool, &destination)?;
    }
    if options.repair_prefix_parser {
        progress(Progress {
            message: "Inspecting the staged prefix-version parser without executing runner code"
                .into(),
            fraction: Some(0.9),
        });
        let status = crate::proton_repair::repair_staged(&tool)?;
        progress(Progress {
            message: match status {
                crate::proton_repair::Status::Applied { .. } => {
                    "Verified parser repair applied; original upstream source preserved".into()
                }
                crate::proton_repair::Status::NotApplicable { reason } => reason,
                _ => unreachable!("staged repair only applies or reports an upstream fix"),
            },
            fraction: Some(0.92),
        });
    }
    let tools = describe(target, &tool)?;
    ensure!(
        tools.len() == 1 && tools[0].path == tool,
        "Archive must declare exactly one tool with install_path '.'"
    );
    let mut installed = tools
        .into_iter()
        .next()
        .context("Archive has no compatibility tool")?;
    ensure!(
        tool.join("proton").is_file() || tool.join("steamtinkerlaunch").is_file(),
        "Archive does not contain its compatibility-tool entrypoint"
    );
    let receipt = Receipt {
        owner: "Hyprdeck".into(),
        schema: 1,
        target_id: target.id.clone(),
        root: root.clone(),
        directory: name,
        tool_id: installed.name.clone(),
        release_id: release.id.clone(),
        source: artifact.url.clone(),
    };
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tool.join(RECEIPT))?;
    file.write_all(&serde_json::to_vec_pretty(&receipt)?)?;
    file.sync_all()?;
    archive::cancelled(cancel)?;
    progress(Progress {
        message: "Committing installation atomically".into(),
        fraction: Some(0.98),
    });
    rename_noreplace(&tool, &destination)?;
    installed.path = destination;
    installed.managed = true;
    progress(Progress {
        message: "Installed. Restart the launcher to discover the tool.".into(),
        fraction: Some(1.0),
    });
    Ok(installed)
}

pub(super) fn require_stopped(target: &Target) -> Result<()> {
    let uid = unsafe { libc::geteuid() };
    for entry in fs::read_dir("/proc").context("Checking running launchers and Wine processes")? {
        let entry = entry?;
        if !entry.file_name().as_bytes().iter().all(u8::is_ascii_digit) {
            continue;
        }
        let path = entry.path();
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        if meta.uid() != uid {
            continue;
        }
        let Ok(name) = fs::read_to_string(path.join("comm")) else {
            continue;
        };
        let name = name.trim().to_ascii_lowercase();
        let wine = name.starts_with("wine")
            || name == "wineserver"
            || name == "proton"
            || name == "umu-run";
        let launcher = match target.kind {
            TargetKind::Steam => {
                name == "steam" || name.starts_with("steamwebhelper") || name == "steamtinkerlaunc"
            }
            TargetKind::Lutris => name == "lutris",
            TargetKind::Heroic => name == "heroic" || name == "heroicgameslaun",
            TargetKind::Bottles => name == "bottles" || name == "bottles-cli",
        };
        ensure!(
            !wine && !launcher,
            "Close {:?} and all Wine/Proton games before changing a compatibility tool (running: {name})",
            target.kind
        );
    }
    Ok(())
}

fn ensure_unreferenced(target: &Target, tool: &InstalledTool) -> Result<()> {
    if target.kind != TargetKind::Steam {
        return Ok(());
    }
    // Include every detected installation sharing this canonical tool directory,
    // not just a custom alias whose lexical parent may be unrelated to Steam.
    steam_root(target).context("Cannot establish the Steam owner; removal is blocked")?;
    let root = fs::canonicalize(&target.root)?;
    let mut owners = vec![target.clone()];
    owners.extend(targets()?.into_iter().filter(|candidate| {
        candidate.kind == TargetKind::Steam
            && fs::canonicalize(&candidate.root).is_ok_and(|candidate_root| candidate_root == root)
    }));
    let inventory = crate::steam::inventory(&owners)?;
    ensure!(
        inventory.warnings.is_empty(),
        "Steam mappings could not be completely inspected; removal is blocked"
    );
    ensure!(
        !inventory
            .mappings
            .iter()
            .any(|mapping| mapping.tool.as_deref() == Some(&tool.name)),
        "A saved Steam game or default mapping still references this tool; change it in Steam before removal"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_and_uninstalled_game_mappings_block_tool_removal() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("steamapps")).unwrap();
        fs::create_dir_all(root.path().join("config")).unwrap();
        let target = target(root.path().join("compatibilitytools.d"));
        fs::create_dir_all(&target.root).unwrap();
        let alias_path = root.path().join("tools-alias");
        std::os::unix::fs::symlink(&target.root, &alias_path).unwrap();
        let alias = Target {
            root: alias_path,
            launcher_root: None,
            ..target.clone()
        };
        let tool = InstalledTool {
            name: "in-use".into(),
            display_name: "Fixture".into(),
            path: target.root.join("Fixture"),
            target_id: target.id.clone(),
            managed: true,
        };
        for app_id in [0, 99] {
            fs::write(
                root.path().join("config/config.vdf"),
                format!(r#""CompatToolMapping" {{ "{app_id}" {{ "name" "in-use" }} }}"#),
            )
            .unwrap();
            assert!(ensure_unreferenced(&target, &tool).is_err());
            assert!(ensure_unreferenced(&alias, &tool).is_err());
        }
        fs::write(
            root.path().join("config/config.vdf"),
            r#""CompatToolMapping" { "0" { "name" "different-tool" } }"#,
        )
        .unwrap();
        ensure_unreferenced(&target, &tool).unwrap();
        ensure_unreferenced(&alias, &tool).unwrap();
    }

    fn cpu(architecture: &str, x86_64_level: Option<u8>) -> CpuProfile {
        CpuProfile {
            architecture: architecture.into(),
            x86_64_level,
        }
    }

    fn release_assets(provider: &Provider, names: &[&str]) -> Value {
        serde_json::json!({"tag_name": "fixture-release", "assets": names.iter().map(|name| {
            serde_json::json!({"name": name, "size": 100, "browser_download_url": format!(
                "https://github.com/{}/releases/download/fixture-release/{name}", provider.repo
            )})
        }).collect::<Vec<_>>()})
    }

    #[test]
    fn compression_preference_applies_within_the_highest_compatible_tier() {
        let provider = provider("ge-proton").unwrap();
        let names = [
            "GE-Proton42-1-x86_64.tar.zst",
            "GE-Proton42-1-x86_64_v3.tar.gz",
            "GE-Proton42-1-x86_64_v3.tar.xz",
            "GE-Proton42-1-x86_64_v3.tar.zst",
        ];
        for (advertised, expected) in [(&names[..], names[3]), (&names[..3], names[2])] {
            let value = release_assets(&provider, advertised);
            assert_eq!(
                asset(&provider, &value, &cpu("x86_64", Some(3)))
                    .unwrap()
                    .unwrap()
                    .name,
                expected
            );
        }
    }

    #[test]
    fn different_abi_builds_are_not_compression_duplicates() {
        let provider = provider("ge-proton").unwrap();
        let ambiguous = release_assets(
            &provider,
            &[
                "GE-Proton42-1-x86_64_v3.tar.zst",
                "GE-Proton42-1-musl-x86_64_v3.tar.xz",
            ],
        );
        assert!(asset(&provider, &ambiguous, &cpu("x86_64", Some(3))).is_err());
        // An ambiguous lower tier does not hide an unambiguous preferred tier.
        let preferred = release_assets(
            &provider,
            &[
                "GE-Proton42-1-x86_64.tar.zst",
                "GE-Proton42-1-musl-x86_64.tar.xz",
                "GE-Proton42-1-x86_64_v3.tar.xz",
            ],
        );
        assert_eq!(
            asset(&provider, &preferred, &cpu("x86_64", Some(3)))
                .unwrap()
                .unwrap()
                .name,
            "GE-Proton42-1-x86_64_v3.tar.xz"
        );
    }

    fn target(root: PathBuf) -> Target {
        Target {
            id: "fixture".into(),
            name: "Fixture".into(),
            root,
            kind: TargetKind::Steam,
            launcher_root: None,
            steam_packaging: None,
        }
    }

    #[test]
    fn baseline_remains_available_and_unsupported_advertised_tiers_are_skipped() {
        let provider = provider("proton-cachyos").unwrap();
        let names = [
            "proton-cachyos-42.0-fixture-slr-x86_64.tar.xz",
            "proton-cachyos-42.0-fixture-slr-x86_64_v2.tar.xz",
            "proton-cachyos-42.0-fixture-slr-x86_64_v3.tar.xz",
            "proton-cachyos-42.0-fixture-slr-x86_64_v4.tar.xz",
        ];
        let baseline = release_assets(&provider, &names[..1]);
        assert_eq!(
            asset(&provider, &baseline, &cpu("x86_64", Some(4)))
                .unwrap()
                .unwrap()
                .name,
            names[0]
        );
        for level in 1..=4 {
            // Reverse order exercises selection independently of upstream asset order.
            let advertised = release_assets(&provider, &[names[3], names[2], names[1], names[0]]);
            assert_eq!(
                asset(&provider, &advertised, &cpu("x86_64", Some(level)))
                    .unwrap()
                    .unwrap()
                    .name,
                names[usize::from(level - 1)]
            );
        }
        let too_new = release_assets(&provider, &names[3..]);
        assert!(
            asset(&provider, &too_new, &cpu("x86_64", Some(3)))
                .unwrap()
                .is_none()
        );
        let sparse = release_assets(&provider, &[names[3], names[1]]);
        assert_eq!(
            asset(&provider, &sparse, &cpu("x86_64", Some(3)))
                .unwrap()
                .unwrap()
                .name,
            names[1]
        );
    }

    #[test]
    fn current_architecture_suffixes_and_legacy_ge_assets_are_supported() {
        let x86 = cpu("x86_64", Some(3));
        let arm = cpu("aarch64", None);
        let ge = provider("ge-proton").unwrap();
        let ge_names = [
            "GE-Proton42-1-x86_64.tar.gz",
            "GE-Proton42-1-aarch64.tar.gz",
        ];
        let value = release_assets(&ge, &ge_names);
        assert_eq!(asset(&ge, &value, &x86).unwrap().unwrap().name, ge_names[0]);
        assert_eq!(asset(&ge, &value, &arm).unwrap().unwrap().name, ge_names[1]);
        assert!(binary_matches("ge-proton", "GE-Proton42-1.tar.gz", &x86));
        assert!(!binary_matches("ge-proton", "GE-Proton42-1.tar.gz", &arm));
        let cachy = provider("proton-cachyos").unwrap();
        let cachy_names = [
            "proton-cachyos-42.0-fixture-slr-x86_64.tar.xz",
            "proton-cachyos-42.0-fixture-slr-x86_64_v3.tar.xz",
            "proton-cachyos-42.0-fixture-slr-arm64.tar.xz",
        ];
        let value = release_assets(&cachy, &cachy_names);
        assert_eq!(
            asset(&cachy, &value, &x86).unwrap().unwrap().name,
            cachy_names[1]
        );
        assert_eq!(
            asset(&cachy, &value, &arm).unwrap().unwrap().name,
            cachy_names[2]
        );
        let wineland = provider("proton-wineland").unwrap();
        let wineland_names = [
            "proton-wineland-42.0-fixture-x86_64.tar.xz",
            "proton-wineland-42.0-fixture-x86_64_v3.tar.xz",
            "proton-wineland-42.0-fixture-x86_64_wow64.tar.xz",
        ];
        let value = release_assets(&wineland, &wineland_names);
        assert_eq!(
            asset(&wineland, &value, &x86).unwrap().unwrap().name,
            wineland_names[1]
        );
        assert!(asset(&wineland, &value, &arm).unwrap().is_none());
        assert!(
            asset(&ge, &release_assets(&ge, &ge_names), &cpu("riscv64", None))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn sources_native_packages_unknown_tiers_and_alternate_abis_are_not_selected() {
        let x86 = cpu("x86_64", Some(4));
        for (provider, name) in [
            ("proton-cachyos", "proton-cachyos-42.0-native-x86_64.tar.xz"),
            (
                "proton-cachyos",
                "proton-cachyos-42.0-native-slr-x86_64_v3.tar.xz",
            ),
            ("proton-cachyos", "proton-cachyos-42.0-slr-x86_64_v5.tar.xz"),
            ("proton-tkg", "wine-tkg-staging-x86_64.pkg.tar.zst"),
            ("proton-tkg", "proton-tkg-source.tar.gz"),
            ("ge-proton", "GE-Proton42-1-debug-x86_64.tar.gz"),
            ("ge-proton", "GE-Proton42-1-riscv64.tar.gz"),
            ("ge-proton", "GE-Proton42-1-v4.tar.gz"),
            (
                "proton-wineland",
                "proton-wineland-42.0-fixture-x86_64_wow64.tar.xz",
            ),
            (
                "proton-wineland",
                "proton-wineland-42.0-fixture-x86_64_v3.sha512sum",
            ),
        ] {
            assert!(!binary_matches(provider, name, &x86), "{name}");
        }
        assert!(binary_matches(
            "proton-tkg",
            "proton_tkg_42.0-valve.tar.gz",
            &x86
        ));
    }

    #[test]
    fn release_availability_records_selected_artifact_and_cpu_suitability() {
        let provider = provider("proton-cachyos").unwrap();
        let name = "proton-cachyos-42.0-fixture-slr-x86_64_v3.tar.xz";
        let value = release_assets(&provider, &[name]);
        let target = target(PathBuf::from("/fixture"));
        let release =
            release_from_for_cpu(&provider, &target, &value, &cpu("x86_64", Some(3))).unwrap();
        assert!(release.availability.contains(name));
        assert!(release.availability.contains("CPU/OS-compatible"));
        assert!(release.availability.contains("usable x86-64-v3"));
        let unsupported =
            release_from_for_cpu(&provider, &target, &value, &cpu("x86_64", Some(2))).unwrap();
        assert!(unsupported.artifact.is_none());
        assert!(
            unsupported
                .availability
                .contains("No CPU/OS-compatible binary")
        );
    }

    #[test]
    fn manual_artifacts_cannot_bypass_isa_architecture_or_filename_checks() {
        let provider = provider("proton-cachyos").unwrap();
        let names = [
            "proton-cachyos-42.0-fixture-slr-x86_64_v3.tar.xz",
            "proton-cachyos-42.0-fixture-slr-x86_64_v4.tar.xz",
        ];
        let target = target(PathBuf::from("/fixture"));
        let mut release = release_from_for_cpu(
            &provider,
            &target,
            &release_assets(&provider, &names),
            &cpu("x86_64", Some(4)),
        )
        .unwrap();
        assert!(check_artifact_for_cpu(&target, &release, &cpu("x86_64", Some(4))).is_ok());
        assert!(check_artifact_for_cpu(&target, &release, &cpu("x86_64", Some(3))).is_err());
        assert!(check_artifact_for_cpu(&target, &release, &cpu("aarch64", None)).is_err());
        // A compatible label must not disguise a URL to an unsupported build.
        release.artifact.as_mut().unwrap().name = names[0].into();
        assert!(check_artifact_for_cpu(&target, &release, &cpu("x86_64", Some(3))).is_err());
        release.artifact = asset(
            &provider,
            &release_assets(&provider, &names[..1]),
            &cpu("x86_64", Some(3)),
        )
        .unwrap();
        assert!(check_artifact_for_cpu(&target, &release, &cpu("x86_64", Some(3))).is_ok());
        assert!(check_artifact_for_cpu(&target, &release, &cpu("x86_64", Some(2))).is_err());
    }

    #[test]
    fn discovers_internal_ids_and_actual_install_paths_not_folder_names() {
        let dir = tempfile::tempdir().unwrap();
        let tool = dir.path().join("folder-name");
        fs::create_dir_all(tool.join("runtime")).unwrap();
        fs::write(tool.join("compatibilitytool.vdf"), "\"compatibilitytools\" { \"compat_tools\" { \"actual-tool-id\" { \"install_path\" \"runtime\" \"display_name\" \"Friendly name\" } } }").unwrap();
        let tools = installed(&target(dir.path().to_path_buf())).unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "actual-tool-id");
        assert_eq!(tools[0].display_name, "Friendly name");
        assert_eq!(tools[0].path, tool.join("runtime"));
        assert!(!tools[0].managed);
    }

    #[test]
    fn receipts_cannot_authorize_another_root_or_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let target = target(dir.path().to_path_buf());
        let tool = dir.path().join("tool");
        fs::create_dir(&tool).unwrap();
        let receipt = Receipt {
            owner: "Hyprdeck".into(),
            schema: 1,
            target_id: target.id.clone(),
            root: dir.path().to_path_buf(),
            directory: "tool".into(),
            tool_id: "internal".into(),
            release_id: "fixture".into(),
            source: "fixture".into(),
        };
        fs::write(tool.join(RECEIPT), serde_json::to_vec(&receipt).unwrap()).unwrap();
        assert!(valid_receipt(&target, &tool, "internal").is_ok());
        assert!(valid_receipt(&target, &tool, "different-id").is_err());
        assert!(
            valid_receipt(
                &Target {
                    id: "different".into(),
                    ..target.clone()
                },
                &tool,
                "internal"
            )
            .is_err()
        );
        std::os::unix::fs::symlink(&tool, dir.path().join("link")).unwrap();
        assert!(valid_receipt(&target, &dir.path().join("link"), "internal").is_err());
    }

    #[test]
    fn atomic_cutover_never_replaces_existing_directory() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        let destination = dir.path().join("existing");
        fs::create_dir(&source).unwrap();
        fs::create_dir(&destination).unwrap();
        assert!(rename_noreplace(&source, &destination).is_err());
        assert!(source.is_dir());
        assert!(destination.is_dir());
    }

    #[test]
    fn official_builds_have_steam_actions_not_source_downloads() {
        let provider = provider("valve-proton").unwrap();
        let target = target(PathBuf::from("/fixture"));
        let value = serde_json::json!({"tag_name":"proton-11.0-2e","body":"Notes","html_url":"https://github.com/ValveSoftware/Proton/releases/tag/proton-11.0-2e","assets":[]});
        let release = release_from(&provider, &target, &value).unwrap();
        assert!(release.artifact.is_none());
        assert!(release.steam_app.is_none());
        assert!(check_artifact(&target, &release).is_err());
    }

    #[test]
    fn packaging_uses_canonical_roots_not_labels_or_incidental_components() {
        let directory = tempfile::tempdir().unwrap();
        let steam = directory.path().join("Steam");
        fs::create_dir_all(steam.join("steamapps")).unwrap();
        let link = directory.path().join("root-link");
        std::os::unix::fs::symlink(&steam, &link).unwrap();
        let mut selected = target(steam.join("compatibilitytools.d"));
        selected.name = "Arbitrary Flatpak Snap label".into();
        selected.launcher_root = Some(steam.clone());
        selected.steam_packaging = Some(SteamPackaging::Native);
        assert!(same_steam_root(&selected, &link));
        assert!(!flatpak(&selected));
        selected.root = directory.path().join("snap/custom/compatibilitytools.d");
        assert!(!same_steam_root(&selected, &steam));
        assert!(!flatpak(&selected));
    }

    #[test]
    fn interpreted_stl_setup_copies_layout_without_executing_script() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(source.join("steamtinkerlaunch"), "#!/bin/bash\nPREFIX=\"/usr\"\nPROGNAME=\"SteamTinkerLaunch\"\nSHOSTL=\"stl\"\ntouch /must-not-execute\n").unwrap();
        for name in ["collections", "eval", "guicfgs", "lang", "misc"] {
            fs::create_dir(source.join(name)).unwrap();
        }
        setup_stl(&source, Path::new("/fixture/SteamTinkerLaunch")).unwrap();
        assert!(source.join("share/steamtinkerlaunch/lang").is_dir());
        assert!(
            fs::read_to_string(source.join("steamtinkerlaunch"))
                .unwrap()
                .contains("PREFIX='/fixture/SteamTinkerLaunch'")
        );
        assert_eq!(
            describe(&target(dir.path().to_path_buf()), &source).unwrap()[0].name,
            "Proton-stl"
        );
    }
}
