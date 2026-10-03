//! Latest GitHub release lookup (unauthenticated REST API) with an on-disk cache
//! in the state dir so the 60 requests/hour anonymous limit is never a problem.

use std::path::PathBuf;

use anyhow::{Context, Result, anyhow, bail};
use hyprdeck_core::{cmd, store};
use serde::{Deserialize, Serialize};

use crate::parse;

/// Cached results younger than this are used without touching the network.
const CACHE_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Release {
    pub tag: String,
    pub name: String,
    /// Unix seconds.
    pub published_at: i64,
    pub url: String,
    /// Release notes (markdown).
    pub notes: String,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

/// A downloadable file attached to a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Asset {
    pub name: String,
    /// `browser_download_url`.
    pub url: String,
    pub size: u64,
}

impl Release {
    /// Tag without the conventional `v` prefix (`v5.2.1` → `5.2.1`).
    pub fn version(&self) -> &str {
        self.tag.strip_prefix('v').unwrap_or(&self.tag)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    fetched_at: i64,
    /// `None`: the repository has no published releases (HTTP 404).
    release: Option<Release>,
}

#[derive(Deserialize)]
struct ApiRelease {
    tag_name: String,
    name: Option<String>,
    published_at: Option<String>,
    html_url: String,
    body: Option<String>,
    #[serde(default)]
    assets: Vec<ApiAsset>,
}

#[derive(Deserialize)]
struct ApiAsset {
    name: String,
    browser_download_url: String,
    #[serde(default)]
    size: u64,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

/// Result of a lookup: the release (`None` when the repository has none) plus a
/// warning when only stale cached data could be returned because the network
/// request failed.
pub struct Lookup {
    pub release: Option<Release>,
    pub fetched_at: i64,
    pub warning: Option<String>,
}

fn cache_path(repo: &str) -> PathBuf {
    store::state_dir()
        .join("github")
        .join(format!("{}.json", repo.replace('/', "_")))
}

fn read_cache(repo: &str) -> Option<CacheEntry> {
    let text = std::fs::read_to_string(cache_path(repo)).ok()?;
    serde_json::from_str(&text).ok()
}

/// Latest release of `owner/repo`, served from cache when younger than one hour.
/// Blocking (spawns curl).
pub fn latest_release(repo: &str, now: i64) -> Result<Lookup> {
    let cached = read_cache(repo);
    if let Some(c) = &cached
        && now - c.fetched_at < CACHE_TTL_SECS
        && now >= c.fetched_at
    {
        return Ok(Lookup {
            release: c.release.clone(),
            fetched_at: c.fetched_at,
            warning: None,
        });
    }
    match fetch(repo, now) {
        Ok(release) => {
            let entry = CacheEntry {
                fetched_at: now,
                release,
            };
            let json = serde_json::to_vec_pretty(&entry).context("serializing cache")?;
            if let Err(e) = store::write_atomic(&cache_path(repo), &json) {
                tracing::warn!("caching GitHub release for {repo}: {e:#}");
            }
            Ok(Lookup {
                release: entry.release,
                fetched_at: now,
                warning: None,
            })
        }
        Err(err) => match cached {
            Some(c) => Ok(Lookup {
                warning: Some(format!(
                    "showing data cached {} ({err:#})",
                    crate::ago(now - c.fetched_at)
                )),
                release: c.release,
                fetched_at: c.fetched_at,
            }),
            None => Err(err),
        },
    }
}

/// `Ok(None)` when GitHub answers 404 (no releases, or no such repository).
fn fetch(repo: &str, now: i64) -> Result<Option<Release>> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let out = cmd::output(
        "curl",
        [
            "-sS",
            "-i",
            "--max-time",
            "20",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            "X-GitHub-Api-Version: 2022-11-28",
            "-A",
            "hyprdeck",
            url.as_str(),
        ],
    )?;
    if !out.ok() {
        bail!("GitHub request failed: {}", out.stderr.trim());
    }
    let resp = parse::parse_http_response(&out.stdout)
        .ok_or_else(|| anyhow!("malformed HTTP response from GitHub"))?;
    match resp.status {
        200 => {}
        403 | 429 if resp.header("x-ratelimit-remaining") == Some("0") || resp.status == 429 => {
            let reset = resp
                .header("x-ratelimit-reset")
                .and_then(|v| v.parse::<i64>().ok());
            return Err(match reset {
                Some(t) if t > now => anyhow!(
                    "GitHub API rate limit reached; resets in {}",
                    crate::duration(t - now)
                ),
                _ => anyhow!("GitHub API rate limit reached"),
            });
        }
        404 => return Ok(None),
        status => {
            let msg = serde_json::from_str::<ApiError>(resp.body)
                .map(|e| e.message)
                .unwrap_or_default();
            bail!("GitHub returned HTTP {status} {msg}");
        }
    }
    let api: ApiRelease =
        serde_json::from_str(resp.body).context("unexpected GitHub release JSON")?;
    Ok(Some(Release {
        name: api
            .name
            .filter(|n| !n.trim().is_empty())
            .unwrap_or_else(|| api.tag_name.clone()),
        published_at: api
            .published_at
            .as_deref()
            .and_then(parse::parse_iso8601)
            .unwrap_or(0),
        tag: api.tag_name,
        url: api.html_url,
        notes: api.body.unwrap_or_default().replace("\r\n", "\n"),
        assets: api
            .assets
            .into_iter()
            .map(|a| Asset {
                name: a.name,
                url: a.browser_download_url,
                size: a.size,
            })
            .collect(),
    }))
}
