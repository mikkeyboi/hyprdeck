//! hyprdeck's own updates. Running from an AppImage (`$APPIMAGE`): compare with
//! the latest GitHub release and replace the AppImage in place. Built from a
//! source checkout: git state, crate updates and `install.sh`. Otherwise just
//! the installed version. Everything here blocks; run it off the GTK thread.

use std::cmp::Ordering;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use hyprdeck_core::cmd;

use crate::check::failure;
use crate::github::{self, Asset};
use crate::parse::{self, CrateUpdate};

/// GitHub repository hyprdeck releases are published to.
pub const REPO: &str = "mikkeyboi/hyprdeck";
pub const RELEASES_URL: &str = "https://github.com/mikkeyboi/hyprdeck/releases";
/// Release asset holding the AppImage, and its optional `sha256sum` file.
pub const APPIMAGE_ASSET: &str = "Hyprdeck-x86_64.AppImage";
pub const CHECKSUM_ASSET: &str = "Hyprdeck-x86_64.AppImage.sha256";
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
const UNIT: &str = "hyprdeck.service";

/// How this hyprdeck was installed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// Running from the AppImage at this path.
    AppImage(PathBuf),
    /// Built from this source checkout (it still exists and has `install.sh`).
    Source(PathBuf),
    /// Anything else (e.g. a distro package).
    Installed,
}

pub fn mode() -> Mode {
    if let Some(path) = std::env::var_os("APPIMAGE").filter(|p| !p.is_empty()) {
        return Mode::AppImage(PathBuf::from(path));
    }
    source_dir().map_or(Mode::Installed, Mode::Source)
}

/// The workspace this binary was built from, if it is still on disk.
fn source_dir() -> Option<PathBuf> {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .find(|d| d.join("install.sh").is_file() && d.join("Cargo.toml").is_file())
        .map(Path::to_path_buf)
}

/// Semver precedence of two versions (leading `v` and `+build` metadata ignored).
pub fn compare_versions(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (Vec<u64>, Option<&str>) {
        let v = v.trim().trim_start_matches(['v', 'V']);
        let v = v.split_once('+').map_or(v, |(v, _)| v);
        let (core, pre) = match v.split_once('-') {
            Some((core, pre)) => (core, Some(pre)),
            None => (v, None),
        };
        (
            core.split('.').map(|n| n.parse().unwrap_or(0)).collect(),
            pre,
        )
    }
    let ((ca, pa), (cb, pb)) = (split(a), split(b));
    let len = ca.len().max(cb.len());
    let part = |c: &[u64], i: usize| c.get(i).copied().unwrap_or(0);
    (0..len)
        .map(|i| part(&ca, i).cmp(&part(&cb, i)))
        .find(|o| o.is_ne())
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            match (pa, pb) {
                (None, None) => Ordering::Equal,
                // A pre-release sorts before its release.
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(pa), Some(pb)) => {
                    let (mut ia, mut ib) = (pa.split('.'), pb.split('.'));
                    loop {
                        match (ia.next(), ib.next()) {
                            (None, None) => break Ordering::Equal,
                            (None, Some(_)) => break Ordering::Less,
                            (Some(_), None) => break Ordering::Greater,
                            (Some(x), Some(y)) => {
                                let o = match (x.parse::<u64>(), y.parse::<u64>()) {
                                    (Ok(x), Ok(y)) => x.cmp(&y),
                                    (Ok(_), Err(_)) => Ordering::Less,
                                    (Err(_), Ok(_)) => Ordering::Greater,
                                    (Err(_), Err(_)) => x.cmp(y),
                                };
                                if o.is_ne() {
                                    break o;
                                }
                            }
                        }
                    }
                }
            }
        })
}

/// A release tag newer than the running version.
pub fn is_newer(tag: &str) -> bool {
    compare_versions(tag, VERSION).is_gt()
}

/// The release files an AppImage update needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateAssets {
    pub appimage: Asset,
    pub checksum: Option<Asset>,
}

/// The AppImage asset (exact name) and its checksum file if published.
pub fn select_assets(assets: &[Asset]) -> Option<UpdateAssets> {
    let find = |name: &str| assets.iter().find(|a| a.name == name).cloned();
    Some(UpdateAssets {
        appimage: find(APPIMAGE_ASSET)?,
        checksum: find(CHECKSUM_ASSET),
    })
}

/// The hash from a `sha256sum` line (`<64 hex>  <file>`), lowercased.
pub fn parse_sha256(text: &str) -> Option<String> {
    let hash = text.split_whitespace().next()?;
    (hash.len() == 64 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
        .then(|| hash.to_ascii_lowercase())
}

/// Latest published release (`None`: none published yet). Cached like the other lookups.
pub fn latest(now: i64) -> Result<github::Lookup> {
    github::latest_release(REPO, now)
}

fn curl_args<'a>(extra: &[&'a str], url: &'a str) -> Vec<&'a str> {
    let mut args = vec!["-fsSL", "--retry", "2", "-A", "hyprdeck"];
    args.extend_from_slice(extra);
    args.push(url);
    args
}

