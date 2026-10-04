//! Local clones of AUR package repositories (`~/.cache/hyprdeck/aur/<pkgbase>`),
//! the revision the user last approved per package base, and `.SRCINFO` parsing.
//! Nothing here executes code from a repository.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use hyprdeck_core::store;
use serde::{Deserialize, Serialize};

use crate::scan::{self, RepoFile};

/// Files larger than this are listed, not shown.
const MAX_SHOWN_FILE: usize = 256 * 1024;

/// Where AUR repositories are cloned and built.
pub fn cache_dir() -> PathBuf {
    dirs::cache_dir()
        .unwrap_or_else(|| store::home().join(".cache"))
        .join("hyprdeck")
        .join("aur")
}

pub fn repo_dir(pkgbase: &str) -> PathBuf {
    cache_dir().join(pkgbase)
}

/// Package (base) names: `[a-z0-9@._+-]+`, not starting with `-` or `.`.
pub fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 256
        && !name.starts_with(['-', '.'])
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"@._+-".contains(&b))
}

/// What the user approved for a package base (recorded after a successful install).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Approval {
    pub commit: String,
    pub maintainer: Option<String>,
    pub source_hosts: Vec<String>,
    pub approved_at: i64,
    /// Warning/Critical findings the user accepted with this revision.
    #[serde(default)]
    pub findings: Vec<KnownFinding>,
}

impl Approval {
    pub fn previous(&self) -> scan::Previous {
        scan::Previous {
            maintainer: self.maintainer.clone(),
            source_hosts: self.source_hosts.clone(),
        }
    }
}

/// A finding identified by its rule and normalized line text, so it is
/// recognised again when unrelated lines move.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KnownFinding {
    pub rule: String,
    pub text: String,
}

impl KnownFinding {
    pub fn of(f: &scan::Finding) -> Self {
        KnownFinding {
            rule: f.rule.clone(),
            text: f.excerpt.split_whitespace().collect::<Vec<_>>().join(" "),
        }
    }
}

fn approvals_path() -> PathBuf {
    store::state_dir().join("aur-approved.json")
}

/// Approved revisions by package base. Blocking.
pub fn approvals() -> BTreeMap<String, Approval> {
    std::fs::read_to_string(approvals_path())
        .ok()
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_default()
}

/// Record `approval` for `pkgbase`. Blocking.
pub fn approve(pkgbase: &str, approval: Approval) -> Result<()> {
    let mut all = approvals();
    all.insert(pkgbase.to_owned(), approval);
    store::write_atomic(&approvals_path(), &serde_json::to_vec_pretty(&all)?)
}

