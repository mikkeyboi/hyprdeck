//! AUR RPC v5 `info` lookups: pending AUR updates of installed foreign packages
//! (`pacman -Qm` + RPC + vercmp, no AUR helper needed) and which package names
//! exist in the AUR (cached in the state dir for a week).

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use hyprdeck_core::{cmd, store};
use serde::{Deserialize, Serialize};

use crate::parse::{self, Update};
use crate::scan;
use crate::vercmp::vercmp;

const CACHE_TTL_SECS: i64 = 7 * 86_400;
const RPC_INFO: &str = "https://aur.archlinux.org/rpc/v5/info?";
/// Keep request URLs well below the AUR's URI length limit.
const MAX_URL_LEN: usize = 4000;

/// VCS packages (built from a branch head): their AUR version only changes
/// when the maintainer bumps it, so new upstream commits are not detected.
pub const VCS_NOTE: &str = "-git/-hg/-svn/-bzr packages only show an update when their AUR version is bumped; new upstream commits are not detected";

/// One package as reported by the AUR RPC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AurInfo {
    pub name: String,
    pub pkgbase: String,
    pub version: String,
    /// `None`: orphaned.
    pub maintainer: Option<String>,
    pub votes: u32,
    pub popularity: f64,
    /// Unix seconds when flagged out of date.
    pub out_of_date: Option<i64>,
    pub first_submitted: i64,
    pub last_modified: i64,
    /// Upstream project URL.
    pub url: Option<String>,
}

impl AurInfo {
    pub fn meta(&self) -> scan::Meta {
        scan::Meta {
            maintainer: self.maintainer.clone(),
            votes: self.votes,
            popularity: self.popularity,
            out_of_date: self.out_of_date,
            first_submitted: self.first_submitted,
        }
    }

    pub fn page_url(&self) -> String {
        format!("https://aur.archlinux.org/packages/{}", self.name)
    }
}

/// An installed foreign package and its AUR status.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Foreign {
    pub name: String,
    pub version: String,
}

/// Result of comparing installed foreign packages with the AUR.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Listing {
    pub updates: Vec<Update>,
    /// RPC data of each updated package, same order as `updates`.
    pub info: Vec<AurInfo>,
    /// Foreign packages the AUR doesn't know (local builds, removed packages).
    pub not_in_aur: Vec<Foreign>,
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
#[serde(rename_all = "PascalCase")]
struct RpcPackage {
    name: String,
    package_base: String,
    version: String,
    #[serde(default)]
    maintainer: Option<String>,
    #[serde(default)]
    num_votes: u32,
    #[serde(default)]
    popularity: f64,
    #[serde(default)]
    out_of_date: Option<i64>,
    #[serde(default)]
    first_submitted: i64,
    #[serde(default)]
    last_modified: i64,
    #[serde(rename = "URL", default)]
    url: Option<String>,
}

impl From<RpcPackage> for AurInfo {
    fn from(p: RpcPackage) -> Self {
        AurInfo {
            name: p.name,
            pkgbase: p.package_base,
            version: p.version,
            maintainer: p.maintainer,
            votes: p.num_votes,
            popularity: p.popularity,
            out_of_date: p.out_of_date,
            first_submitted: p.first_submitted,
            last_modified: p.last_modified,
            url: p.url.filter(|u| !u.is_empty()),
        }
    }
}

/// Whether `name` is a VCS package (`-git`, `-hg`, `-svn`, `-bzr`).
pub fn is_vcs(name: &str) -> bool {
    ["-git", "-hg", "-svn", "-bzr"]
        .iter()
        .any(|s| name.ends_with(s))
}

