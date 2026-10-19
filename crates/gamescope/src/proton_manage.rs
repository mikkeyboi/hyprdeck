//! Explicit changes to user-owned standalone runners; never tool IDs or prefixes.
use super::{
    InstalledTool, RECEIPT, Target, TargetKind, describe, ensure_unreferenced, rename_noreplace,
    require_stopped, steam_root, valid_receipt,
};
use anyhow::{Context, Result, bail, ensure};
use std::ffi::CString;
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MANIFEST: &str = "compatibilitytool.vdf";
const MAX_MANIFEST: u64 = 1024 * 1024;

#[derive(Clone, Debug)]
pub struct Capabilities {
    pub editable: bool,
    pub removable: bool,
    pub reason: String,
}

struct Manifest {
    source: String,
    metadata: Metadata,
    path: Vec<String>,
}

struct Verified {
    root: PathBuf,
    root_directory: File,
    directory: File,
    metadata: Metadata,
    manifest: Option<Manifest>,
    descriptor: InstalledTool,
}

fn same_inode(left: &Metadata, right: &Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino()
}

fn same_file(left: &Metadata, right: &Metadata) -> bool {
    same_inode(left, right)
        && left.len() == right.len()
        && left.mode() == right.mode()
        && left.uid() == right.uid()
        && left.gid() == right.gid()
        && left.mtime() == right.mtime()
        && left.mtime_nsec() == right.mtime_nsec()
        && left.ctime() == right.ctime()
        && left.ctime_nsec() == right.ctime_nsec()
}

fn preserve_group(file: &File, original: &Metadata) -> Result<()> {
    if file.metadata()?.gid() != original.gid() {
        ensure!(
            unsafe { libc::fchown(file.as_raw_fd(), !0, original.gid()) } == 0,
            "Cannot preserve the original manifest group: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn anchored(directory: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", directory.as_raw_fd()))
}

fn open_directory(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("Cannot safely open directory {}", path.display()))?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Changes require a user-owned, non-shared directory (not a system/package-owned runner)"
    );
    Ok(file)
}

fn owned_manifest(path: &Path) -> Result<(String, Metadata)> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("Cannot safely read the compatibility manifest")?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.nlink() == 1
            && metadata.mode() & 0o022 == 0,
        "Compatibility manifest must be an owned, non-shared, non-hardlinked regular file"
    );
    ensure!(
        metadata.len() <= MAX_MANIFEST,
        "Compatibility manifest is too large"
    );
    let mut source = String::new();
    file.take(MAX_MANIFEST + 1).read_to_string(&mut source)?;
    ensure!(
        source.len() as u64 <= MAX_MANIFEST,
        "Compatibility manifest is too large"
    );
    Ok((source, metadata))
}