/// Run git without hooks, prompts, pagers or external diff/textconv drivers.
fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args([
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.fsmonitor=false",
        ])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .context("failed to run git")?;
    if !out.status.success() {
        bail!(
            "git {} failed: {}",
            args.first().unwrap_or(&""),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Clone the AUR repository of `pkgbase`, or fetch it and reset to the remote
/// head. Returns the head commit. Blocking (network).
pub fn sync(pkgbase: &str) -> Result<String> {
    if !valid_name(pkgbase) {
        bail!("invalid package base {pkgbase:?}");
    }
    let dir = repo_dir(pkgbase);
    let url = format!("https://aur.archlinux.org/{pkgbase}.git");
    if dir.join(".git").is_dir() {
        git(&dir, &["fetch", "--quiet", &url, "master"])?;
        git(&dir, &["reset", "--quiet", "--hard", "FETCH_HEAD"])?;
    } else {
        if dir.exists() {
            std::fs::remove_dir_all(&dir)
                .with_context(|| format!("removing stale {}", dir.display()))?;
        }
        let parent = dir.parent().context("cache dir has no parent")?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
        git(
            parent,
            &[
                "clone",
                "--quiet",
                &url,
                dir.to_str().context("non-UTF-8 cache path")?,
            ],
        )?;
    }
    head(&dir).with_context(|| format!("{pkgbase} has no commits in the AUR"))
}

pub fn head(dir: &Path) -> Result<String> {
    Ok(git(dir, &["rev-parse", "--verify", "HEAD^{commit}"])?
        .trim()
        .to_owned())
}

/// What the review shows for a package base.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Baseline {
    /// Never approved: the full files.
    First,
    /// Unified diff since the approved commit.
    Since { commit: String, approved_at: i64 },
    /// Head is the approved commit.
    Unchanged { commit: String },
    /// The approved commit is gone from the history (rewritten): full files.
    Missing { commit: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Changes {
    pub baseline: Baseline,
    /// Unified diff (`Since`) or the files with `==> path <==` headers.
    pub text: String,
}

impl Changes {
    pub fn is_diff(&self) -> bool {
        matches!(self.baseline, Baseline::Since { .. })
    }

    /// Lines added and removed (diffs only).
    pub fn stat(&self) -> (usize, usize) {
        if !self.is_diff() {
            return (0, 0);
        }
        self.text.lines().fold((0, 0), |(a, r), l| {
            if l.starts_with("+++") || l.starts_with("---") {
                (a, r)
            } else if l.starts_with('+') {
                (a + 1, r)
            } else if l.starts_with('-') {
                (a, r + 1)
            } else {
                (a, r)
            }
        })
    }
}

/// The review text for `head` relative to the user's last approval. Blocking.
pub fn changes(dir: &Path, head: &str, approved: Option<&Approval>) -> Result<Changes> {
    let Some(a) = approved else {
        return Ok(Changes {
            baseline: Baseline::First,
            text: full_text(dir, head)?,
        });
    };
    if a.commit == head {
        return Ok(Changes {
            baseline: Baseline::Unchanged {
                commit: a.commit.clone(),
            },
            text: String::new(),
        });
    }
    let known = git(
        dir,
        &["cat-file", "-e", &format!("{}^{{commit}}", a.commit)],
    )
    .is_ok();
    if !known {
        return Ok(Changes {
            baseline: Baseline::Missing {
                commit: a.commit.clone(),
            },
            text: full_text(dir, head)?,
        });
    }
    let text = git(
        dir,
        &[
            "diff",
            "--no-color",
            "--no-ext-diff",
            "--no-textconv",
            &a.commit,
            head,
            "--",
            ".",
            ":(exclude).SRCINFO",
        ],
    )?;
    Ok(Changes {
        baseline: Baseline::Since {
            commit: a.commit.clone(),
            approved_at: a.approved_at,
        },
        text,
    })
}

/// Tracked files at `rev`, read from git (not the work tree).
pub fn files(dir: &Path, rev: &str) -> Result<Vec<(String, Vec<u8>)>> {
    let list = git(dir, &["ls-tree", "-r", "-z", "--name-only", rev])?;
    list.split('\0')
        .filter(|p| !p.is_empty())
        .map(|path| {
            let out = Command::new("git")
                .arg("-C")
                .arg(dir)
                .args(["cat-file", "blob", &format!("{rev}:{path}")])
                .stdin(Stdio::null())
                .output()
                .context("failed to run git")?;
            if !out.status.success() {
                bail!(
                    "reading {path}: {}",
                    String::from_utf8_lossy(&out.stderr).trim()
                );
            }
            Ok((path.to_owned(), out.stdout))
        })
        .collect()
}

fn is_text(bytes: &[u8]) -> bool {
    !bytes.contains(&0) && std::str::from_utf8(bytes).is_ok()
}

/// Text files at `rev` for the security scanner.
pub fn scan_files(dir: &Path, rev: &str) -> Result<Vec<RepoFile>> {
    Ok(files(dir, rev)?
        .into_iter()
        .filter(|(_, b)| is_text(b) && b.len() <= MAX_SHOWN_FILE)
        .map(|(path, bytes)| RepoFile {
            path,
            text: String::from_utf8(bytes).unwrap_or_default(),
        })
        .collect())
}

/// All files except `.SRCINFO` (generated from the PKGBUILD), PKGBUILD first.
fn full_text(dir: &Path, rev: &str) -> Result<String> {
    let mut files = files(dir, rev)?;
    files.retain(|(p, _)| p != ".SRCINFO");
    files.sort_by_key(|(p, _)| (p != "PKGBUILD", p.clone()));
    let mut out = String::new();
    for (path, bytes) in files {
        out.push_str(&format!("==> {path} <==\n"));
        if is_text(&bytes) && bytes.len() <= MAX_SHOWN_FILE {
            out.push_str(&String::from_utf8_lossy(&bytes));
            if !bytes.ends_with(b"\n") {
                out.push('\n');
            }
        } else {
            out.push_str(&format!("(binary or large file, {} bytes)\n", bytes.len()));
        }
        out.push('\n');
    }
    Ok(out)
}

/// Parsed `.SRCINFO` / `makepkg --printsrcinfo` output.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SrcInfo {
    pub pkgbase: String,
    base: Vec<(String, String)>,
    /// `pkgname` sections in order.
    packages: Vec<(String, Vec<(String, String)>)>,
}

pub fn parse_srcinfo(text: &str) -> SrcInfo {
    let mut info = SrcInfo::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim().to_owned(), value.trim().to_owned());
        match key.as_str() {
            "pkgbase" => info.pkgbase = value,
            "pkgname" => info.packages.push((value, Vec::new())),
            _ => match info.packages.last_mut() {
                Some((_, kv)) => kv.push((key, value)),
                None => info.base.push((key, value)),
            },
        }
    }
    info
}

