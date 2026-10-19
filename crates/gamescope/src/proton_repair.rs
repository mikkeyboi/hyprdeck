//! Explicit, reversible repair of a recognized upstream parser—not prefix cleanup.
//! Installation can repair private staged files; changes to installed runners require
//! stopped launchers. Original source is retained and unknown/edited layouts fail closed.
use crate::proton::{self, InstalledTool, Target};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
    process::{Command, Stdio},
};

pub(crate) const RECORD: &str = ".hyprdeck-prefix-parser.json";
const MAX_SOURCE: usize = 1024 * 1024;
const PLANNER: &str = include_str!("proton_repair.py");

pub const WARNING: &str = "Optional workaround for an exact, recognized upstream prefix-version parser defect. The original runner source is preserved for restoration. Repair changes the runner, not a prefix, saves or game assignments. Correct parsing can enable normal upstream prefix migrations that were previously skipped: back up game prefixes before using the repaired runner. Restoring the runner does not undo migrations already performed. Unknown or edited source is refused; this is not a guaranteed game-launch fix.";
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Available { version: String },
    Applied { version: String },
    Restored { version: String },
    NotApplicable { reason: String },
}

#[derive(Deserialize)]
struct Plan {
    kind: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    reason: String,
    patched: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Record {
    schema: u32,
    original: String,
    original_mode: u32,
}

fn plan(source: &str) -> Result<Plan> {
    ensure!(
        source.len() <= MAX_SOURCE,
        "Runner source exceeds the repair inspection limit"
    );
    let mut child = Command::new("python3")
        .args(["-I", "-S", "-c", PLANNER])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("Python 3 is required to inspect the runner without executing its source")?;
    let input = child
        .stdin
        .take()
        .context("Cannot open parser inspection input")?;
    // The helper reads all input before responding; no runner imports/code execute.
    let write = {
        let mut input = input;
        input.write_all(source.as_bytes())
    };
    let output = child
        .wait_with_output()
        .context("Waiting for parser inspection")?;
    write.context("Sending source to parser inspection")?;
    ensure!(
        output.status.success(),
        "Parser inspection failed; the runner was not changed"
    );
    serde_json::from_slice(&output.stdout).context("Parser inspection returned invalid metadata")
}

fn read_owned(path: &Path, limit: usize) -> Result<(String, u32)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("Cannot safely read {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file() && metadata.uid() == unsafe { libc::geteuid() } && metadata.nlink() == 1,
        "Parser repair requires an owned, regular, non-hardlinked file"
    );
    ensure!(
        metadata.len() <= limit as u64,
        "Parser repair file exceeds the inspection limit"
    );
    let mut text = String::new();
    file.take(limit as u64 + 1).read_to_string(&mut text)?;
    ensure!(
        text.len() <= limit,
        "Parser repair file exceeds the inspection limit"
    );
    Ok((text, metadata.permissions().mode() & 0o777))
}

fn validate_directory(path: &Path) -> Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Parser repair requires an owned, non-symlink, non-shared tool directory"
    );
    Ok(())
}

fn validate_managed(target: &Target, tool: &InstalledTool) -> Result<()> {
    let root = fs::canonicalize(&target.root)?;
    ensure!(
        tool.target_id == target.id && tool.path.parent() == Some(root.as_path()),
        "Runner does not belong to the selected installation target"
    );
    validate_directory(&tool.path)?;
    ensure!(
        proton::installed(target)?
            .iter()
            .any(|installed| installed.name == tool.name
                && installed.path == tool.path
                && installed.managed),
        "Only a verified Hyprdeck-managed runner can be repaired or restored"
    );
    Ok(())
}

fn record(path: &Path) -> Result<Option<Record>> {
    match fs::symlink_metadata(path.join(RECORD)) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(_) => {
            let (text, _) = read_owned(&path.join(RECORD), MAX_SOURCE * 8)?;
            let record: Record =
                serde_json::from_str(&text).context("Invalid parser backup metadata")?;
            ensure!(
                record.schema == 1
                    && record.original.len() <= MAX_SOURCE
                    && record.original_mode & !0o777 == 0,
                "Unsupported parser backup metadata"
            );
            Ok(Some(record))
        }
    }
}

fn recorded_plan(record: &Record) -> Result<Plan> {
    let planned = plan(&record.original)?;
    ensure!(
        planned.kind == "affected" && planned.patched.is_some(),
        "The saved upstream parser is not a recognized affected layout; nothing was changed"
    );
    Ok(planned)
}