fn validate_name(name: &str) -> Result<()> {
    ensure!(
        !name.trim().is_empty()
            && name == name.trim()
            && name.len() <= 256
            && !name.chars().any(|character| {
                character.is_control()
                    || matches!(character, '\u{2028}' | '\u{2029}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
            }),
        "Display name must be plain text, 1–256 bytes, without surrounding whitespace or line breaks"
    );
    Ok(())
}

fn manifest_path(source: &str, id: &str, display_name: &str) -> Result<Vec<String>> {
    let parsed = crate::steam::parse_vdf(source)?;
    let compatibility = parsed
        .get("compatibilitytools")
        .context("Missing compatibilitytools section")?;
    let (tools, nested) = match compatibility.get("compat_tools") {
        Some(tools) => (tools, true),
        None => (compatibility, false),
    };
    let entries = tools
        .entries()
        .context("Missing compatibility tool definitions")?;
    let declarations: Vec<_> = entries
        .iter()
        .filter(|(_, values)| values.entries().is_some())
        .collect();
    ensure!(
        declarations.len() == 1,
        "This directory declares multiple tools; editing/removal is read-only because every declared ID cannot be guarded independently"
    );
    let (declared_id, values) = declarations[0];
    ensure!(
        declared_id == id,
        "Compatibility tool ID changed; refresh the inventory"
    );
    for key in ["install_path", "display_name"] {
        ensure!(
            values
                .entries()
                .context("Invalid compatibility tool definition")?
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case(key))
                .count()
                <= 1,
            "Duplicate compatibility tool {key} is ambiguous; changes are blocked"
        );
    }
    let install_path = match values.get("install_path") {
        Some(value) => value
            .text()
            .context("Compatibility install_path is not plain text")?,
        None => ".",
    };
    ensure!(
        install_path == ".",
        "Only standalone tools with install_path '.' can be changed; nested/external install paths are read-only"
    );
    let mut path = vec!["compatibilitytools".to_owned()];
    if nested {
        path.push("compat_tools".to_owned());
    }
    path.push(id.to_owned());
    path.push("display_name".to_owned());
    // The lossless editor also rejects ambiguous duplicate keys, conditions and
    // external directives that the read-only inventory does not need to edit.
    let parts: Vec<_> = path.iter().map(String::as_str).collect();
    crate::vdf_edit::set_string(source, &parts, display_name)?;
    Ok(path)
}
fn package_path(path: &Path) -> bool {
    path == Path::new("/")
        || [
            "/usr",
            "/opt",
            "/nix",
            "/app",
            "/snap",
            "/etc",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/boot",
            "/dev",
            "/proc",
            "/sys",
            "/run",
            "/var/lib",
            "/var/db",
            "/var/cache",
            "/var/snap",
            "/var/opt",
        ]
        .iter()
        .any(|system| path.starts_with(system))
        || path
            .components()
            .zip(path.components().skip(1))
            .any(|(parent, child)| {
                parent.as_os_str() == "steamapps" && child.as_os_str() == "common"
            })
}

