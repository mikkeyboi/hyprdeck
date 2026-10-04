//! GitHub release lookups (unauthenticated REST API) with an on-disk cache in
//! the state dir so the 60 requests/hour anonymous limit is never a problem.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use hyprdeck_core::{cmd, store};
use serde::de::DeserializeOwned;
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

/// A release published under a fixed tag plus the commit the tag points to
/// (e.g. a rolling `nightly` prerelease).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaggedRelease {
    pub release: Release,
    /// Full commit hash.
    pub commit: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry<T> {
    fetched_at: i64,
    /// `None` inside `T`: the release does not exist (HTTP 404).
    release: T,
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
struct ApiCommit {
    sha: String,
}

#[derive(Deserialize)]
struct ApiError {
    message: String,
}

/// Result of a lookup: the release (`None` when it does not exist) plus a
/// warning when only stale cached data could be returned because the network
/// request failed.
pub struct Lookup<T = Option<Release>> {
    pub release: T,
    pub fetched_at: i64,
    pub warning: Option<String>,
}

fn cache_path(key: &str) -> PathBuf {
    store::state_dir()
        .join("github")
        .join(format!("{}.json", key.replace('/', "_")))
}

/// Latest release of `owner/repo`, served from cache when younger than one hour.
/// Blocking (spawns curl).
pub fn latest_release(repo: &str, now: i64) -> Result<Lookup> {
    latest_release_within(repo, now, CACHE_TTL_SECS)
}

/// Like [`latest_release`] with a custom cache age (`0` forces a request).
pub fn latest_release_within(repo: &str, now: i64, max_age: i64) -> Result<Lookup> {
    cached(&cache_path(repo), now, max_age, || {
        api_get(&format!("repos/{repo}/releases/latest"), now)?
            .map(|body| parse_release(&body))
            .transpose()
    })
}

/// The release published under `tag` and the commit the tag points to;
/// `None` when no such release exists. Cached for `max_age` seconds.
pub fn tagged_release(
    repo: &str,
    tag: &str,
    now: i64,
    max_age: i64,
) -> Result<Lookup<Option<TaggedRelease>>> {
    cached(&cache_path(&format!("{repo}@{tag}")), now, max_age, || {
        let Some(body) = api_get(&format!("repos/{repo}/releases/tags/{tag}"), now)? else {
            return Ok(None);
        };
        let release = parse_release(&body)?;
        let commit = api_get(&format!("repos/{repo}/commits/{tag}"), now)?
            .ok_or_else(|| anyhow!("release {tag} exists but its tag has no commit"))?;
        let commit: ApiCommit =
            serde_json::from_str(&commit).context("unexpected GitHub commit JSON")?;
        Ok(Some(TaggedRelease {
            release,
            commit: commit.sha,
        }))
    })
}

/// Serve `path` when younger than `max_age`, otherwise `fetch` and cache the
/// result; on fetch failure fall back to any stale cache with a warning.
fn cached<T>(
    path: &Path,
    now: i64,
    max_age: i64,
    fetch: impl FnOnce() -> Result<T>,
) -> Result<Lookup<T>>
where
    T: Clone + Serialize + DeserializeOwned,
{
    let cached: Option<CacheEntry<T>> = std::fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    if let Some(c) = &cached
        && now - c.fetched_at < max_age
        && now >= c.fetched_at
    {
        return Ok(Lookup {
            release: c.release.clone(),
            fetched_at: c.fetched_at,
            warning: None,
        });
    }
    match fetch() {
        Ok(release) => {
            let entry = CacheEntry {
                fetched_at: now,
                release,
            };
            let json = serde_json::to_vec_pretty(&entry).context("serializing cache")?;
            if let Err(e) = store::write_atomic(path, &json) {
                tracing::warn!("caching GitHub lookup {}: {e:#}", path.display());
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

/// GET `https://api.github.com/<path>`; `Ok(None)` when GitHub answers 404 or
/// 422 (no such release/repository/commit).
fn api_get(path: &str, now: i64) -> Result<Option<String>> {
    let url = format!("https://api.github.com/{path}");
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
        200 => Ok(Some(resp.body.to_owned())),
        403 | 429 if resp.header("x-ratelimit-remaining") == Some("0") || resp.status == 429 => {
            let reset = resp
                .header("x-ratelimit-reset")
                .and_then(|v| v.parse::<i64>().ok());
            Err(match reset {
                Some(t) if t > now => anyhow!(
                    "GitHub API rate limit reached; resets in {}",
                    crate::duration(t - now)
                ),
                _ => anyhow!("GitHub API rate limit reached"),
            })
        }
        404 | 422 => Ok(None),
        status => {
            let msg = serde_json::from_str::<ApiError>(resp.body)
                .map(|e| e.message)
                .unwrap_or_default();
            bail!("GitHub returned HTTP {status} {msg}");
        }
    }
}

fn parse_release(body: &str) -> Result<Release> {
    let api: ApiRelease = serde_json::from_str(body).context("unexpected GitHub release JSON")?;
    Ok(Release {
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
    })
}
