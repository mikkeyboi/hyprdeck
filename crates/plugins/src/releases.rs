//! Fixed GitHub release assets, cached metadata, checksum verification, no auto-install.
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, ensure};
use hyprdeck_core::{notify, store};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::backend::{self, Stage};
use crate::process;
use crate::protocol::{MAX_DOCUMENT, Manifest, validate_repo};

const CACHE_SECONDS: u64 = 3600;
const MAX_BINARY: usize = 128 * 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Asset {
    name: String,
    browser_download_url: String,
    size: u64,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Release {
    tag_name: String,
    draft: bool,
    prerelease: bool,
    html_url: String,
    assets: Vec<Asset>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Cached {
    fetched_at: u64,
    release: Release,
    manifest: Manifest,
}
#[derive(Debug, Clone, Serialize)]
pub struct Check {
    pub id: String,
    pub installed_version: String,
    pub available_version: String,
    pub update_available: bool,
    pub url: String,
    pub fetched_at: u64,
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
fn cache_path(repo: &str) -> PathBuf {
    store::state_dir()
        .join("plugins/releases")
        .join(format!("{}.json", repo.replace('/', "_")))
}

async fn download(url: &str, limit: usize, github_api: bool) -> Result<Vec<u8>> {
    ensure!(
        url.starts_with("https://") && !url.chars().any(char::is_whitespace),
        "release URL must use HTTPS without whitespace"
    );
    let mut curl = Command::new("curl");
    curl.args([
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
        "90",
        "--max-filesize",
        &limit.to_string(),
        "--user-agent",
        "Hyprdeck-plugin-host/1",
    ]);
    if github_api {
        curl.args([
            "--header",
            "Accept: application/vnd.github+json",
            "--header",
            "X-GitHub-Api-Version: 2022-11-28",
        ]);
    }
    curl.arg(url);
    process::capture(curl, None, Duration::from_secs(95), limit).await.map(|output| output.stdout)
        .context("downloading GitHub release (curl is required; check network, repository, release assets and GitHub rate limit)")
}

fn asset<'a>(release: &'a Release, repo: &str, name: &str) -> Result<&'a Asset> {
    let matches: Vec<_> = release
        .assets
        .iter()
        .filter(|asset| asset.name == name)
        .collect();
    ensure!(
        matches.len() == 1,
        "release must contain exactly one asset named {name}"
    );
    let asset = matches[0];
    ensure!(
        asset
            .browser_download_url
            .starts_with(&format!("https://github.com/{repo}/releases/download/")),
        "asset {name} does not belong to {repo}"
    );
    Ok(asset)
}

fn validate_release(release: &Release, manifest: &Manifest, repo: &str) -> Result<()> {
    manifest.validate()?;
    ensure!(
        !release.draft && !release.prerelease,
        "only stable published releases are supported"
    );
    ensure!(
        manifest.update_repo == repo,
        "manifest repository {} does not match requested {repo}",
        manifest.update_repo
    );
    ensure!(
        release
            .tag_name
            .strip_prefix('v')
            .unwrap_or(&release.tag_name)
            == manifest.version,
        "release tag {} and manifest version {} disagree",
        release.tag_name,
        manifest.version
    );
    ensure!(
        release
            .html_url
            .starts_with(&format!("https://github.com/{repo}/releases/tag/")),
        "release page repository mismatch"
    );
    let binary = asset(release, repo, &manifest.asset)?;
    ensure!(
        binary.size > 0 && binary.size <= MAX_BINARY as u64,
        "release binary is empty or too large"
    );
    asset(release, repo, &format!("{}.sha256", manifest.asset))?;
    asset(release, repo, "plugin.json")?;
    Ok(())
}

async fn latest(repo: &str, force: bool) -> Result<Cached> {
    validate_repo(repo)?;
    let cache_path = cache_path(repo);
    if !force
        && let Ok(bytes) = std::fs::read(&cache_path)
        && let Ok(cached) = serde_json::from_slice::<Cached>(&bytes)
        && now() >= cached.fetched_at
        && now() - cached.fetched_at < CACHE_SECONDS
    {
        validate_release(&cached.release, &cached.manifest, repo)?;
        return Ok(cached);
    }
    let bytes = download(
        &format!("https://api.github.com/repos/{repo}/releases/latest"),
        MAX_DOCUMENT,
        true,
    )
    .await?;
    let release: Release =
        serde_json::from_slice(&bytes).context("invalid GitHub release metadata")?;
    let manifest_asset = asset(&release, repo, "plugin.json")?;
    let bytes = download(&manifest_asset.browser_download_url, MAX_DOCUMENT, false).await?;
    let manifest: Manifest =
        serde_json::from_slice(&bytes).context("invalid release plugin.json")?;
    validate_release(&release, &manifest, repo)?;
    let cached = Cached {
        fetched_at: now(),
        release,
        manifest,
    };
    if let Err(error) = store::write_atomic(&cache_path, &serde_json::to_vec(&cached)?) {
        tracing::warn!("caching plugin release: {error:#}");
    }
    Ok(cached)
}

fn check_for(old: &Manifest, cached: &Cached) -> Result<Check> {
    let new = &cached.manifest;
    ensure!(
        new.id == old.id
            && new.update_repo == old.update_repo
            && new.executable == old.executable
            && new.asset == old.asset,
        "release identity/repository/executable mismatch; trust cannot be transferred"
    );
    let available = semver::Version::parse(&new.version)?;
    let installed = semver::Version::parse(&old.version)?;
    ensure!(
        available >= installed,
        "latest release {} is older than installed {}; refusing downgrade",
        new.version,
        old.version
    );
    Ok(Check {
        id: old.id.clone(),
        installed_version: old.version.clone(),
        available_version: new.version.clone(),
        update_available: available > installed,
        url: cached.release.html_url.clone(),
        fetched_at: cached.fetched_at,
    })
}

pub async fn check(id: &str, force: bool) -> Result<Check> {
    let old = backend::installed(id)?;
    let cached = latest(&old.update_repo, force).await?;
    check_for(&old, &cached)
}

fn checksum(bytes: &[u8], filename: &str) -> Result<String> {
    let text = std::str::from_utf8(bytes).context("SHA256 asset is not UTF-8")?;
    let lines: Vec<_> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    ensure!(
        lines.len() == 1,
        "checksum asset must contain exactly one SHA256 line"
    );
    let fields: Vec<_> = lines[0].split_whitespace().collect();
    ensure!(
        fields.len() == 1 || fields.len() == 2,
        "invalid SHA256 line"
    );
    ensure!(
        fields[0].len() == 64 && fields[0].bytes().all(|byte| byte.is_ascii_hexdigit()),
        "invalid SHA256 digest"
    );
    if fields.len() == 2 {
        ensure!(
            fields[1].strip_prefix('*').unwrap_or(fields[1]) == filename,
            "SHA256 asset names a different executable asset"
        );
    }
    Ok(fields[0].to_ascii_lowercase())
}

async fn staged(cached: &Cached) -> Result<Stage> {
    let manifest = &cached.manifest;
    let repo = &manifest.update_repo;
    let sum_asset = asset(&cached.release, repo, &format!("{}.sha256", manifest.asset))?;
    let expected = checksum(
        &download(&sum_asset.browser_download_url, 4096, false).await?,
        &manifest.asset,
    )?;
    let executable = asset(&cached.release, repo, &manifest.asset)?;
    let bytes = download(&executable.browser_download_url, MAX_BINARY, false).await?;
    ensure!(
        bytes.len() as u64 == executable.size,
        "downloaded asset size differs from release metadata"
    );
    let stage = backend::stage_bytes(manifest, &bytes)?;
    let mut hash = Command::new("sha256sum");
    hash.arg("--").arg(stage.0.join(&manifest.executable));
    let actual = process::capture(hash, None, Duration::from_secs(15), 4096)
        .await
        .context("verifying binary checksum (sha256sum is required)")?;
    let actual = std::str::from_utf8(&actual.stdout)?
        .split_whitespace()
        .next()
        .context("missing sha256sum output")?;
    ensure!(
        actual.eq_ignore_ascii_case(&expected),
        "SHA256 mismatch; refusing installation and keeping existing plugin"
    );
    Ok(stage)
}

pub async fn install(repo: &str) -> Result<Manifest> {
    let _lock = backend::lock().await?;
    let cached = latest(repo, true).await?;
    let stage = staged(&cached).await?;
    backend::install_stage(stage, &cached.manifest)?;
    Ok(cached.manifest)
}

pub async fn update(id: &str) -> Result<Manifest> {
    let _lock = backend::lock().await?;
    let old = backend::installed(id)?;
    // Updates always re-fetch; a stale check never authorizes replacement.
    let cached = latest(&old.update_repo, true).await?;
    cached.manifest.validate_update(&old)?;
    let stage = staged(&cached).await?;
    backend::replace(stage, &cached.manifest, &old).await?;
    Ok(cached.manifest)
}

pub async fn background() {
    let mut notified: BTreeMap<String, String> =
        std::fs::read(store::state_dir().join("plugins/notified.json"))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
    tokio::time::sleep(Duration::from_secs(30)).await;
    loop {
        match backend::list() {
            Ok(plugins) => {
                for plugin in plugins {
                    if plugin.manifest.is_none() {
                        continue;
                    }
                    match check(&plugin.id, false).await {
                    Ok(check) if check.update_available && notified.get(&plugin.id) != Some(&check.available_version) => {
                        match notify::notify(notify::Category::System, notify::Severity::Normal, "Plugin update available", &format!("{}: {} → {}. Review and apply from Plugins; nothing is installed automatically.", plugin.id, check.installed_version, check.available_version)).await {
                            Ok(()) => {
                                notified.insert(plugin.id, check.available_version);
                                match serde_json::to_vec(&notified).map_err(anyhow::Error::from).and_then(|bytes| store::write_atomic(&store::state_dir().join("plugins/notified.json"), &bytes)) {
                                    Ok(()) => (), Err(error) => tracing::warn!("saving plugin notification state: {error:#}"),
                                }
                            }
                            Err(error) => tracing::warn!("plugin update notification: {error:#}"),
                        }
                    }
                    Ok(_) => (),
                    Err(error) => tracing::warn!(plugin = %plugin.id, "plugin release check: {error:#}"),
                }
                }
            }
            Err(error) => tracing::warn!("plugin inventory: {error:#}"),
        }
        tokio::time::sleep(Duration::from_secs(CACHE_SECONDS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn checksum_requires_exact_asset_and_one_digest() {
        let digest = "a".repeat(64);
        assert!(
            checksum(
                format!("{digest}  sample-linux-x86_64\n").as_bytes(),
                "sample-linux-x86_64"
            )
            .is_ok()
        );
        assert!(checksum(format!("{digest}  ../sample\n").as_bytes(), "sample").is_err());
        assert!(checksum(format!("{digest}\n{digest}\n").as_bytes(), "sample").is_err());
        assert!(checksum(b"not-a-hash", "sample").is_err());
    }
}
