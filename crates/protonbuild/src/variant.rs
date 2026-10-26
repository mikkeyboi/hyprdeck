//! Hyprdeck variants: a runner plus a launcher that applies per-game profiles.
//!
//! A variant hardlinks every file of an installed runner into a new folder in
//! `compatibilitytools.d` and adds a small `proton` launcher. The runner's own script stays
//! in the variant as `proton.upstream`; Proton finds its files relative to the script it was
//! started as, so the game runs entirely from the variant. Only new files are ever written,
//! never a hardlinked one, so the original runner is never modified.
use std::collections::BTreeMap;
use std::fs;
use std::io::ErrorKind;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};

use crate::builder::BUILD_RECEIPT;
use crate::profile::{Family, Profiles};
use crate::recipe::Recipe;

pub const RECEIPT: &str = ".hyprdeck-variant.json";
pub const PREFIX: &str = "hyprdeck-";
const GAMES: &str = "hyprdeck-games";
const UPSTREAM: &str = "proton.upstream";

/// What a variant was made from, written into it so its lineage survives the source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub source: PathBuf,
    pub family: Family,
    pub display_name: String,
    /// Steam name of the runner it came from.
    #[serde(default)]
    pub source_name: String,
    /// Seconds since the Unix epoch.
    #[serde(default)]
    pub created: u64,
    /// The recipe, when the source is a Hyprdeck build.
    #[serde(default)]
    pub build: Option<Recipe>,
}

const LAUNCHER: &str = r#"#!/usr/bin/env python3
# Hyprdeck launcher: applies this game's Hyprdeck settings (its own file, else the global
# defaults), then runs the runner's own script. Variables already set, for example in Steam
# launch options, win over the settings.
import json
import os
import sys

here = os.path.dirname(os.path.abspath(sys.argv[0]))
app = os.environ.get("SteamAppId") or os.environ.get("SteamGameId") or ""
games = os.path.join(here, "hyprdeck-games")
profile = os.path.join(games, app + ".json")
if not app or not os.path.isfile(profile):
    profile = os.path.join(games, "defaults.json")
try:
    with open(profile) as f:
        for key, value in json.load(f).get("env", {}).items():
            os.environ.setdefault(key, str(value))
except FileNotFoundError:
    pass
except Exception as error:
    sys.stderr.write("hyprdeck: ignoring %s: %s\n" % (profile, error))

upstream = os.path.join(here, "proton.upstream")
os.execv(sys.executable, [sys.executable, upstream] + sys.argv[1:])
"#;

/// Folder name of the variant for a runner folder.
pub fn variant_name(source: &Path) -> Result<String> {
    let name = source
        .file_name()
        .and_then(|n| n.to_str())
        .context("Runner folder has no UTF-8 name")?;
    ensure!(
        !name.starts_with(PREFIX),
        "{name} is already a Hyprdeck variant"
    );
    Ok(format!("{PREFIX}{name}"))
}