fn verify(target: &Target, tool: &InstalledTool) -> Result<Verified> {
    ensure!(
        tool.target_id == target.id,
        "Tool belongs to a different target"
    );
    let root = fs::canonicalize(&target.root).context("Cannot resolve this tool target")?;
    ensure!(
        !package_path(&root),
        "System/package and Steam appmanifest installation directories are read-only"
    );
    let root_file = open_directory(&root)?;
    if target.kind == TargetKind::Steam {
        steam_root(target).context("Cannot establish the Steam owner of this standalone tool")?;
        let inventory = crate::steam::inventory(std::slice::from_ref(target))?;
        ensure!(
            inventory.warnings.is_empty(),
            "Steam package ownership could not be completely inspected; changes are blocked"
        );
        ensure!(
            !inventory.steam_tools.iter().any(|official| {
                fs::canonicalize(&official.path).is_ok_and(|path| tool.path.starts_with(path))
            }),
            "Steam appmanifest tools must be managed through the Steam client"
        );
    }
    ensure!(
        tool.path.parent() == Some(root.as_path())
            && tool
                .path
                .file_name()
                .is_some_and(|name| !name.as_bytes().starts_with(b".")),
        "Tool must be an immediate, non-hidden child of its selected target"
    );
    let directory = open_directory(&tool.path)?;
    let metadata = directory.metadata()?;
    ensure!(
        same_inode(&root_file.metadata()?, &fs::metadata(&root)?),
        "Tool target changed while inspecting it"
    );
    let path = anchored(&directory);
    let manifest = match fs::symlink_metadata(path.join(MANIFEST)) {
        Ok(_) => {
            let (source, metadata) = owned_manifest(&path.join(MANIFEST))?;
            let manifest_path = manifest_path(&source, &tool.name, &tool.display_name)?;
            Some(Manifest {
                source,
                metadata,
                path: manifest_path,
            })
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    // describe() is reused rather than trusting caller-supplied managed/name/path.
    // Check the lexical entry both before and after its canonicalizing read.
    ensure!(
        same_inode(&metadata, &fs::symlink_metadata(&tool.path)?),
        "Tool directory changed"
    );
    let descriptions = describe(target, &tool.path)?;
    ensure!(
        descriptions.len() == 1,
        "Only a single standalone runner can be changed"
    );
    let descriptor = descriptions
        .into_iter()
        .next()
        .context("Runner no longer exists")?;
    ensure!(
        descriptor.name == tool.name
            && descriptor.display_name == tool.display_name
            && descriptor.path == tool.path
            && descriptor.target_id == tool.target_id
            && descriptor.managed == tool.managed,
        "Tool descriptor changed or does not match this installation; refresh the inventory"
    );
    match fs::symlink_metadata(path.join(RECEIPT)) {
        Ok(_) => {
            valid_receipt(target, &tool.path, &tool.name)
                .context("Invalid Hyprdeck ownership receipt; changes are blocked")?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            ensure!(
                !descriptor.managed,
                "Hyprdeck ownership receipt disappeared"
            );
        }
        Err(error) => return Err(error.into()),
    }
    let verified = Verified {
        root,
        root_directory: root_file,
        directory,
        metadata,
        manifest,
        descriptor,
    };
    recheck(&verified, &tool.path)?;
    Ok(verified)
}

fn recheck(verified: &Verified, path: &Path) -> Result<()> {
    ensure!(
        same_inode(
            &verified.root_directory.metadata()?,
            &fs::metadata(&verified.root)?
        ),
        "Tool target changed during the operation"
    );
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        metadata.is_dir()
            && !metadata.file_type().is_symlink()
            && same_inode(&verified.metadata, &metadata)
            && metadata.uid() == verified.metadata.uid()
            && metadata.mode() == verified.metadata.mode()
            && metadata.mtime() == verified.metadata.mtime()
            && metadata.mtime_nsec() == verified.metadata.mtime_nsec(),
        "Tool directory changed during the operation; nothing else will be deleted or replaced"
    );
    if let Some(manifest) = &verified.manifest {
        let (source, metadata) = owned_manifest(&anchored(&verified.directory).join(MANIFEST))?;
        ensure!(
            same_file(&manifest.metadata, &metadata) && source == manifest.source,
            "Compatibility manifest changed; refresh the inventory"
        );
    }
    Ok(())
}

/// Structural capability only: launcher and mapping guards run when committing.
/// Keeping referenced tools visible allows the UI to explicitly unassign first.
pub fn capabilities(target: &Target, tool: &InstalledTool) -> Result<Capabilities> {
    Ok(match verify(target, tool) {
        Ok(verified) => Capabilities {
            editable: verified.manifest.is_some(),
            removable: true,
            reason: if verified.manifest.is_some() {
                "User-owned standalone tool; only its display name is editable. Removal requires no saved game/default references and a stopped launcher.".into()
            } else {
                "User-owned standalone runner can be removed. Display-name editing requires a compatibilitytool.vdf manifest.".into()
            },
        },
        Err(error) => Capabilities {
            editable: false,
            removable: false,
            reason: format!("{error:#}"),
        },
    })
}

fn lock_tool(directory: &File) -> Result<()> {
    if unsafe { libc::flock(directory.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("Another operation is changing this runner");
    }
    Ok(())
}

fn exchange(source: &Path, destination: &Path) -> Result<()> {
    let source = CString::new(source.as_os_str().as_bytes())?;
    let destination = CString::new(destination.as_os_str().as_bytes())?;
    if unsafe {
        libc::renameat2(
            libc::AT_FDCWD,
            source.as_ptr(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            libc::RENAME_EXCHANGE,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error())
            .context("Atomically exchanging the compatibility manifest");
    }
    Ok(())
}

fn rename_verified(
    target: &Target,
    verified: Verified,
    display_name: &str,
) -> Result<InstalledTool> {
    lock_tool(&verified.directory)?;
    validate_name(display_name)?;
    let manifest = verified
        .manifest
        .as_ref()
        .context("This runner has no editable compatibilitytool.vdf manifest")?;
    let path: Vec<_> = manifest.path.iter().map(String::as_str).collect();
    let updated = crate::vdf_edit::set_string(&manifest.source, &path, display_name)?;
    if updated == manifest.source {
        return Ok(verified.descriptor);
    }
    recheck(&verified, &verified.descriptor.path)?;
    let directory = anchored(&verified.directory);
    let mut backup = tempfile::Builder::new()
        .prefix(".hyprdeck-display-backup-")
        .tempfile_in(&directory)?;
    backup.write_all(manifest.source.as_bytes())?;
    preserve_group(backup.as_file(), &manifest.metadata)?;
    backup
        .as_file()
        .set_permissions(manifest.metadata.permissions())?;
    backup.as_file().sync_all()?;
    // Retain a unique original for every edit; never overwrite an earlier backup.
    let (_, backup_path) = backup.keep().map_err(|error| error.error)?;
    let backup_path = verified
        .descriptor
        .path
        .join(backup_path.file_name().context("Backup has no filename")?);
    let mut replacement = tempfile::Builder::new()
        .prefix(".hyprdeck-display-")
        .tempfile_in(&directory)?;
    replacement.write_all(updated.as_bytes())?;
    preserve_group(replacement.as_file(), &manifest.metadata)?;
    replacement
        .as_file()
        .set_permissions(manifest.metadata.permissions())?;
    replacement.as_file().sync_all()?;
    // Creating the backup/tempfile changes the directory mtime, so validate the
    // directory identity and the original manifest separately at cutover.
    ensure!(
        same_inode(
            &verified.metadata,
            &fs::symlink_metadata(&verified.descriptor.path)?
        ),
        "Tool directory changed; original manifest backup is at {}",
        backup_path.display()
    );
    let (current, metadata) = owned_manifest(&directory.join(MANIFEST))?;
    ensure!(
        same_file(&manifest.metadata, &metadata) && current == manifest.source,
        "Manifest changed before replacement; the original was backed up"
    );
    let replacement_metadata = replacement.as_file().metadata()?;
    exchange(replacement.path(), &directory.join(MANIFEST))?;
    // Exchange retains the displaced inode until it has been verified. If a
    // concurrent writer replaced the manifest, restore it rather than discarding
    // the unexpected file. Rename can change ctime, so compare inode + contents.
    let displaced_matches = owned_manifest(replacement.path()).is_ok_and(|(source, metadata)| {
        source == manifest.source
            && same_inode(&metadata, &manifest.metadata)
            && metadata.mode() == manifest.metadata.mode()
            && metadata.uid() == manifest.metadata.uid()
            && metadata.gid() == manifest.metadata.gid()
            && metadata.mtime() == manifest.metadata.mtime()
            && metadata.mtime_nsec() == manifest.metadata.mtime_nsec()
    });
    if !displaced_matches {
        let current_is_ours = fs::symlink_metadata(directory.join(MANIFEST))
            .is_ok_and(|metadata| same_inode(&replacement_metadata, &metadata));
        if current_is_ours && exchange(replacement.path(), &directory.join(MANIFEST)).is_ok() {
            bail!("Manifest was replaced during editing; the concurrent version was restored");
        }
        let (_, preserved) = replacement.keep().map_err(|error| error.error)?;
        let preserved = verified.descriptor.path.join(
            preserved
                .file_name()
                .context("Preserved manifest has no filename")?,
        );
        bail!(
            "Manifest changed during editing; the displaced file was preserved at {}",
            preserved.display()
        );
    }
    verified.directory.sync_all()?;
    ensure!(
        same_inode(
            &verified.metadata,
            &fs::symlink_metadata(&verified.descriptor.path)?
        ),
        "Tool moved during editing; refresh the inventory"
    );
    let refreshed = describe(target, &verified.descriptor.path)?;
    let refreshed = refreshed
        .into_iter()
        .find(|tool| tool.name == verified.descriptor.name)
        .context("Edited tool is missing from its refreshed descriptor")?;
    ensure!(
        refreshed.display_name == display_name,
        "Edited display name could not be re-read"
    );
    Ok(refreshed)
}

fn lock_steam(target: &Target) -> Result<Vec<File>> {
    if target.kind != TargetKind::Steam {
        return Ok(Vec::new());
    }
    let tools_root = fs::canonicalize(&target.root)?;
    let mut owners = std::collections::BTreeSet::new();
    owners.insert(steam_root(target)?);
    for candidate in super::targets()? {
        if candidate.kind == TargetKind::Steam
            && fs::canonicalize(&candidate.root).is_ok_and(|root| root == tools_root)
        {
            owners.insert(steam_root(&candidate)?);
        }
    }
    // A shared tools directory can belong to several Steam installations. Lock
    // them in stable order so an assignment through any detected alias cannot
    // race the final mapping check and directory quarantine.
    owners
        .into_iter()
        .map(|root| crate::steam_assign::lock_steam_root(&root))
        .collect()
}

pub fn rename(target: &Target, tool: &InstalledTool, display_name: &str) -> Result<InstalledTool> {
    validate_name(display_name)?;
    require_stopped(target)?;
    let _steam_lock = lock_steam(target)?;
    let verified = verify(target, tool)?;
    rename_verified(target, verified, display_name)
}

fn check_tree(directory: &File, depth: usize) -> Result<()> {
    ensure!(
        depth <= 128,
        "Runner directory nesting exceeds the removal safety limit"
    );
    let device = directory.metadata()?.dev();
    for entry in fs::read_dir(anchored(directory))? {
        let entry = entry?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Runner contains files owned by another user/package; removal is blocked"
        );
        ensure!(
            metadata.dev() == device,
            "Runner contains another mounted filesystem; removal is blocked"
        );
        if metadata.is_dir() {
            let child = open_directory(&entry.path())?;
            ensure!(
                same_inode(&metadata, &child.metadata()?),
                "Runner contents changed during inspection"
            );
            check_tree(&child, depth + 1)?;
        } else {
            ensure!(
                metadata.is_file() || metadata.file_type().is_symlink(),
                "Runner contains a special file; removal is blocked"
            );
        }
    }
    Ok(())
}

// Work through pinned directory FDs. A replacement at an old pathname is never
// traversed; unlinkat(AT_REMOVEDIR) can only remove an empty replacement, not its
// contents. Symlinks inside archives are unlinked, never followed.
fn clear_directory(directory: &File, depth: usize) -> Result<()> {
    ensure!(
        depth <= 128,
        "Runner directory nesting exceeds the removal safety limit"
    );
    let device = directory.metadata()?.dev();
    for entry in fs::read_dir(anchored(directory))? {
        let entry = entry?;
        let name = CString::new(entry.file_name().as_bytes())?;
        let metadata = fs::symlink_metadata(entry.path())?;
        ensure!(
            metadata.uid() == unsafe { libc::geteuid() },
            "Runner ownership changed during removal"
        );
        ensure!(
            metadata.dev() == device,
            "Runner mount changed during removal"
        );
        let flags = if metadata.is_dir() {
            let child = open_directory(&entry.path())?;
            ensure!(
                same_inode(&metadata, &child.metadata()?),
                "Runner contents changed during removal"
            );
            clear_directory(&child, depth + 1)?;
            ensure!(
                same_inode(&metadata, &fs::symlink_metadata(entry.path())?),
                "Runner subdirectory was replaced during removal"
            );
            libc::AT_REMOVEDIR
        } else {
            ensure!(
                metadata.is_file() || metadata.file_type().is_symlink(),
                "Runner contains a special file"
            );
            0
        };
        let result = unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), flags) };
        if result != 0 {
            return Err(std::io::Error::last_os_error())
                .context("Removing quarantined runner contents");
        }
    }
    Ok(())
}

