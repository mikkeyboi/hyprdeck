//! Which package names exist in the AUR (RPC v5 `info`), cached in the state dir
//! for a week since packages rarely appear or disappear.

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use hyprdeck_core::{cmd, store};
use serde::{Deserialize, Serialize};

const CACHE_TTL_SECS: i64 = 7 * 86_400;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// Package name → (exists, checked at).
    packages: BTreeMap<String, (bool, i64)>,
}

#[derive(Deserialize)]
struct RpcResponse {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    results: Vec<RpcPackage>,
}

#[derive(Deserialize)]
struct RpcPackage {
    #[serde(rename = "Name")]
    name: String,
}

fn cache_path() -> PathBuf {
    store::state_dir().join("aur-packages.json")
}

/// The subset of `names` that exist in the AUR. Fresh cache entries are used
/// as-is; stale or missing ones are looked up in one request. When the lookup
/// fails, stale entries are still trusted and unknown names count as absent.
/// Blocking (spawns curl).
pub fn existing(names: &[&str], now: i64) -> HashSet<String> {
    if names.is_empty() {
        return HashSet::new();
    }
    let mut cache: Cache = std::fs::read_to_string(cache_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default();
    let fresh = |c: &Cache, n: &str| {
        c.packages
            .get(n)
            .is_some_and(|&(_, at)| now >= at && now - at < CACHE_TTL_SECS)
    };
    let missing: Vec<&str> = names
        .iter()
        .copied()
        .filter(|n| !fresh(&cache, n))
        .collect();
    if !missing.is_empty() {
        match lookup(&missing) {
            Ok(found) => {
                for n in missing {
                    cache
                        .packages
                        .insert(n.to_owned(), (found.contains(n), now));
                }
                let saved = serde_json::to_vec_pretty(&cache)
                    .map_err(anyhow::Error::from)
                    .and_then(|json| store::write_atomic(&cache_path(), &json));
                if let Err(e) = saved {
                    tracing::warn!("caching AUR package info: {e:#}");
                }
            }
            Err(e) => tracing::warn!("AUR lookup failed: {e:#}"),
        }
    }
    names
        .iter()
        .filter(|n| cache.packages.get(**n).is_some_and(|&(exists, _)| exists))
        .map(|n| (*n).to_owned())
        .collect()
}

fn lookup(names: &[&str]) -> Result<HashSet<String>> {
    // `-g`: the `arg[]` brackets must not be treated as curl URL globs.
    let out = cmd::output(
        "curl",
        [
            "-sS",
            "-f",
            "-g",
            "--max-time",
            "20",
            "-A",
            "hyprdeck",
            rpc_url(names).as_str(),
        ],
    )?;
    if !out.ok() {
        bail!("AUR request failed: {}", out.stderr.trim());
    }
    let resp: RpcResponse = serde_json::from_str(&out.stdout).context("unexpected AUR RPC JSON")?;
    if resp.kind == "error" {
        bail!("AUR RPC error: {}", resp.error.unwrap_or_default());
    }
    Ok(resp.results.into_iter().map(|p| p.name).collect())
}

fn rpc_url(names: &[&str]) -> String {
    let mut url = String::from("https://aur.archlinux.org/rpc/v5/info?");
    for (i, n) in names.iter().enumerate() {
        if i > 0 {
            url.push('&');
        }
        url.push_str("arg[]=");
        // Package names are [a-z0-9@._+-]; only '+' needs escaping in a query.
        url.push_str(&n.replace('+', "%2B"));
    }
    url
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rpc_url_lists_every_name() {
        assert_eq!(
            rpc_url(&["foo-git", "libc++-git"]),
            "https://aur.archlinux.org/rpc/v5/info?arg[]=foo-git&arg[]=libc%2B%2B-git"
        );
    }

    #[test]
    fn rpc_response_parses() {
        let r: RpcResponse = serde_json::from_str(
            r#"{"resultcount":1,"results":[{"Name":"foo-git","Version":"1.0.r1.gabc-1"}],"type":"multiinfo","version":5}"#,
        )
        .unwrap();
        assert_eq!(r.kind, "multiinfo");
        assert_eq!(r.results[0].name, "foo-git");
    }
}