/// Compare installed foreign packages with their AUR entries. A package is
/// updatable when the AUR version is newer (VCS packages included: they only
/// update when the AUR version was bumped).
pub fn classify(foreign: &[(String, String)], info: &[AurInfo]) -> Listing {
    let by_name: BTreeMap<&str, &AurInfo> = info.iter().map(|i| (i.name.as_str(), i)).collect();
    let mut out = Listing::default();
    for (name, installed) in foreign {
        match by_name.get(name.as_str()) {
            None => out.not_in_aur.push(Foreign {
                name: name.clone(),
                version: installed.clone(),
            }),
            Some(i) if vercmp(&i.version, installed).is_gt() => {
                out.updates.push(Update {
                    name: name.clone(),
                    old: installed.clone(),
                    new: i.version.clone(),
                    important: parse::is_important(name),
                });
                out.info.push((*i).clone());
            }
            Some(_) => {}
        }
    }
    out
}

/// Installed foreign packages (`pacman -Qm`) and their pending AUR updates. Blocking.
pub fn list_updates() -> Result<Listing> {
    let out = cmd::output("pacman", ["-Qm"])?;
    // Exit 1 without output: no foreign packages installed.
    if !out.ok() && !(out.status == 1 && out.stderr.trim().is_empty()) {
        bail!(crate::check::failure("pacman -Qm", out.status, &out.stderr));
    }
    let foreign = parse::parse_query(&out.stdout);
    let names: Vec<&str> = foreign.iter().map(|(n, _)| n.as_str()).collect();
    let info = info(&names)?;
    Ok(classify(&foreign, &info))
}

/// RPC `info` for `names`, in as few requests as the URL length allows. Blocking.
pub fn info(names: &[&str]) -> Result<Vec<AurInfo>> {
    let mut all = Vec::new();
    for url in rpc_urls(names) {
        all.extend(request(&url)?);
    }
    Ok(all)
}

fn request(url: &str) -> Result<Vec<AurInfo>> {
    // `-g`: the `arg[]` brackets must not be treated as curl URL globs.
    let out = cmd::output(
        "curl",
        ["-sS", "-f", "-g", "--max-time", "20", "-A", "hyprdeck", url],
    )?;
    if !out.ok() {
        bail!("AUR request failed: {}", out.stderr.trim());
    }
    parse_response(&out.stdout)
}

fn parse_response(json: &str) -> Result<Vec<AurInfo>> {
    let resp: RpcResponse = serde_json::from_str(json).context("unexpected AUR RPC JSON")?;
    if resp.kind == "error" {
        bail!("AUR RPC error: {}", resp.error.unwrap_or_default());
    }
    Ok(resp.results.into_iter().map(AurInfo::from).collect())
}