fn inspect_files(path: &Path) -> Result<Status> {
    validate_directory(path)?;
    let (source, _) = read_owned(&path.join("proton"), MAX_SOURCE)?;
    if let Some(record) = record(path)? {
        let planned = recorded_plan(&record)?;
        if planned.patched.as_deref() == Some(&source) {
            return Ok(Status::Applied {
                version: planned.version,
            });
        }
        ensure!(
            source == record.original,
            "Runner changed since repair; automatic restoration is refused"
        );
        return Ok(Status::Restored {
            version: planned.version,
        });
    }
    let planned = plan(&source)?;
    Ok(match planned.kind.as_str() {
        "affected" => Status::Available {
            version: planned.version,
        },
        "fixed" => Status::NotApplicable {
            reason:
                "This runner already uses the recognized normalized parser; no repair is needed."
                    .into(),
        },
        _ => Status::NotApplicable {
            reason: planned.reason,
        },
    })
}

pub fn inspect(target: &Target, tool: &InstalledTool) -> Result<Status> {
    validate_managed(target, tool)?;
    inspect_files(&tool.path)
}

fn replace_source(path: &Path, expected: &str, replacement: &str, mode: u32) -> Result<()> {
    let (current, _) = read_owned(&path.join("proton"), MAX_SOURCE)?;
    ensure!(
        current == expected,
        "Runner changed during repair; nothing was replaced"
    );
    let mut file = tempfile::Builder::new()
        .prefix(".hyprdeck-parser-")
        .tempfile_in(path)?;
    file.write_all(replacement.as_bytes())?;
    file.as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    file.as_file().sync_all()?;
    validate_directory(path)?;
    file.persist(path.join("proton"))
        .map_err(|e| e.error)
        .context("Atomically replacing the verified parser")?;
    File::open(path)?.sync_all()?;
    Ok(())
}

fn apply_files(path: &Path) -> Result<Status> {
    validate_directory(path)?;
    let (source, mode) = read_owned(&path.join("proton"), MAX_SOURCE)?;
    let (planned, original_mode) = if let Some(record) = record(path)? {
        let planned = recorded_plan(&record)?;
        if planned.patched.as_deref() == Some(&source) {
            return Ok(Status::Applied {
                version: planned.version,
            });
        }
        ensure!(
            source == record.original,
            "Runner changed since repair; it was not modified"
        );
        (planned, record.original_mode)
    } else {
        let planned = plan(&source)?;
        if planned.kind == "fixed" {
            return Ok(Status::NotApplicable {
                reason: "This upstream parser is already normalized; no modification was made."
                    .into(),
            });
        }
        ensure!(
            planned.kind == "affected",
            "Parser repair unavailable: {}",
            planned.reason
        );
        let backup = Record {
            schema: 1,
            original: source.clone(),
            original_mode: mode,
        };
        let mut file = tempfile::Builder::new()
            .prefix(".hyprdeck-parser-backup-")
            .tempfile_in(path)?;
        serde_json::to_writer(&mut file, &backup)?;
        file.as_file().sync_all()?;
        file.persist_noclobber(path.join(RECORD))
            .map_err(|e| e.error)
            .context("Preserving the original upstream parser without overwriting a backup")?;
        File::open(path)?.sync_all()?;
        (planned, mode)
    };
    let patched = planned
        .patched
        .context("Recognized parser has no repair plan")?;
    replace_source(path, &source, &patched, original_mode)?;
    Ok(Status::Applied {
        version: planned.version,
    })
}

/// Called only on the private extracted installation before its atomic cutover.
/// No launcher can select this staging directory and no installed tool is replaced.
pub(crate) fn repair_staged(path: &Path) -> Result<Status> {
    apply_files(path)
}

pub fn apply(target: &Target, tool: &InstalledTool) -> Result<Status> {
    validate_managed(target, tool)?;
    proton::require_stopped(target)?;
    apply_files(&tool.path)
}

fn restore_files(path: &Path) -> Result<Status> {
    validate_directory(path)?;
    let record = record(path)?.context("No original upstream parser backup is recorded")?;
    let planned = recorded_plan(&record)?;
    let patched = planned
        .patched
        .context("No verified repaired parser is recorded")?;
    let (source, _) = read_owned(&path.join("proton"), MAX_SOURCE)?;
    ensure!(
        source == patched || source == record.original,
        "Runner changed since repair; restoring it automatically would overwrite unknown changes"
    );
    if source != record.original {
        replace_source(path, &patched, &record.original, record.original_mode)?;
    }
    Ok(Status::Restored {
        version: planned.version,
    })
}