impl SrcInfo {
    fn base_values<'a>(&'a self, key: &str) -> Vec<&'a str> {
        self.base
            .iter()
            .filter(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .collect()
    }

    fn base_value(&self, key: &str) -> Option<&str> {
        self.base
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }

    /// `[epoch:]pkgver-pkgrel`.
    pub fn version(&self) -> Option<String> {
        let ver = self.base_value("pkgver")?;
        let rel = self.base_value("pkgrel")?;
        Some(
            match self
                .base_value("epoch")
                .filter(|e| !e.is_empty() && *e != "0")
            {
                Some(e) => format!("{e}:{ver}-{rel}"),
                None => format!("{ver}-{rel}"),
            },
        )
    }

    /// Values of `key` (and `key_<arch>`) for package `name`: a package section
    /// that sets the key (even to empty) overrides the pkgbase value.
    fn package_values<'a>(&'a self, name: &str, key: &str, arch: &str) -> Vec<&'a str> {
        let arch_key = format!("{key}_{arch}");
        let section = self
            .packages
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, kv)| kv.as_slice())
            .unwrap_or_default();
        let mut out = Vec::new();
        for k in [key, arch_key.as_str()] {
            let own: Vec<&str> = section
                .iter()
                .filter(|(sk, _)| sk == k)
                .map(|(_, v)| v.as_str())
                .collect();
            if own.is_empty() {
                out.extend(self.base_values(k));
            } else {
                out.extend(own);
            }
        }
        out.retain(|v| !v.is_empty());
        out
    }

    /// Everything needed to build and install `names` on `arch`: their
    /// `depends`, plus the base's `makedepends` and `checkdepends`. Deduplicated,
    /// version constraints kept (`foo>=1.2`).
    pub fn build_deps(&self, names: &[String], arch: &str) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut push = |d: &str| {
            if !out.iter().any(|o| o == d) {
                out.push(d.to_owned());
            }
        };
        for n in names {
            for d in self.package_values(n, "depends", arch) {
                push(d);
            }
        }
        for key in ["makedepends", "checkdepends"] {
            let arch_key = format!("{key}_{arch}");
            for d in [self.base_values(key), self.base_values(&arch_key)].concat() {
                if !d.is_empty() {
                    push(d);
                }
            }
        }
        out
    }
}

/// `foo>=1.2` → `foo`.
pub fn dep_name(dep: &str) -> &str {
    dep.split(['<', '>', '=']).next().unwrap_or(dep).trim()
}