/// Download the release AppImage next to `appimage`, verify its checksum when
/// one is published, make it executable and atomically replace `appimage`.
/// Returns whether the checksum was verified.
pub fn install_appimage(appimage: &Path, assets: &UpdateAssets) -> Result<bool> {
    // Replace the real file even if $APPIMAGE is a symlink to it.
    let target = std::fs::canonicalize(appimage).unwrap_or_else(|_| appimage.to_path_buf());
    let dir = target
        .parent()
        .context("AppImage path has no parent directory")?;
    let tmp = dir.join(format!(".{APPIMAGE_ASSET}.{}.part", std::process::id()));
    let result = download_verified(&tmp, assets).and_then(|verified| {
        let mode = std::fs::metadata(&target).map_or(0o755, |m| m.mode() & 0o7777) | 0o755;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))
            .context("making the download executable")?;
        std::fs::rename(&tmp, &target)
            .with_context(|| format!("replacing {}", target.display()))?;
        Ok(verified)
    });
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn download_verified(tmp: &Path, assets: &UpdateAssets) -> Result<bool> {
    let tmp_str = tmp.to_str().context("non-UTF-8 AppImage path")?;
    let out = cmd::output(
        "curl",
        curl_args(&["--max-time", "1800", "-o", tmp_str], &assets.appimage.url),
    )?;
    if !out.ok() {
        bail!(
            "{}",
            failure("Downloading the AppImage", out.status, &out.stderr)
        );
    }
    if assets.appimage.size > 0 {
        let got = std::fs::metadata(tmp).map(|m| m.len()).unwrap_or(0);
        if got != assets.appimage.size {
            bail!(
                "download incomplete: got {got} bytes, expected {}",
                assets.appimage.size
            );
        }
    }
    let Some(checksum) = &assets.checksum else {
        return Ok(false);
    };
    let out = cmd::output("curl", curl_args(&["--max-time", "60"], &checksum.url))?;
    if !out.ok() {
        bail!(
            "{}",
            failure("Downloading the checksum", out.status, &out.stderr)
        );
    }
    let want = parse_sha256(&out.stdout)
        .ok_or_else(|| anyhow!("{CHECKSUM_ASSET} does not contain a SHA-256 hash"))?;
    let got = cmd::run("sha256sum", [tmp_str])?;
    let got = parse_sha256(&got).ok_or_else(|| anyhow!("unexpected sha256sum output"))?;
    if got != want {
        bail!("checksum mismatch: expected {want}, downloaded file has {got}");
    }
    Ok(true)
}

/// Restart into the updated AppImage: restart the user service when it runs,
/// and re-exec this process unless it is the service itself (then systemd
/// already replaces it). Only returns on failure.
pub fn restart(appimage: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let active =
        cmd::output("systemctl", ["--user", "is-active", "--quiet", UNIT]).is_ok_and(|o| o.ok());
    if active {
        let main_pid = cmd::systemctl_user(["show", "-p", "MainPID", "--value", UNIT])
            .ok()
            .and_then(|s| s.trim().parse::<u32>().ok());
        // --no-block: when we are the service, systemd stops us mid-call.
        cmd::systemctl_user(["--no-block", "restart", UNIT])?;
        if main_pid == Some(std::process::id()) {
            return Ok(());
        }
    }
    let err = std::process::Command::new(appimage)
        .args(std::env::args_os().skip(1))
        .exec();
    Err(anyhow!(err).context(format!("restarting {}", appimage.display())))
}

/// State of hyprdeck's own source checkout.
pub struct SourceInfo {
    pub dir: PathBuf,
    /// `None` when the directory is not a git checkout (e.g. an unpacked tarball).
    pub git: Option<GitInfo>,
    pub has_install_script: bool,
}

pub struct GitInfo {
    pub head: Option<Head>,
    pub branch: Option<String>,
    /// Number of uncommitted/untracked paths.
    pub dirty: usize,
}

pub struct Head {
    pub hash: String,
    pub subject: String,
    pub time: i64,
}

pub fn source_info(dir: &Path) -> Result<SourceInfo> {
    let git = if dir.join(".git").exists() && cmd::which("git").is_some() {
        Some(git_info(dir)?)
    } else {
        None
    };
    Ok(SourceInfo {
        has_install_script: is_executable(&dir.join("install.sh")),
        dir: dir.to_path_buf(),
        git,
    })
}

fn git_info(dir: &Path) -> Result<GitInfo> {
    let d = dir.to_string_lossy();
    let git =
        |args: &[&str]| cmd::run("git", ["-C", &d].iter().chain(args)).map(|s| s.trim().to_owned());
    let head = git(&["log", "-1", "--format=%h%x09%ct%x09%s"])
        .ok()
        .and_then(|line| {
            let mut it = line.splitn(3, '\t');
            Some(Head {
                hash: it.next()?.to_owned(),
                time: it.next()?.parse().ok()?,
                subject: it.next().unwrap_or_default().to_owned(),
            })
        });
    let branch = git(&["rev-parse", "--abbrev-ref", "HEAD"]).ok();
    let dirty = git(&["status", "--porcelain"])?
        .lines()
        .filter(|l| !l.trim().is_empty())
        .count();
    Ok(GitInfo {
        head,
        branch,
        dirty,
    })
}