fn quarantine_remove(verified: Verified) -> Result<()> {
    lock_tool(&verified.directory)?;
    check_tree(&verified.directory, 0)?;
    recheck(&verified, &verified.descriptor.path)?;
    // Do not give TempDir ownership of moved data: its Drop would recursively
    // delete even a mismatched/replaced directory on an error path.
    let root_path = anchored(&verified.root_directory);
    let stage = tempfile::Builder::new()
        .prefix(".hyprdeck-remove-")
        .tempdir_in(&root_path)?;
    let stage_path = stage.keep();
    let recovery_path = verified.root.join(
        stage_path
            .file_name()
            .context("Quarantine has no directory name")?,
    );
    let stage_file = open_directory(&stage_path)?;
    let destination = anchored(&stage_file).join("owned-tool");
    if let Err(error) = rename_noreplace(&verified.descriptor.path, &destination) {
        let _ = fs::remove_dir(&stage_path);
        return Err(error);
    }
    if let Err(error) = recheck(&verified, &destination) {
        // Put an unexpected directory back only if that does not overwrite a
        // newly installed runner. Otherwise retain it for manual recovery.
        let restored = rename_noreplace(&destination, &verified.descriptor.path).is_ok();
        if restored {
            let _ = fs::remove_dir(&stage_path);
        }
        bail!(
            "Quarantined runner identity changed; no contents were deleted ({error:#}). {}",
            if restored {
                "Directory restored to its original path".to_owned()
            } else {
                format!("Preserved for recovery in {}", recovery_path.display())
            }
        );
    }
    clear_directory(&verified.directory, 0).with_context(|| {
        format!(
            "Removal stopped; remaining quarantined files are in {}",
            recovery_path.display()
        )
    })?;
    ensure!(
        same_inode(&verified.metadata, &fs::symlink_metadata(&destination)?),
        "Quarantined directory was replaced; it has not been deleted"
    );
    let name = CString::new("owned-tool")?;
    if unsafe { libc::unlinkat(stage_file.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } != 0 {
        return Err(std::io::Error::last_os_error())
            .context("Removing the empty verified runner directory");
    }
    fs::remove_dir(&stage_path)?;
    verified.root_directory.sync_all()?;
    Ok(())
}

/// Never trusts `managed`: validates the actual manifest, ownership and identity.
pub fn remove(target: &Target, tool: &InstalledTool) -> Result<()> {
    require_stopped(target)?;
    let _steam_lock = lock_steam(target)?;
    let verified = verify(target, tool)?;
    ensure_unreferenced(target, &verified.descriptor)?;
    quarantine_remove(verified)
}

#[cfg(test)]
mod tests {
    use super::super::Receipt;
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    const SOURCE: &str = "// upstream comment\n\"compatibilitytools\"\n{\n  \"compat_tools\"\n  {\n    \"fixture-proton-id\"\n    {\n      \"install_path\" \".\"\n      \"display_name\" \"Original name\" // keep this\n      \"from_oslist\" \"windows\"\n      \"unknown_setting\" \"unchanged\"\n    }\n  }\n}\n";

    fn fixture() -> (tempfile::TempDir, Target, InstalledTool) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("tools");
        let path = root.join("fixture-folder");
        fs::create_dir_all(&path).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(path.join(MANIFEST), SOURCE).unwrap();
        fs::set_permissions(path.join(MANIFEST), fs::Permissions::from_mode(0o640)).unwrap();
        fs::write(path.join("proton"), "fixture runner, never executed").unwrap();
        let target = Target {
            id: "fixture-target".into(),
            name: "Fixture".into(),
            root,
            kind: TargetKind::Lutris,
            launcher_root: None,
            steam_packaging: None,
        };
        let tool = describe(&target, &path).unwrap().remove(0);
        (temp, target, tool)
    }

    #[test]
    fn external_manifest_rename_preserves_ids_comments_permissions_and_backup() {
        let (_temp, target, tool) = fixture();
        let capabilities = capabilities(&target, &tool).unwrap();
        assert!(capabilities.editable && capabilities.removable);
        let renamed = rename_verified(
            &target,
            verify(&target, &tool).unwrap(),
            "Friendly \"fixture\" \\ runner",
        )
        .unwrap();
        assert_eq!(renamed.name, tool.name);
        assert_eq!(renamed.path, tool.path);
        assert!(!renamed.managed);
        assert_eq!(renamed.display_name, "Friendly \"fixture\" \\ runner");
        let text = fs::read_to_string(tool.path.join(MANIFEST)).unwrap();
        assert!(text.contains("// upstream comment"));
        assert!(text.contains("// keep this"));
        assert!(text.contains("\"unknown_setting\" \"unchanged\""));
        assert!(text.contains("\"fixture-proton-id\""));
        assert_eq!(
            fs::metadata(tool.path.join(MANIFEST)).unwrap().mode() & 0o777,
            0o640
        );
        let backup = fs::read_dir(&tool.path)
            .unwrap()
            .map(Result::unwrap)
            .find(|entry| {
                entry
                    .file_name()
                    .as_bytes()
                    .starts_with(b".hyprdeck-display-backup-")
            })
            .unwrap();
        assert_eq!(fs::read_to_string(backup.path()).unwrap(), SOURCE);
        assert_eq!(backup.metadata().unwrap().mode() & 0o777, 0o640);
    }

    #[test]
    fn display_edit_preserves_valid_receipt_and_parser_metadata() {
        let (_temp, target, tool) = fixture();
        let receipt = Receipt {
            owner: "Hyprdeck".into(),
            schema: 1,
            target_id: target.id.clone(),
            root: target.root.clone(),
            directory: "fixture-folder".into(),
            tool_id: tool.name.clone(),
            release_id: "fixture-release".into(),
            source: "https://example.invalid/fixture.tar.gz".into(),
        };
        let receipt_bytes = serde_json::to_vec(&receipt).unwrap();
        fs::write(tool.path.join(RECEIPT), &receipt_bytes).unwrap();
        fs::write(
            tool.path.join(crate::proton_repair::RECORD),
            "untouched parser metadata",
        )
        .unwrap();
        let tool = describe(&target, &tool.path).unwrap().remove(0);
        assert!(tool.managed);
        let renamed =
            rename_verified(&target, verify(&target, &tool).unwrap(), "Edited display").unwrap();
        assert!(renamed.managed);
        valid_receipt(&target, &renamed.path, &renamed.name).unwrap();
        assert_eq!(fs::read(tool.path.join(RECEIPT)).unwrap(), receipt_bytes);
        assert_eq!(
            fs::read_to_string(tool.path.join(crate::proton_repair::RECORD)).unwrap(),
            "untouched parser metadata"
        );
    }

    #[test]
    fn standalone_manual_directory_can_be_quarantined_and_removed_without_receipt() {
        let (_temp, target, tool) = fixture();
        let outside = target.root.parent().unwrap().join("outside");
        fs::write(&outside, "must survive").unwrap();
        std::os::unix::fs::symlink(&outside, tool.path.join("upstream-link")).unwrap();
        quarantine_remove(verify(&target, &tool).unwrap()).unwrap();
        assert!(!tool.path.exists());
        assert_eq!(fs::read_to_string(outside).unwrap(), "must survive");
    }

    #[test]
    fn forged_stale_cross_target_shared_and_symlink_descriptors_are_read_only() {
        let (_temp, target, tool) = fixture();
        for forged in [
            InstalledTool {
                name: "forged-id".into(),
                ..tool.clone()
            },
            InstalledTool {
                display_name: "forged display".into(),
                ..tool.clone()
            },
            InstalledTool {
                target_id: "another-target".into(),
                ..tool.clone()
            },
            InstalledTool {
                managed: true,
                ..tool.clone()
            },
        ] {
            assert!(verify(&target, &forged).is_err());
            let capability = capabilities(&target, &forged).unwrap();
            assert!(!capability.editable && !capability.removable && !capability.reason.is_empty());
        }
        let wrong = Target {
            root: target.root.parent().unwrap().to_path_buf(),
            ..target.clone()
        };
        assert!(verify(&wrong, &tool).is_err());
        fs::set_permissions(&tool.path, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(verify(&target, &tool).is_err());
        fs::set_permissions(&tool.path, fs::Permissions::from_mode(0o700)).unwrap();
        let link = target.root.join("symlink-tool");
        std::os::unix::fs::symlink(&tool.path, &link).unwrap();
        assert!(
            verify(
                &target,
                &InstalledTool {
                    path: link,
                    ..tool.clone()
                }
            )
            .is_err()
        );
        let manifest = tool.path.join(MANIFEST);
        fs::rename(&manifest, tool.path.join("real-manifest")).unwrap();
        std::os::unix::fs::symlink("real-manifest", &manifest).unwrap();
        assert!(verify(&target, &tool).is_err());
    }

    #[test]
    fn multi_tool_nested_and_ambiguous_manifests_are_read_only() {
        let (_temp, target, tool) = fixture();
        for source in [
            SOURCE.replace(
                "\"compat_tools\"\n  {",
                "\"compat_tools\"\n  { \"other-id\" { \"install_path\" \".\" }",
            ),
            SOURCE.replace("\"install_path\" \".\"", "\"install_path\" \"../outside\""),
            SOURCE.replace("\"install_path\" \".\"", "\"install_path\" \"runtime\""),
            SOURCE.replace(
                "\"install_path\" \".\"",
                "\"install_path\" \".\" \"INSTALL_PATH\" \"../outside\"",
            ),
            SOURCE.replace(
                "\"install_path\" \".\"",
                "\"install_path\" { \"malformed\" \"value\" }",
            ),
            SOURCE.replace(
                "\"display_name\" \"Original name\"",
                "\"display_name\" \"Original name\" \"DISPLAY_NAME\" \"ambiguous\"",
            ),
            format!("#base \"other.vdf\"\n{SOURCE}"),
        ] {
            fs::write(tool.path.join(MANIFEST), source).unwrap();
            assert!(verify(&target, &tool).is_err());
        }
    }

    #[test]
    fn replaced_directory_and_changed_manifest_are_never_deleted() {
        let (_temp, target, tool) = fixture();
        let verified = verify(&target, &tool).unwrap();
        let original = target.root.join("original-moved");
        fs::rename(&tool.path, &original).unwrap();
        fs::create_dir(&tool.path).unwrap();
        fs::write(tool.path.join("replacement"), "must survive").unwrap();
        assert!(quarantine_remove(verified).is_err());
        assert!(original.join(MANIFEST).exists());
        assert!(tool.path.join("replacement").exists());
        fs::remove_dir_all(&tool.path).unwrap();
        fs::rename(original, &tool.path).unwrap();
        let verified = verify(&target, &tool).unwrap();
        fs::write(
            tool.path.join(MANIFEST),
            SOURCE.replace("Original name", "Changed name"),
        )
        .unwrap();
        assert!(quarantine_remove(verified).is_err());
        assert!(tool.path.join(MANIFEST).exists());
    }

    #[test]
    fn display_names_reject_control_line_and_oversized_input() {
        for name in [
            "",
            " ",
            " leading",
            "trailing ",
            "line\nbreak",
            "line\rbreak",
            "null\0byte",
            "line\u{2028}break",
            "bidi\u{202e}name",
        ] {
            assert!(validate_name(name).is_err(), "accepted {name:?}");
        }
        assert!(validate_name(&"a".repeat(257)).is_err());
        assert!(validate_name("Friendly \"quoted\" \\ fixture {name}").is_ok());
    }

    #[test]
    fn invalid_or_renamed_receipts_never_claim_managed_ownership() {
        let (_temp, target, tool) = fixture();
        let receipt = Receipt {
            owner: "Hyprdeck".into(),
            schema: 1,
            target_id: target.id.clone(),
            root: target.root.clone(),
            directory: "old-directory-name".into(),
            tool_id: tool.name.clone(),
            release_id: "fixture-release".into(),
            source: "https://example.invalid/fixture.tar.gz".into(),
        };
        fs::write(
            tool.path.join(RECEIPT),
            serde_json::to_vec(&receipt).unwrap(),
        )
        .unwrap();
        assert!(valid_receipt(&target, &tool.path, &tool.name).is_err());
        assert!(verify(&target, &tool).is_err());
        assert!(tool.path.join(MANIFEST).exists());
    }

    #[test]
    fn manifestless_owned_wine_runner_is_removable_but_not_editable() {
        let (_temp, target, tool) = fixture();
        fs::remove_file(tool.path.join(MANIFEST)).unwrap();
        let tool = describe(&target, &tool.path).unwrap().remove(0);
        let capability = capabilities(&target, &tool).unwrap();
        assert!(!capability.editable && capability.removable);
        assert!(capability.reason.contains("compatibilitytool.vdf"));
        quarantine_remove(verify(&target, &tool).unwrap()).unwrap();
        assert!(!tool.path.exists());
    }

    #[test]
    fn system_package_and_steam_appmanifest_paths_are_read_only() {
        for path in [
            "/usr/local/fixture",
            "/opt/fixture",
            "/nix/store/fixture",
            "/fixture/Steam/steamapps/common/Proton",
        ] {
            assert!(package_path(Path::new(path)));
        }
        assert!(!package_path(Path::new(
            "/fixture/Steam/compatibilitytools.d"
        )));
        let (temp, target, tool) = fixture();
        let common = temp.path().join("steamapps/common");
        fs::create_dir_all(&common).unwrap();
        let moved = common.join("fixture-proton");
        fs::rename(&tool.path, &moved).unwrap();
        let target = Target {
            root: common,
            ..target
        };
        let error = verify(
            &target,
            &InstalledTool {
                path: moved,
                ..tool
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("Steam appmanifest"));
    }
}