/// Request URLs covering all `names`, each shorter than [`MAX_URL_LEN`].
fn rpc_urls(names: &[&str]) -> Vec<String> {
    let mut urls = Vec::new();
    let mut url = String::from(RPC_INFO);
    for n in names {
        // Package names are [a-z0-9@._+-]; only '+' needs escaping in a query.
        let arg = format!("arg[]={}", n.replace('+', "%2B"));
        if url.len() > RPC_INFO.len() && url.len() + 1 + arg.len() > MAX_URL_LEN {
            urls.push(std::mem::replace(&mut url, String::from(RPC_INFO)));
        }
        if url.len() > RPC_INFO.len() {
            url.push('&');
        }
        url.push_str(&arg);
    }
    if url.len() > RPC_INFO.len() {
        urls.push(url);
    }
    urls
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Cache {
    /// Package name → (exists, checked at).
    packages: BTreeMap<String, (bool, i64)>,
}

fn cache_path() -> PathBuf {
    store::state_dir().join("aur-packages.json")
}

/// The subset of `names` that exist in the AUR. Fresh cache entries are used
/// as-is; stale or missing ones are looked up. When the lookup fails, stale
/// entries are still trusted and unknown names count as absent. Blocking.
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
        match info(&missing) {
            Ok(found) => {
                let found: HashSet<&str> = found.iter().map(|i| i.name.as_str()).collect();
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

#[cfg(test)]
mod tests {
    use super::*;

    const RESPONSE: &str = r#"{"resultcount":3,"results":[
        {"Name":"example-app-bin","PackageBase":"example-app-bin","Version":"3.5.0-1","Maintainer":"alice","NumVotes":40,"Popularity":1.5,"OutOfDate":null,"FirstSubmitted":1700000000,"LastModified":1790000000,"URL":"https://github.com/example/app","Submitter":"alice"},
        {"Name":"libfoo-git","PackageBase":"foo-git","Version":"2.0.r10.gabc-1","Maintainer":null,"NumVotes":2,"Popularity":0.03,"OutOfDate":1780000000,"FirstSubmitted":1600000000,"LastModified":1740000000,"URL":null},
        {"Name":"tool","PackageBase":"tool","Version":"1:4.0.2-3","Maintainer":"bob","NumVotes":900,"Popularity":3.25,"OutOfDate":null,"FirstSubmitted":1500000000,"LastModified":1750000000,"URL":""}
    ],"type":"multiinfo","version":5}"#;

    #[test]
    fn rpc_response_parses_metadata() {
        let info = parse_response(RESPONSE).unwrap();
        assert_eq!(info.len(), 3);
        assert_eq!(info[0].maintainer.as_deref(), Some("alice"));
        assert_eq!(
            info[0].url.as_deref(),
            Some("https://github.com/example/app")
        );
        assert_eq!(info[1].pkgbase, "foo-git");
        assert_eq!(info[1].maintainer, None);
        assert_eq!(info[1].out_of_date, Some(1780000000));
        assert_eq!(info[2].url, None);
        assert_eq!(info[2].votes, 900);
        assert!(
            parse_response(r#"{"type":"error","error":"Too many package results.","results":[]}"#)
                .is_err()
        );
    }

    #[test]
    fn classify_by_vercmp() {
        let info = parse_response(RESPONSE).unwrap();
        let installed = [
            ("example-app-bin", "3.4.1-1"),
            ("libfoo-git", "2.0.r12.gdef-1"),
            ("tool", "4.1.0-1"),
            ("local-only", "0.1-1"),
        ]
        .map(|(n, v)| (n.to_owned(), v.to_owned()));
        let l = classify(&installed, &info);
        // The VCS package is ahead of its AUR version (built from a newer commit).
        // The epoch makes the AUR "tool" newer than the installed 4.1.0.
        assert_eq!(
            l.updates
                .iter()
                .map(|u| (u.name.as_str(), u.old.as_str(), u.new.as_str()))
                .collect::<Vec<_>>(),
            [
                ("example-app-bin", "3.4.1-1", "3.5.0-1"),
                ("tool", "4.1.0-1", "1:4.0.2-3")
            ]
        );
        assert_eq!(l.info[1].pkgbase, "tool");
        assert_eq!(
            l.not_in_aur,
            [Foreign {
                name: "local-only".into(),
                version: "0.1-1".into()
            }]
        );
        let bumped = [("libfoo-git".to_owned(), "2.0.r9.g123-1".to_owned())];
        assert_eq!(classify(&bumped, &info).updates[0].new, "2.0.r10.gabc-1");
    }

    #[test]
    fn vcs_names() {
        for n in ["foo-git", "bar-hg", "baz-svn", "qux-bzr"] {
            assert!(is_vcs(n), "{n}");
        }
        for n in ["git", "foo-bin", "gitg", "foo-github"] {
            assert!(!is_vcs(n), "{n}");
        }
    }

    #[test]
    fn rpc_urls_batch_and_escape() {
        assert_eq!(
            rpc_urls(&["foo-git", "libc++-git"]),
            ["https://aur.archlinux.org/rpc/v5/info?arg[]=foo-git&arg[]=libc%2B%2B-git"]
        );
        assert!(rpc_urls(&[]).is_empty());
        let names: Vec<String> = (0..500).map(|i| format!("package-number-{i}")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let urls = rpc_urls(&refs);
        assert!(urls.len() > 1);
        assert!(urls.iter().all(|u| u.len() <= MAX_URL_LEN));
        assert_eq!(
            urls.iter()
                .map(|u| u.matches("arg[]=").count())
                .sum::<usize>(),
            500
        );
    }
}