fn is_executable(p: &Path) -> bool {
    std::fs::metadata(p).is_ok_and(|m| m.is_file() && m.mode() & 0o111 != 0)
}

/// Semver-compatible crate updates for hyprdeck's lockfile (`cargo update --dry-run`).
pub fn crate_updates(dir: &Path) -> Result<Vec<CrateUpdate>> {
    if cmd::which("cargo").is_none() {
        bail!("cargo not found on PATH");
    }
    let manifest = dir.join("Cargo.toml");
    let out = cmd::output(
        "cargo",
        [
            "update".as_ref(),
            "--dry-run".as_ref(),
            "--color".as_ref(),
            "never".as_ref(),
            "--manifest-path".as_ref(),
            manifest.as_os_str(),
        ],
    )?;
    if !out.ok() {
        bail!(
            "{}",
            failure("cargo update --dry-run", out.status, &out.stderr)
        );
    }
    Ok(parse::parse_cargo_dry_run(&out.stderr))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semver_precedence() {
        use Ordering::*;
        for (a, b, want) in [
            ("v0.2.0", "0.1.9", Greater),
            ("0.10.0", "0.9.0", Greater),
            ("1.0.0", "v1.0.0", Equal),
            ("1.0", "1.0.0", Equal),
            ("1.0.1", "1.0.0+build.5", Greater),
            ("1.0.0+a", "1.0.0+b", Equal),
            ("1.0.0-rc.1", "1.0.0", Less),
            ("1.0.0-alpha", "1.0.0-alpha.1", Less),
            ("1.0.0-alpha.1", "1.0.0-alpha.beta", Less),
            ("1.0.0-beta.2", "1.0.0-beta.11", Less),
            ("1.0.0-rc.1", "1.0.0-beta.11", Greater),
            ("2.0.0", "10.0.0", Less),
        ] {
            assert_eq!(compare_versions(a, b), want, "{a} vs {b}");
            assert_eq!(compare_versions(b, a), want.reverse(), "{b} vs {a}");
        }
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            url: format!("https://example.invalid/{name}"),
            size: 1,
        }
    }

    #[test]
    fn asset_selection_is_exact() {
        let all = [
            asset("Hyprdeck-aarch64.AppImage"),
            asset("Hyprdeck-x86_64.AppImage.zsync"),
            asset(CHECKSUM_ASSET),
            asset(APPIMAGE_ASSET),
        ];
        let sel = select_assets(&all).unwrap();
        assert_eq!(sel.appimage.name, APPIMAGE_ASSET);
        assert_eq!(
            sel.checksum.as_ref().map(|a| a.name.as_str()),
            Some(CHECKSUM_ASSET)
        );

        let sel = select_assets(&[asset(APPIMAGE_ASSET)]).unwrap();
        assert_eq!(sel.checksum, None);
        assert_eq!(
            select_assets(&[asset("hyprdeck-x86_64.appimage"), asset(CHECKSUM_ASSET)]),
            None
        );
        assert_eq!(select_assets(&[]), None);
    }

    #[test]
    fn sha256_lines() {
        let h = "E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855";
        assert_eq!(
            parse_sha256(&format!("{h}  Hyprdeck-x86_64.AppImage\n")),
            Some(h.to_ascii_lowercase())
        );
        assert_eq!(parse_sha256(h), Some(h.to_ascii_lowercase()));
        assert_eq!(parse_sha256("abc  file"), None);
        assert_eq!(parse_sha256(""), None);
    }

    /// End-to-end replace via `file://` URLs (curl + sha256sum, no network).
    #[test]
    fn install_replaces_appimage_and_checks_hash() {
        let dir = std::env::temp_dir().join(format!("hd-selfupdate-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join(APPIMAGE_ASSET);
        let new = dir.join("new.AppImage");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&new, b"new build").unwrap();
        let hash = cmd::run("sha256sum", [&new]).unwrap();
        let good = dir.join("good.sha256");
        let bad = dir.join("bad.sha256");
        std::fs::write(&good, &hash).unwrap();
        std::fs::write(&bad, format!("{}  x\n", "0".repeat(64))).unwrap();
        let url = |p: &Path| Asset {
            name: String::new(),
            url: format!("file://{}", p.display()),
            size: 0,
        };
        let assets = |sum: Option<&Path>| UpdateAssets {
            appimage: Asset {
                size: 9,
                ..url(&new)
            },
            checksum: sum.map(url),
        };

        let err = install_appimage(&target, &assets(Some(&bad))).unwrap_err();
        assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
        assert_eq!(std::fs::read(&target).unwrap(), b"old");

        assert!(install_appimage(&target, &assets(Some(&good))).unwrap());
        assert_eq!(std::fs::read(&target).unwrap(), b"new build");
        assert_ne!(std::fs::metadata(&target).unwrap().mode() & 0o111, 0);
        assert!(!install_appimage(&target, &assets(None)).unwrap());

        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(!names.iter().any(|n| n.ends_with(".part")), "{names:?}");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