pub fn restore(target: &Target, tool: &InstalledTool) -> Result<Status> {
    validate_managed(target, tool)?;
    proton::require_stopped(target)?;
    restore_files(&tool.path)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &str = "CURRENT_PREFIX_VERSION = 'Vendor-42.7-900'\nclass Prefix:\n    def upgrade_pfx(self, old_ver):\n        try:\n            old_proton_ver, old_prefix_ver = old_ver.split('-')\n            old_proton_maj, old_proton_min = old_proton_ver.split('.')\n            new_proton_ver, _ = CURRENT_PREFIX_VERSION.split('-')\n            new_proton_maj, new_proton_min = new_proton_ver.split('.')\n            return [int(old_proton_maj), int(old_proton_min), int(old_prefix_ver), int(new_proton_maj), int(new_proton_min), old_proton_ver]\n        except ValueError:\n            return 'invalid'\n";

    fn exercise(source: &str, old: &str) -> serde_json::Value {
        // Only our self-contained fixture is executed. No installed/downloaded
        // runner, migration I/O or imports are executed by the repair planner.
        let mut child = Command::new("python3").args(["-I", "-S", "-c", "import json,sys; d=json.load(sys.stdin); ns={}; exec(d['source'],ns); print(json.dumps(ns['Prefix']().upgrade_pfx(d['old'])))"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        serde_json::to_writer(
            child.stdin.take().unwrap(),
            &serde_json::json!({"source":source,"old":old}),
        )
        .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success());
        serde_json::from_slice(&output.stdout).unwrap()
    }
    fn fixture() -> (tempfile::TempDir, String) {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(dir.path().join("proton"), SOURCE).unwrap();
        fs::set_permissions(dir.path().join("proton"), fs::Permissions::from_mode(0o755)).unwrap();
        (dir, SOURCE.into())
    }
    #[test]
    fn repairs_vendor_and_plain_markers_without_skipping_comparisons() {
        assert_eq!(exercise(SOURCE, "41.5-600"), serde_json::json!("invalid"));
        let planned = plan(SOURCE).unwrap();
        let changed = planned.patched.unwrap();
        assert_eq!(
            exercise(&changed, "41.5-600"),
            serde_json::json!([41, 5, 600, 42, 7, "41.5"])
        );
        assert_eq!(
            exercise(&changed, "Another-Vendor-43.1-901"),
            serde_json::json!([43, 1, 901, 42, 7, "43.1"])
        );
        assert_eq!(
            exercise(&changed, "invalid-marker"),
            serde_json::json!("invalid")
        );
        assert_eq!(plan(&changed).unwrap().kind, "fixed");
    }
    #[test]
    fn keeps_original_and_round_trips_without_touching_prefix_data() {
        let (dir, original) = fixture();
        fs::write(dir.path().join("save-sentinel"), "untouched").unwrap();
        assert!(matches!(
            apply_files(dir.path()).unwrap(),
            Status::Applied { .. }
        ));
        assert!(matches!(
            inspect_files(dir.path()).unwrap(),
            Status::Applied { .. }
        ));
        restore_files(dir.path()).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join("proton")).unwrap(),
            original
        );
        assert_eq!(
            fs::read_to_string(dir.path().join("save-sentinel")).unwrap(),
            "untouched"
        );
        assert_eq!(
            fs::metadata(dir.path().join("proton"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert!(matches!(
            inspect_files(dir.path()).unwrap(),
            Status::Restored { .. }
        ));
        apply_files(dir.path()).unwrap();
        assert!(matches!(
            inspect_files(dir.path()).unwrap(),
            Status::Applied { .. }
        ));
    }
    #[test]
    fn caller_flags_cannot_authorize_changes_to_unmanaged_runners() {
        let root = tempfile::tempdir().unwrap();
        let (dir, original) = fixture();
        let destination = root.path().join("runner");
        fs::rename(dir.path(), &destination).unwrap();
        fs::write(
            destination.join("compatibilitytool.vdf"),
            r#""compatibilitytools" { "compat_tools" { "fixture" { "install_path" "." } } }"#,
        )
        .unwrap();
        let target = Target {
            id: "fixture-target".into(),
            name: "Fixture".into(),
            root: root.path().to_path_buf(),
            kind: proton::TargetKind::Steam,
            launcher_root: None,
            steam_packaging: None,
        };
        let tool = InstalledTool {
            name: "fixture".into(),
            display_name: "Fixture".into(),
            path: destination.clone(),
            target_id: target.id.clone(),
            managed: true,
        };
        assert!(inspect(&target, &tool).is_err());
        assert!(apply(&target, &tool).is_err());
        assert!(restore(&target, &tool).is_err());
        assert_eq!(
            fs::read_to_string(destination.join("proton")).unwrap(),
            original
        );
        assert!(!destination.join(RECORD).exists());
    }
    #[test]
    fn refuses_unknown_or_edited_source_and_symlinked_entrypoints() {
        let (dir, _) = fixture();
        fs::write(dir.path().join("proton"), "# unrelated upstream layout\n").unwrap();
        assert!(apply_files(dir.path()).is_err());
        assert!(!dir.path().join(RECORD).exists());
        fs::write(dir.path().join("proton"), SOURCE).unwrap();
        apply_files(dir.path()).unwrap();
        fs::write(dir.path().join("proton"), "# user changes\n").unwrap();
        assert!(restore_files(dir.path()).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("proton")).unwrap(),
            "# user changes\n"
        );
        fs::remove_file(dir.path().join("proton")).unwrap();
        std::os::unix::fs::symlink(dir.path().join(RECORD), dir.path().join("proton")).unwrap();
        assert!(apply_files(dir.path()).is_err());
    }
}
