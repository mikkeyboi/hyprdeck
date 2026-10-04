//! Stamp the build with the git commit it was built from (`HYPRDECK_COMMIT`):
//! CI's `GITHUB_SHA` when set, otherwise `git rev-parse HEAD` of the checkout.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=GITHUB_SHA");
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let git_dir = root.join(".git");
    if git_dir.is_dir() {
        println!("cargo:rerun-if-changed={}", git_dir.join("HEAD").display());
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("refs/heads").display()
        );
        println!(
            "cargo:rerun-if-changed={}",
            git_dir.join("packed-refs").display()
        );
    }
    let commit = std::env::var("GITHUB_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            let out = Command::new("git")
                .args(["-C", &root.to_string_lossy(), "rev-parse", "HEAD"])
                .output()
                .ok()?;
            out.status
                .success()
                .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
        });
    if let Some(commit) =
        commit.filter(|c| c.len() >= 7 && c.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        println!("cargo:rustc-env=HYPRDECK_COMMIT={commit}");
    }
}