/// Which of `pkgnames` a built package file belongs to
/// (`<dir>/<pkgname>-<version>-<arch>.pkg.tar.*`).
pub fn package_of<'a>(file: &str, pkgnames: &[&'a str], version: &str) -> Option<&'a str> {
    let base = file.rsplit('/').next().unwrap_or(file);
    pkgnames
        .iter()
        .copied()
        .filter(|n| {
            base.strip_prefix(n)
                .and_then(|r| r.strip_prefix('-'))
                .and_then(|r| r.strip_prefix(version))
                .is_some_and(|r| r.starts_with('-'))
        })
        // `foo` and `foo-utils` may both match `foo-utils-1-1-…` only for the longer name.
        .max_by_key(|n| n.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRCINFO: &str = "\
pkgbase = example
\tpkgdesc = Example split package
\tpkgver = 1.2.3
\tpkgrel = 2
\tepoch = 1
\tarch = x86_64
\tmakedepends = cmake
\tmakedepends = ninja>=1.11
\tcheckdepends = python-pytest
\tdepends = glibc
\tdepends = zlib
\tdepends_x86_64 = lib32-glibc
\tsource = https://example.org/example-1.2.3.tar.gz

pkgname = example
\tdepends = glibc
\tdepends = openssl>=3

pkgname = example-docs
\tdepends =
\tarch = any

pkgname = example-utils
";

    #[test]
    fn srcinfo_deps_follow_overrides() {
        let s = parse_srcinfo(SRCINFO);
        assert_eq!(s.pkgbase, "example");
        assert_eq!(
            s.packages
                .iter()
                .map(|(n, _)| n.as_str())
                .collect::<Vec<_>>(),
            ["example", "example-docs", "example-utils"]
        );
        assert_eq!(s.version().as_deref(), Some("1:1.2.3-2"));
        let deps = s.build_deps(&["example".into()], "x86_64");
        assert_eq!(
            deps,
            [
                "glibc",
                "openssl>=3",
                "lib32-glibc",
                "cmake",
                "ninja>=1.11",
                "python-pytest"
            ]
        );
        // An empty override clears the base depends; no arch-specific override.
        assert_eq!(
            s.build_deps(&["example-docs".into()], "aarch64"),
            ["cmake", "ninja>=1.11", "python-pytest"]
        );
        // No override: inherits the base.
        assert_eq!(
            s.build_deps(&["example-utils".into()], "aarch64")[..2],
            ["glibc", "zlib"]
        );
    }

    #[test]
    fn dep_names_and_package_files() {
        assert_eq!(dep_name("openssl>=3"), "openssl");
        assert_eq!(dep_name("foo=1-1"), "foo");
        assert_eq!(dep_name("libc++<20"), "libc++");
        assert_eq!(dep_name("bar"), "bar");
        let names = ["example", "example-utils", "example-debug"];
        let v = "1:1.2.3-2";
        assert_eq!(
            package_of("/c/example/example-1:1.2.3-2-x86_64.pkg.tar.zst", &names, v),
            Some("example")
        );
        assert_eq!(
            package_of(
                "/c/example/example-utils-1:1.2.3-2-x86_64.pkg.tar.zst",
                &names,
                v
            ),
            Some("example-utils")
        );
        assert_eq!(
            package_of("/c/example/other-1:1.2.3-2-x86_64.pkg.tar.zst", &names, v),
            None
        );
    }

    #[test]
    fn names() {
        for n in ["foo", "libc++", "foo-git", "python3.12", "r@x_y"] {
            assert!(valid_name(n), "{n}");
        }
        for n in ["", "-foo", ".foo", "Foo", "a/b", "a b", "..", "a;b", "$(x)"] {
            assert!(!valid_name(n), "{n}");
        }
    }

    fn sh(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap()
            .status
            .success();
        assert!(ok, "git {args:?}");
    }

    #[test]
    fn changes_against_approval() {
        let dir = std::env::temp_dir().join(format!("hd-pkgbuild-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        sh(&dir, &["init", "-q"]);
        std::fs::write(dir.join("PKGBUILD"), "pkgname=x\npkgver=1\n").unwrap();
        std::fs::write(dir.join(".SRCINFO"), "pkgbase = x\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "one"]);
        let first = head(&dir).unwrap();
        std::fs::write(dir.join("PKGBUILD"), "pkgname=x\npkgver=2\n").unwrap();
        std::fs::write(dir.join("x.install"), "post_install() { :; }\n").unwrap();
        sh(&dir, &["add", "."]);
        sh(&dir, &["commit", "-qm", "two"]);
        let second = head(&dir).unwrap();

        let c = changes(&dir, &second, None).unwrap();
        assert_eq!(c.baseline, Baseline::First);
        assert!(
            c.text
                .starts_with("==> PKGBUILD <==\npkgname=x\npkgver=2\n")
        );
        assert!(c.text.contains("==> x.install <=="));
        assert!(!c.text.contains("SRCINFO"));

        let approval = |commit: &str| Approval {
            commit: commit.into(),
            maintainer: Some("m".into()),
            source_hosts: Vec::new(),
            approved_at: 5,
            findings: Vec::new(),
        };
        let c = changes(&dir, &second, Some(&approval(&first))).unwrap();
        assert!(c.is_diff());
        assert!(c.text.contains("-pkgver=1\n+pkgver=2\n"), "{}", c.text);
        assert_eq!(c.stat(), (2, 1));
        let c = changes(&dir, &second, Some(&approval(&second))).unwrap();
        assert!(matches!(c.baseline, Baseline::Unchanged { .. }));
        let gone = "0123456789abcdef0123456789abcdef01234567";
        let c = changes(&dir, &second, Some(&approval(gone))).unwrap();
        assert!(matches!(c.baseline, Baseline::Missing { .. }));
        assert!(c.text.contains("==> PKGBUILD <=="));

        let scanned = scan_files(&dir, &second).unwrap();
        assert_eq!(
            scanned.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            [".SRCINFO", "PKGBUILD", "x.install"]
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