/// Creates (or recreates) the variant of `source` in `tools_dir` and writes the settings.
/// `progress` gets (files linked, total files).
pub fn create(
    source: &Path,
    tools_dir: &Path,
    display_name: &str,
    profiles: &Profiles,
    progress: &mut dyn FnMut(u64, u64),
) -> Result<PathBuf> {
    ensure!(
        source.join("proton").is_file() && source.join("toolmanifest.vdf").is_file(),
        "{} is not a Proton runner (no proton script or toolmanifest.vdf)",
        source.display()
    );
    let name = variant_name(source)?;
    let family = Family::detect(&name[PREFIX.len()..])
        .or_else(|| Family::detect(display_name))
        .unwrap_or(Family::Ge);
    let target = tools_dir.join(&name);
    if target.exists() {
        remove(&target)?;
    }
    let staging = tools_dir.join(format!(".{name}.staging"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    let total = count_files(source)?;
    let mut done = 0;
    progress(0, total);
    let built = (|| -> Result<()> {
        link_tree(source, &staging, &mut |_| {
            done += 1;
            if done % 200 == 0 || done == total {
                progress(done, total);
            }
        })?;
        // Replace the hardlinked entry points with new files; never write through a link.
        fs::rename(staging.join("proton"), staging.join(UPSTREAM))?;
        write_new(&staging.join("proton"), LAUNCHER, 0o755)?;
        let manifest = staging.join("compatibilitytool.vdf");
        if manifest.exists() {
            fs::remove_file(&manifest)?;
        }
        let shown = format!("{display_name} (Hyprdeck)");
        write_new(&manifest, &compat_manifest(&name, &shown), 0o644)?;
        let receipt = Receipt {
            source: source.to_owned(),
            family,
            display_name: shown,
            source_name: display_name.to_owned(),
            created: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs()),
            build: fs::read_to_string(source.join(BUILD_RECEIPT))
                .ok()
                .and_then(|text| serde_json::from_str(&text).ok()),
        };
        write_new(
            &staging.join(RECEIPT),
            &serde_json::to_string_pretty(&receipt)?,
            0o644,
        )?;
        write_profiles(&staging, family, profiles)?;
        Ok(())
    })();
    if let Err(err) = built {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }
    fs::rename(&staging, &target)?;
    Ok(target)
}

/// Reads a variant's receipt; errors for folders Hyprdeck did not create.
pub fn receipt(variant: &Path) -> Result<Receipt> {
    let path = variant.join(RECEIPT);
    let text = fs::read_to_string(&path)
        .with_context(|| format!("{} is not a Hyprdeck variant", variant.display()))?;
    Ok(serde_json::from_str(&text)?)
}

/// Removes a variant. Refuses anything without a Hyprdeck receipt.
pub fn remove(variant: &Path) -> Result<()> {
    receipt(variant)?;
    fs::remove_dir_all(variant).with_context(|| format!("Removing {}", variant.display()))
}

/// Variants in `tools_dir`, with their receipts.
pub fn list(tools_dir: &Path) -> Result<Vec<(PathBuf, Receipt)>> {
    let mut found = Vec::new();
    let entries = match fs::read_dir(tools_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(found),
        Err(err) => return Err(err.into()),
    };
    for entry in entries {
        let path = entry?.path();
        let is_variant = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(PREFIX));
        if is_variant && let Ok(receipt) = receipt(&path) {
            found.push((path, receipt));
        }
    }
    found.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(found)
}

/// Rewrites every variant's environments from the saved settings.
pub fn sync_profiles(tools_dir: &Path, profiles: &Profiles) -> Result<()> {
    for (path, receipt) in list(tools_dir)? {
        write_profiles(&path, receipt.family, profiles)?;
    }
    Ok(())
}

#[derive(Serialize)]
struct GameEnv<'a> {
    env: &'a BTreeMap<String, String>,
    unsupported: &'a [&'static str],
}

/// Writes `defaults.json` from the global defaults and `<appid>.json` for each game with
/// overrides, already merged, so the launcher reads exactly one file.
fn write_profiles(variant: &Path, family: Family, profiles: &Profiles) -> Result<()> {
    let games = variant.join(GAMES);
    let staging = variant.join(format!(".{GAMES}.staging"));
    if staging.exists() {
        fs::remove_dir_all(&staging)?;
    }
    fs::create_dir(&staging)?;
    let files = std::iter::once(("defaults".to_owned(), profiles.global.clone())).chain(
        profiles
            .games
            .keys()
            .map(|app| (app.to_string(), profiles.effective(*app))),
    );
    for (name, profile) in files {
        let resolved = profile.resolve(family);
        let json = serde_json::to_string_pretty(&GameEnv {
            env: &resolved.env,
            unsupported: &resolved.unsupported,
        })?;
        write_new(&staging.join(format!("{name}.json")), &json, 0o644)?;
    }
    if games.exists() {
        fs::remove_dir_all(&games)?;
    }
    fs::rename(&staging, &games)?;
    Ok(())
}

