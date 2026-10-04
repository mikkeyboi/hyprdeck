//! Stamp the build with its identity:
//! - `HYPRDECK_COMMIT`: `git rev-parse HEAD`, else CI's `GITHUB_SHA` (no checkout).
//! - `HYPRDECK_VERSION`: release version. Releases are cut by tagging (`vX.Y.Z`),
//!   so it comes from `$HYPRDECK_VERSION` (release CI), else the nearest
//!   `v*` tag (`git describe`), else the crate version.
//! - `HYPRDECK_DISTANCE`: commits since that tag (0 for a tagged release build).

use std::path::Path;
use std::process::Command;

fn git(root: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    println!("cargo:rerun-if-env-changed=HYPRDECK_VERSION");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git_dir = root.join(".git");
    if git_dir.is_dir() {
        for p in ["HEAD", "refs/heads", "refs/tags", "packed-refs"] {
            println!("cargo:rerun-if-changed={}", git_dir.join(p).display());
        }
    }

    let commit = git(&root, &["rev-parse", "HEAD"])
        .or_else(|| std::env::var("GITHUB_SHA").ok().filter(|s| !s.is_empty()));
    if let Some(commit) =
        commit.filter(|c| c.len() >= 7 && c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        println!("cargo:rustc-env=HYPRDECK_COMMIT={commit}");
    }

    let explicit = std::env::var("HYPRDECK_VERSION")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let described = git(
        &root,
        &["describe", "--tags", "--match", "v[0-9]*", "--long"],
    );
    let (version, distance) = match (explicit, described.as_deref().and_then(parse_describe)) {
        (Some(v), _) => (v.trim().trim_start_matches('v').to_owned(), 0),
        (None, Some((v, d))) => (v, d),
        (None, None) => (env!("CARGO_PKG_VERSION").to_owned(), 0),
    };
    println!("cargo:rustc-env=HYPRDECK_VERSION={version}");
    println!("cargo:rustc-env=HYPRDECK_DISTANCE={distance}");
}

/// `v0.1.2-5-gabc1234` → (`0.1.2`, 5).
fn parse_describe(s: &str) -> Option<(String, u32)> {
    let mut parts = s.rsplitn(3, '-');
    let _hash = parts.next()?;
    let distance = parts.next()?.parse().ok()?;
    let tag = parts.next()?;
    Some((tag.trim_start_matches('v').to_owned(), distance))
}