fn compat_manifest(internal: &str, display: &str) -> String {
    let quote = |s: &str| s.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "\"compatibilitytools\"\n{{\n  \"compat_tools\"\n  {{\n    \"{}\"\n    {{\n      \"install_path\" \".\"\n      \"display_name\" \"{}\"\n      \"from_oslist\" \"windows\"\n      \"to_oslist\" \"linux\"\n    }}\n  }}\n}}\n",
        quote(internal),
        quote(display)
    )
}

fn write_new(path: &Path, contents: &str, mode: u32) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .with_context(|| format!("Creating {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

fn count_files(dir: &Path) -> Result<u64> {
    let mut total = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let kind = entry.file_type()?;
        total += if kind.is_dir() && !kind.is_symlink() {
            count_files(&entry.path())?
        } else {
            1
        };
    }
    Ok(total)
}

/// Mirrors `source` into `target` with hardlinks, copying when links cross filesystems.
/// `linked` is called once per file or symlink.
fn link_tree(source: &Path, target: &Path, linked: &mut dyn FnMut(&Path)) -> Result<()> {
    fs::create_dir(target).with_context(|| format!("Creating {}", target.display()))?;
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let from = entry.path();
        let to = target.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_symlink() {
            symlink(fs::read_link(&from)?, &to)?;
            linked(&from);
        } else if kind.is_dir() {
            link_tree(&from, &to, linked)?;
            fs::set_permissions(&to, fs::metadata(&from)?.permissions())?;
        } else if kind.is_file() {
            match fs::hard_link(&from, &to) {
                Ok(()) => (),
                Err(err) if err.raw_os_error() == Some(libc::EXDEV) => {
                    fs::copy(&from, &to)?;
                }
                Err(err) => return Err(err).with_context(|| format!("Linking {}", from.display())),
            }
            linked(&from);
        } else {
            bail!("Unsupported file type in runner: {}", from.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::profile::{GameProfile, Overrides};
    use std::os::unix::fs::MetadataExt;
    use std::process::Command;

    fn runner(tools: &Path, name: &str) -> PathBuf {
        let dir = tools.join(name);
        fs::create_dir_all(dir.join("files/lib/wine/dxvk")).unwrap();
        fs::write(dir.join("files/lib/wine/dxvk/d3d11.dll"), "dll").unwrap();
        // Prints what the real script would see: its directory and the profile's variables.
        write_new(
            &dir.join("proton"),
            "#!/usr/bin/env python3\nimport os, sys\nprint(os.path.dirname(os.path.abspath(sys.argv[0])))\nprint(os.environ.get('PROTON_ENABLE_HDR', '-'), os.environ.get('PROTON_LOG', '-'))\nprint(' '.join(sys.argv[1:]))\n",
            0o755,
        )
        .unwrap();
        fs::write(
            dir.join("toolmanifest.vdf"),
            "\"manifest\" { \"commandline\" \"/proton %verb%\" }",
        )
        .unwrap();
        fs::write(dir.join("compatibilitytool.vdf"), "original").unwrap();
        symlink("files/lib", dir.join("lib-link")).unwrap();
        dir
    }

    fn profiles() -> Profiles {
        let mut overrides = Overrides::default();
        overrides.set_flag("hdr", Some(true));
        Profiles {
            global: GameProfile::default(),
            games: BTreeMap::from([(1091500, overrides)]),
        }
    }

    #[test]
    fn variant_links_files_and_leaves_the_runner_untouched() {
        let tools = tempfile::tempdir().unwrap();
        let source = runner(tools.path(), "GE-Proton11-1");
        fs::write(
            source.join(BUILD_RECEIPT),
            serde_json::to_string(&Recipe::new(
                "GE-Proton11-1",
                crate::recipe::FlagProfile::V3,
            ))
            .unwrap(),
        )
        .unwrap();
        let mut seen = Vec::new();
        let variant = create(
            &source,
            tools.path(),
            "GE-Proton11-1",
            &profiles(),
            &mut |done, total| seen.push((done, total)),
        )
        .unwrap();
        assert_eq!(variant, tools.path().join("hyprdeck-GE-Proton11-1"));
        assert_eq!(seen.first(), Some(&(0, 6)));
        assert_eq!(seen.last(), Some(&(6, 6)));
        let lineage = receipt(&variant).unwrap();
        assert_eq!(lineage.source_name, "GE-Proton11-1");
        assert_eq!(lineage.build.unwrap().base, "GE-Proton11-1");
        assert!(lineage.created > 0);
        let dll = "files/lib/wine/dxvk/d3d11.dll";
        assert_eq!(
            fs::metadata(source.join(dll)).unwrap().ino(),
            fs::metadata(variant.join(dll)).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(source.join("proton")).unwrap().ino(),
            fs::metadata(variant.join(UPSTREAM)).unwrap().ino()
        );
        assert_ne!(
            fs::metadata(source.join("proton")).unwrap().ino(),
            fs::metadata(variant.join("proton")).unwrap().ino()
        );
        assert_eq!(
            fs::read_to_string(source.join("compatibilitytool.vdf")).unwrap(),
            "original"
        );
        let manifest = fs::read_to_string(variant.join("compatibilitytool.vdf")).unwrap();
        assert!(manifest.contains("\"hyprdeck-GE-Proton11-1\""));
        assert!(manifest.contains("GE-Proton11-1 (Hyprdeck)"));
        assert_eq!(
            fs::read_link(variant.join("lib-link")).unwrap(),
            Path::new("files/lib")
        );
        let env = fs::read_to_string(variant.join("hyprdeck-games/1091500.json")).unwrap();
        assert!(env.contains("PROTON_ENABLE_HDR"));
        assert_eq!(receipt(&variant).unwrap().family, Family::Ge);
    }

    #[test]
    fn launcher_applies_profile_without_overriding_explicit_env() {
        if Command::new("python3").arg("-V").output().is_err() {
            return;
        }
        let tools = tempfile::tempdir().unwrap();
        let source = runner(tools.path(), "GE-Proton11-1");
        let mut games = profiles();
        games.global.proton_log = true;
        games.global.dlss_upgrade = true;
        let variant = create(
            &source,
            tools.path(),
            "GE-Proton11-1",
            &games,
            &mut |_, _| (),
        )
        .unwrap();
        let out = Command::new(variant.join("proton"))
            .args(["waitforexitandrun", "game.exe"])
            .env("SteamAppId", "1091500")
            .env("PROTON_LOG", "+seh")
            .env_remove("PROTON_ENABLE_HDR")
            .output()
            .unwrap();
        let text = String::from_utf8(out.stdout).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(Path::new(lines[0]), variant);
        assert_eq!(lines[1], "1 +seh");
        assert_eq!(lines[2], "waitforexitandrun game.exe");

        // Games without overrides get the global defaults: log on, HDR off.
        let other = Command::new(variant.join("proton"))
            .arg("run")
            .env("SteamAppId", "42")
            .env_remove("PROTON_ENABLE_HDR")
            .env_remove("PROTON_LOG")
            .output()
            .unwrap();
        assert!(String::from_utf8(other.stdout).unwrap().contains("- 1"));
    }

    #[test]
    fn sync_rewrites_profiles_and_remove_requires_a_receipt() {
        let tools = tempfile::tempdir().unwrap();
        let source = runner(tools.path(), "proton-cachyos-11.0-20260702-slr-x86_64_v3");
        let variant = create(
            &source,
            tools.path(),
            "proton-cachyos",
            &profiles(),
            &mut |_, _| (),
        )
        .unwrap();
        let env = fs::read_to_string(variant.join("hyprdeck-games/1091500.json")).unwrap();
        assert!(env.contains("DXVK_HDR"));
        sync_profiles(tools.path(), &Profiles::default()).unwrap();
        assert!(!variant.join("hyprdeck-games/1091500.json").exists());
        assert!(variant.join("hyprdeck-games/defaults.json").exists());
        assert_eq!(list(tools.path()).unwrap().len(), 1);
        assert!(remove(&source).is_err());
        assert!(source.join("proton").exists());
        remove(&variant).unwrap();
        assert!(!variant.exists());
        assert!(variant_name(&tools.path().join("hyprdeck-x")).is_err());
    }
}
