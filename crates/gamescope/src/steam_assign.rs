//! Installation-wide, previewed Steam compatibility assignments. Prefixes and
//! game content are never written. Backups remain beside the original configs.

use anyhow::{Context, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{CString, OsStr, OsString};
use std::fs::{self, File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::proton::Target;
use crate::vdf_edit::{contains_key, paths_to_key, remove_key, set_string};

const LIMIT: u64 = 16 * 1024 * 1024;
const GLOBAL: &[&str] = &[
    "InstallConfigStore",
    "Software",
    "Valve",
    "Steam",
    "CompatToolMapping",
];
static SERIAL: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub app_id: u32,
    /// An installed internal ID, not the display name. None removes overrides.
    pub tool: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub steam_root: PathBuf,
    /// Exactly the files that would change; these are installation-wide edits.
    pub files: Vec<PathBuf>,
    pub changes: Vec<Change>,
    target: Target,
    root: Arc<File>,
    root_identity: Identity,
    snapshots: Vec<Snapshot>,
    topology: Vec<(PathBuf, Option<Identity>)>,
    edits: Vec<Edit>,
    requested: Vec<Change>,
}

#[derive(Clone, Debug)]
pub struct Outcome {
    /// Complete original files, with their original permission bits. Restore
    /// these while Steam/Wine are stopped; no prefixes/saves are in this list.
    pub backups: Vec<PathBuf>,
    pub changed_games: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    uid: u32,
    gid: u32,
    mode: u32,
}

impl Identity {
    fn of(meta: &Metadata) -> Self {
        Self {
            dev: meta.dev(),
            ino: meta.ino(),
            uid: meta.uid(),
            gid: meta.gid(),
            mode: meta.mode(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Stamp {
    identity: Identity,
    len: u64,
    modified: (i64, i64),
    changed: (i64, i64),
    links: u64,
}

impl Stamp {
    fn of(meta: &Metadata) -> Self {
        Self {
            identity: Identity::of(meta),
            len: meta.len(),
            modified: (meta.mtime(), meta.mtime_nsec()),
            changed: (meta.ctime(), meta.ctime_nsec()),
            links: meta.nlink(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Snapshot {
    relative: PathBuf,
    parent_identity: Identity,
    stamp: Option<Stamp>,
    source: String,
}

#[derive(Clone, Debug)]
struct Edit {
    snapshot: usize,
    output: String,
    games: BTreeSet<u32>,
}

/// Preview only: no runtime check, lock file, backup or other write is made.
pub fn plan(target: &Target, changes: &[Change]) -> Result<Plan> {
    ensure!(
        !changes.is_empty(),
        "Select at least one installed Steam game"
    );
    let mut ids = BTreeSet::new();
    for change in changes {
        ensure!(
            change.app_id != 0,
            "Steam's application-0 default cannot be changed here"
        );
        ensure!(
            ids.insert(change.app_id),
            "Duplicate or conflicting Steam game changes are not allowed"
        );
        ensure!(
            change
                .tool
                .as_ref()
                .is_none_or(|tool| !tool.is_empty() && !tool.contains('\0')),
            "Select an installed compatibility-tool internal ID"
        );
    }
    let steam_root = crate::proton::steam_root(target)?;
    let root = Arc::new(open_root(&steam_root)?);
    let root_identity = Identity::of(&root.metadata()?);
    let (snapshots, topology) = scan(&root)?;
    validate_games_and_tools(target, changes)?;
    // Fail if the configuration topology moved while its manifests were read.
    ensure!(
        snapshot_matches(&root, &snapshots, &topology)?,
        "Steam configurations changed during preview; preview again"
    );
    let mut edits = Vec::new();
    for (index, snapshot) in snapshots.iter().enumerate() {
        let global = snapshot.relative == Path::new("config/config.vdf");
        let mut output = snapshot.source.clone();
        let mut maps = paths_to_key(&output, "CompatToolMapping")?;
        validate_application_keys(&output, &maps, changes)?;
        if global && !maps.iter().any(|path| same_path(path, GLOBAL)) {
            maps.push(GLOBAL.iter().map(|key| (*key).to_owned()).collect());
        }
        let mut games = BTreeSet::new();
        for change in changes {
            let mut changed = false;
            let app = change.app_id.to_string();
            for mapping in &maps {
                let mut path: Vec<_> = mapping.iter().map(String::as_str).collect();
                path.push(&app);
                let exists = contains_key(&output, &path)?;
                match &change.tool {
                    Some(tool) if (global && same_path(mapping, GLOBAL)) || exists => {
                        for (key, value) in
                            [("name", tool.as_str()), ("config", ""), ("priority", "250")]
                        {
                            path.push(key);
                            let updated = set_string(&output, &path, value)?;
                            changed |= updated != output;
                            output = updated;
                            path.pop();
                        }
                    }
                    None if exists => {
                        let updated = remove_key(&output, &path)?;
                        changed |= updated != output;
                        output = updated;
                    }
                    _ => {}
                }
            }
            if changed {
                games.insert(change.app_id);
            }
        }
        if output != snapshot.source {
            edits.push(Edit {
                snapshot: index,
                output,
                games,
            });
        }
    }
    let files = edits
        .iter()
        .map(|edit| steam_root.join(&snapshots[edit.snapshot].relative))
        .collect();
    Ok(Plan {
        steam_root,
        files,
        changes: changes.to_vec(),
        target: target.clone(),
        root,
        root_identity,
        snapshots,
        topology,
        edits,
        requested: changes.to_vec(),
    })
}

fn same_path(path: &[String], expected: &[&str]) -> bool {
    path.len() == expected.len()
        && path
            .iter()
            .zip(expected)
            .all(|(a, b)| a.eq_ignore_ascii_case(b))
}

fn validate_application_keys(
    source: &str,
    mappings: &[Vec<String>],
    changes: &[Change],
) -> Result<()> {
    let document = crate::steam::parse_vdf(source)?;
    for mapping in mappings {
        let mut value = &document;
        for key in mapping {
            value = value
                .get(key)
                .context("Steam compatibility mapping disappeared")?;
        }
        for (key, _) in value
            .entries()
            .context("Malformed Steam compatibility mapping")?
        {
            if let Ok(id) = key.parse::<u32>()
                && changes.iter().any(|change| change.app_id == id)
            {
                ensure!(
                    *key == id.to_string(),
                    "A selected Steam game has a non-canonical or ambiguous application-ID key"
                );
            }
        }
    }
    Ok(())
}

fn validate_games_and_tools(target: &Target, changes: &[Change]) -> Result<()> {
    let inventory = crate::steam::inventory(std::slice::from_ref(target))?;
    ensure!(
        inventory.warnings.is_empty(),
        "Cannot safely edit an incomplete Steam inventory: {}",
        inventory.warnings.join("; ")
    );
    let mut games = BTreeMap::<u32, usize>::new();
    for game in &inventory.games {
        *games.entry(game.app_id).or_default() += 1;
    }
    let mut tools = BTreeMap::<String, BTreeSet<PathBuf>>::new();
    for tool in crate::proton::installed(target)? {
        tools
            .entry(tool.name)
            .or_default()
            .insert(fs::canonicalize(tool.path)?);
    }
    for tool in inventory.steam_tools {
        if !tool.internal_name.is_empty() && tool.path.is_dir() {
            tools
                .entry(tool.internal_name)
                .or_default()
                .insert(fs::canonicalize(tool.path)?);
        }
    }
    for change in changes {
        let installed = games.get(&change.app_id) == Some(&1);
        let saved_unassignment = change.tool.is_none()
            && inventory
                .mappings
                .iter()
                .any(|mapping| mapping.app_id == change.app_id);
        ensure!(
            installed || saved_unassignment,
            "Application {} is not an unambiguous installed Steam game or an existing override to remove",
            change.app_id
        );
        if let Some(tool) = &change.tool {
            ensure!(
                tools.get(tool).is_some_and(|paths| paths.len() == 1),
                "The selected compatibility-tool internal ID is not unambiguously installed for this Steam owner"
            );
        }
    }
    Ok(())
}

/// Apply only the exact preview. Steam and all Wine/Proton processes must be
/// stopped by the user; this function never shuts them down automatically.
pub fn apply(plan: &Plan) -> Result<Outcome> {
    apply_with(
        plan,
        &mut || crate::proton::require_stopped(&plan.target),
        &mut |_| Ok(()),
    )
}

fn apply_with(
    plan: &Plan,
    stopped: &mut dyn FnMut() -> Result<()>,
    before_write: &mut dyn FnMut(usize) -> Result<()>,
) -> Result<Outcome> {
    ensure!(
        plan.changes == plan.requested,
        "The assignment preview was modified; preview again"
    );
    ensure!(
        plan.steam_root == crate::proton::steam_root(&plan.target)?,
        "The Steam owner changed; preview again"
    );
    let expected_files: Vec<_> = plan
        .edits
        .iter()
        .map(|edit| {
            plan.steam_root
                .join(&plan.snapshots[edit.snapshot].relative)
        })
        .collect();
    ensure!(
        plan.files == expected_files,
        "The assignment file preview was modified; preview again"
    );
    let current_root = open_root(&plan.steam_root)?;
    ensure!(
        Identity::of(&current_root.metadata()?) == plan.root_identity,
        "The Steam installation directory changed; preview again"
    );
    let _lock = acquire_lock(&plan.root)?;
    stopped()?;
    ensure!(
        snapshot_matches(&plan.root, &plan.snapshots, &plan.topology)?,
        "Steam configuration changed since preview; preview again"
    );
    validate_games_and_tools(&plan.target, &plan.requested)?;
    let mut staged = Vec::new();
    let mut backups = Vec::new();
    let prepare = (|| -> Result<()> {
        for edit in &plan.edits {
            let snapshot = &plan.snapshots[edit.snapshot];
            let parent = open_relative_dir(&plan.root, snapshot.relative.parent().unwrap())?;
            ensure!(
                Identity::of(&parent.metadata()?) == snapshot.parent_identity,
                "A Steam configuration directory changed since preview"
            );
            let name = snapshot.relative.file_name().unwrap().to_owned();
            ensure_snapshot(&parent, &name, snapshot)?;
            let mode = snapshot
                .stamp
                .as_ref()
                .map_or(0o600, |stamp| stamp.identity.mode & 0o777);
            let (temporary, mut file) = create_unique(&parent, &name, "staging")?;
            let mut item = Staged {
                parent,
                name,
                temporary,
                replacement: None,
                original: snapshot,
                expected: &edit.output,
                applied: false,
            };
            let write_result = (|| -> Result<()> {
                preserve_group(&file, snapshot.stamp.as_ref())?;
                file.write_all(edit.output.as_bytes())?;
                file.set_permissions(fs::Permissions::from_mode(mode))?;
                file.sync_all()?;
                item.replacement = Some(Identity::of(&file.metadata()?));
                if snapshot.stamp.is_some() {
                    let (backup, mut copy) = create_unique(&item.parent, &item.name, "backup")?;
                    let backup_path = plan
                        .steam_root
                        .join(snapshot.relative.parent().unwrap())
                        .join(&backup);
                    let copied = (|| -> Result<()> {
                        preserve_group(&copy, snapshot.stamp.as_ref())?;
                        copy.write_all(snapshot.source.as_bytes())?;
                        copy.set_permissions(fs::Permissions::from_mode(mode))?;
                        copy.sync_all()?;
                        item.parent.sync_all()?;
                        Ok(())
                    })();
                    if let Err(error) = copied {
                        unlink_at(&item.parent, &backup)?;
                        return Err(
                            error.context("Could not complete an original-configuration backup")
                        );
                    }
                    backups.push(backup_path);
                }
                Ok(())
            })();
            staged.push(item);
            write_result?;
        }
        Ok(())
    })();
    if let Err(error) = prepare {
        cleanup_staging(&staged, false);
        return Err(error.context(format!(
            "No assignments were changed. {}",
            backup_message(&backups)
        )));
    }
    let result = (|| -> Result<()> {
        // Backups/temp files intentionally do not affect directory identities.
        ensure!(
            snapshot_matches(&plan.root, &plan.snapshots, &plan.topology)?,
            "Steam configuration changed while preparing backups; preview again"
        );
        stopped()?;
        for (index, item) in staged.iter_mut().enumerate() {
            before_write(index)?;
            stopped()?;
            ensure!(
                Identity::of(&open_root(&plan.steam_root)?.metadata()?) == plan.root_identity,
                "The Steam installation directory changed before writing"
            );
            let current_parent =
                open_relative_dir(&plan.root, item.original.relative.parent().unwrap())?;
            ensure!(
                Identity::of(&current_parent.metadata()?) == item.original.parent_identity,
                "A Steam configuration directory changed before writing"
            );
            ensure_snapshot(&item.parent, &item.name, item.original)?;
            if item.original.stamp.is_some() {
                rename_at(
                    &item.parent,
                    &item.temporary,
                    &item.name,
                    libc::RENAME_EXCHANGE,
                )?;
                item.applied = true;
                // Exchange never follows the destination. Inspect what was
                // displaced to close the validation-to-rename race.
                let displaced = read_snapshot(
                    &item.parent,
                    &item.temporary,
                    item.original.relative.clone(),
                )?;
                ensure!(
                    same_contents(&displaced, item.original),
                    "A Steam configuration raced the atomic replacement"
                );
            } else {
                rename_at(
                    &item.parent,
                    &item.temporary,
                    &item.name,
                    libc::RENAME_NOREPLACE,
                )?;
                item.applied = true;
            }
            item.parent.sync_all()?;
        }
        stopped()?;
        ensure!(
            Identity::of(&open_root(&plan.steam_root)?.metadata()?) == plan.root_identity,
            "The Steam installation directory changed during writing"
        );
        let (current, topology) = scan(&plan.root)?;
        ensure!(
            topology == plan.topology && current.len() == plan.snapshots.len(),
            "Steam user configuration topology changed during writing"
        );
        for (index, snapshot) in current.iter().enumerate() {
            if let Some(edit) = plan.edits.iter().find(|edit| edit.snapshot == index) {
                let item = staged
                    .iter()
                    .find(|item| item.original.relative == snapshot.relative)
                    .unwrap();
                ensure!(
                    snapshot.source == edit.output
                        && snapshot.stamp.as_ref().map(|stamp| &stamp.identity)
                            == item.replacement.as_ref(),
                    "Steam configuration changed during writing"
                );
            } else {
                ensure!(
                    *snapshot == plan.snapshots[index],
                    "An unrelated Steam configuration changed during writing"
                );
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        let rollback = rollback(&mut staged);
        if let Err(restore_error) = rollback {
            let originals: Vec<_> = staged
                .iter()
                .filter(|item| item.applied && item.original.stamp.is_some())
                .map(|item| {
                    plan.steam_root
                        .join(item.original.relative.parent().unwrap())
                        .join(&item.temporary)
                        .display()
                        .to_string()
                })
                .collect();
            cleanup_staging(&staged, false);
            bail!(
                "Assignment failed: {error:#}. Automatic restoration was incomplete: {restore_error:#}. Do not start Steam; original staging files: {}. {}",
                originals.join(", "),
                backup_message(&backups)
            );
        }
        cleanup_staging(&staged, false);
        return Err(error.context(format!("No assignment changes remain; all configurations written by Hyprdeck were restored. {}", backup_message(&backups))));
    }
    cleanup_staging(&staged, true);
    let changed_games = plan
        .edits
        .iter()
        .flat_map(|edit| edit.games.iter().copied())
        .collect::<BTreeSet<_>>()
        .len();
    Ok(Outcome {
        backups,
        changed_games,
    })
}

fn backup_message(backups: &[PathBuf]) -> String {
    if backups.is_empty() {
        "No existing configuration required a backup.".to_owned()
    } else {
        format!(
            "Original configuration backups (restore only while Steam/Wine are stopped): {}.",
            backups
                .iter()
                .map(|path| path.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        )
    }
}

struct Staged<'a> {
    parent: File,
    name: OsString,
    temporary: OsString,
    replacement: Option<Identity>,
    original: &'a Snapshot,
    expected: &'a str,
    applied: bool,
}

fn rollback(staged: &mut [Staged<'_>]) -> Result<()> {
    let mut errors = Vec::new();
    for item in staged.iter_mut().rev().filter(|item| item.applied) {
        let restored = (|| -> Result<()> {
            let current = read_snapshot(&item.parent, &item.name, item.original.relative.clone())?;
            ensure!(
                current.source == item.expected
                    && current.stamp.as_ref().map(|stamp| &stamp.identity)
                        == item.replacement.as_ref(),
                "Replacement changed before restoration; refusing to overwrite it"
            );
            if item.original.stamp.is_some() {
                let original = read_snapshot(
                    &item.parent,
                    &item.temporary,
                    item.original.relative.clone(),
                )?;
                ensure!(
                    same_contents(&original, item.original),
                    "Original staging file changed before restoration"
                );
                rename_at(
                    &item.parent,
                    &item.temporary,
                    &item.name,
                    libc::RENAME_EXCHANGE,
                )?;
            } else {
                unlink_at(&item.parent, &item.name)?;
            }
            item.applied = false;
            item.parent.sync_all()?;
            Ok(())
        })();
        if let Err(error) = restored {
            errors.push(format!("{}: {error:#}", item.original.relative.display()));
        }
    }
    ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

fn cleanup_staging(staged: &[Staged<'_>], committed: bool) {
    for item in staged
        .iter()
        .filter(|item| !item.applied || (committed && item.original.stamp.is_some()))
    {
        if let Err(error) = unlink_at(&item.parent, &item.temporary) {
            tracing::warn!("Cannot remove Steam assignment staging file: {error:#}");
        }
    }
}

fn same_contents(a: &Snapshot, b: &Snapshot) -> bool {
    // rename changes ctime, so compare the original inode, metadata and bytes,
    // but not ctime after the exchange itself.
    match (&a.stamp, &b.stamp) {
        (Some(a_stamp), Some(b_stamp)) => {
            a.source == b.source
                && a_stamp.identity == b_stamp.identity
                && a_stamp.len == b_stamp.len
                && a_stamp.modified == b_stamp.modified
                && a_stamp.links == b_stamp.links
        }
        (None, None) => true,
        _ => false,
    }
}

fn ensure_snapshot(parent: &File, name: &OsStr, expected: &Snapshot) -> Result<()> {
    let current = read_snapshot(parent, name, expected.relative.clone())?;
    ensure!(
        current == *expected,
        "Steam configuration changed since preview; preview again"
    );
    Ok(())
}

fn snapshot_matches(
    root: &File,
    snapshots: &[Snapshot],
    topology: &[(PathBuf, Option<Identity>)],
) -> Result<bool> {
    let (current, current_topology) = scan(root)?;
    Ok(current.as_slice() == snapshots && current_topology.as_slice() == topology)
}

type Topology = Vec<(PathBuf, Option<Identity>)>;

fn scan(root: &File) -> Result<(Vec<Snapshot>, Topology)> {
    let config =
        open_dir_at(root, OsStr::new("config"))?.context("Steam's config directory is missing")?;
    let mut topology = vec![(
        PathBuf::from("config"),
        Some(Identity::of(&config.metadata()?)),
    )];
    let mut snapshots = vec![read_snapshot(
        &config,
        OsStr::new("config.vdf"),
        PathBuf::from("config/config.vdf"),
    )?];
    let userdata = open_dir_at(root, OsStr::new("userdata"))?;
    topology.push((
        PathBuf::from("userdata"),
        userdata
            .as_ref()
            .map(|dir| dir.metadata().map(|meta| Identity::of(&meta)))
            .transpose()?,
    ));
    if let Some(userdata) = userdata {
        let mut users = Vec::new();
        for entry in fs::read_dir(fd_path(&userdata))? {
            let entry = entry?;
            let name = entry.file_name();
            if !name.as_bytes().is_empty() && name.as_bytes().iter().all(u8::is_ascii_digit) {
                users.push(name);
            }
        }
        users.sort();
        for name in users {
            let user =
                open_dir_at(&userdata, &name)?.context("Steam user directory disappeared")?;
            let relative = PathBuf::from("userdata").join(&name);
            topology.push((relative.clone(), Some(Identity::of(&user.metadata()?))));
            let config = open_dir_at(&user, OsStr::new("config"))?;
            topology.push((
                relative.join("config"),
                config
                    .as_ref()
                    .map(|dir| dir.metadata().map(|meta| Identity::of(&meta)))
                    .transpose()?,
            ));
            if let Some(config) = config {
                snapshots.push(read_snapshot(
                    &config,
                    OsStr::new("localconfig.vdf"),
                    relative.join("config/localconfig.vdf"),
                )?);
            }
        }
    }
    Ok((snapshots, topology))
}

fn read_snapshot(parent: &File, name: &OsStr, relative: PathBuf) -> Result<Snapshot> {
    let parent_identity = Identity::of(&parent.metadata()?);
    let Some(file) = open_file_at(parent, name)? else {
        return Ok(Snapshot {
            relative,
            parent_identity,
            stamp: None,
            source: String::new(),
        });
    };
    let before = file.metadata()?;
    ensure!(
        before.is_file()
            && before.uid() == unsafe { libc::geteuid() }
            && before.nlink() == 1
            && before.mode() & 0o7022 == 0,
        "Steam configuration must be an owned, non-shared regular file without group/world write access"
    );
    ensure!(before.len() <= LIMIT, "Steam configuration exceeds 16 MiB");
    let mut source = String::new();
    (&file)
        .take(LIMIT + 1)
        .read_to_string(&mut source)
        .context("Cannot read Steam configuration text")?;
    ensure!(
        source.len() as u64 <= LIMIT && Stamp::of(&before) == Stamp::of(&file.metadata()?),
        "Steam configuration changed while being read"
    );
    // Validate even configurations that need no edits. Directives may pull
    // assignments in from other files and must never be silently ignored.
    paths_to_key(&source, "CompatToolMapping")?;
    Ok(Snapshot {
        relative,
        parent_identity,
        stamp: Some(Stamp::of(&before)),
        source,
    })
}

fn open_root(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    validate_dir(&file)?;
    Ok(file)
}

fn validate_dir(file: &File) -> Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o022 == 0,
        "Steam configuration directories must be user-owned and not group/world writable"
    );
    Ok(())
}

fn open_relative_dir(root: &File, relative: &Path) -> Result<File> {
    let mut current = root.try_clone()?;
    for part in relative.components() {
        let Component::Normal(name) = part else {
            bail!("Invalid Steam configuration path")
        };
        current =
            open_dir_at(&current, name)?.context("Steam configuration directory disappeared")?;
    }
    Ok(current)
}

fn open_dir_at(parent: &File, name: &OsStr) -> Result<Option<File>> {
    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error)
            .context("Cannot safely open Steam configuration directory (symlinks are refused)");
    }
    let file = unsafe { File::from_raw_fd(fd) };
    validate_dir(&file)?;
    Ok(Some(file))
}

fn open_file_at(parent: &File, name: &OsStr) -> Result<Option<File>> {
    let name = c_name(name)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(None);
        }
        return Err(error).context("Cannot safely open Steam configuration (symlinks are refused)");
    }
    Ok(Some(unsafe { File::from_raw_fd(fd) }))
}

fn acquire_lock(root: &File) -> Result<File> {
    let name = c_name(OsStr::new(".hyprdeck-compat.lock"))?;
    let fd = unsafe {
        libc::openat(
            root.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            0o600,
        )
    };
    ensure!(
        fd >= 0,
        "Cannot safely open the Steam compatibility-operation lock: {}",
        std::io::Error::last_os_error()
    );
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    ensure!(
        meta.is_file()
            && meta.uid() == unsafe { libc::geteuid() }
            && meta.nlink() == 1
            && meta.mode() & 0o077 == 0,
        "Unsafe Steam compatibility-operation lock"
    );
    ensure!(
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0,
        "Another Steam compatibility operation is running; try again when it finishes"
    );
    Ok(file)
}

/// Hold this guard across Steam mapping validation and tool mutations as well
/// as assignment writes. Dropping the file releases the advisory lock.
pub(crate) fn lock_steam_root(root: &Path) -> Result<File> {
    acquire_lock(&open_root(root)?)
}

fn preserve_group(file: &File, original: Option<&Stamp>) -> Result<()> {
    if let Some(original) = original
        && file.metadata()?.gid() != original.identity.gid
    {
        ensure!(
            unsafe { libc::fchown(file.as_raw_fd(), !0, original.identity.gid) } == 0,
            "Cannot preserve Steam configuration group ownership: {}",
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

fn create_unique(parent: &File, original: &OsStr, kind: &str) -> Result<(OsString, File)> {
    for _ in 0..100 {
        let serial = SERIAL.fetch_add(1, Ordering::Relaxed);
        let mut name = original.to_owned();
        name.push(format!(".hyprdeck-{kind}-{}-{serial}", std::process::id()));
        let c = c_name(&name)?;
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                c.as_ptr(),
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                0o600 as libc::mode_t,
            )
        };
        if fd >= 0 {
            return Ok((name, unsafe { File::from_raw_fd(fd) }));
        }
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(error).context("Cannot create Steam configuration backup/staging file");
        }
    }
    bail!("Cannot allocate a unique Steam configuration backup/staging filename")
}

fn rename_at(
    parent: &File,
    source: &OsStr,
    destination: &OsStr,
    flags: libc::c_uint,
) -> Result<()> {
    let source = c_name(source)?;
    let destination = c_name(destination)?;
    let result = unsafe {
        libc::syscall(
            libc::SYS_renameat2,
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            flags,
        )
    };
    ensure!(
        result == 0,
        "Cannot atomically replace/restore Steam configuration: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

fn unlink_at(parent: &File, name: &OsStr) -> Result<()> {
    let name = c_name(name)?;
    let result = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), 0) };
    if result != 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
    }
    Ok(())
}

fn c_name(name: &OsStr) -> Result<CString> {
    ensure!(
        !name.as_bytes().is_empty()
            && !name.as_bytes().contains(&b'/')
            && name != "."
            && name != "..",
        "Unsafe Steam configuration filename"
    );
    CString::new(name.as_bytes()).map_err(|_| anyhow!("Invalid Steam configuration filename"))
}

fn fd_path(file: &File) -> PathBuf {
    PathBuf::from(format!("/proc/self/fd/{}", file.as_raw_fd()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proton::TargetKind;
    use std::os::unix::fs::symlink;

    struct Fixture {
        _temp: tempfile::TempDir,
        root: PathBuf,
        target: Target,
    }
    impl Fixture {
        fn new() -> Self {
            let temp = tempfile::tempdir().unwrap();
            let root = temp.path().join("Steam");
            for dir in [
                "config",
                "steamapps",
                "compatibilitytools.d/custom",
                "userdata/100/config",
                "userdata/200/config",
            ] {
                fs::create_dir_all(root.join(dir)).unwrap();
            }
            fs::write(root.join("compatibilitytools.d/custom/compatibilitytool.vdf"), r#""compatibilitytools" { "compat_tools" { "custom\"\\id" { "display_name" "Custom" "install_path" "." } } }"#).unwrap();
            for id in [42, 43] {
                fs::write(root.join(format!("steamapps/appmanifest_{id}.acf")), format!(r#""AppState" {{ "appid" "{id}" "name" "Game {id}" "installdir" "game-{id}" }}"#)).unwrap();
            }
            fs::write(
                root.join("config/config.vdf"),
                r#"// global comment
"InstallConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" {
"0" { "name" "default-unchanged" "priority" "75" }
"42" { "name" "old" "config" "old-config" "priority" "100" }
"43" { "name" "unrelated-game" }
} "Unrelated" "preserved" } } } }"#,
            )
            .unwrap();
            fs::write(root.join("userdata/100/config/localconfig.vdf"), r#""UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" { "42" { "name" "different" "priority" "100" } "0" { "name" "local-default" } } "apps" { "42" { "LaunchOptions" "--preserve-me" } } } } } } // keep"#).unwrap();
            fs::write(root.join("userdata/200/config/localconfig.vdf"), r#""UserLocalConfigStore" { "Software" { "Valve" { "Steam" { "apps" { "42" { "LaunchOptions" "--other-user" } } } } } }"#).unwrap();
            let target = Target {
                id: "assignment-fixture".into(),
                name: "Fixture".into(),
                root: root.join("compatibilitytools.d"),
                kind: TargetKind::Steam,
                launcher_root: Some(root.clone()),
                steam_packaging: None,
            };
            Self {
                _temp: temp,
                root,
                target,
            }
        }
        fn assignment(&self) -> Change {
            Change {
                app_id: 42,
                tool: Some("custom\"\\id".into()),
            }
        }
        fn apply(&self, plan: &Plan) -> Result<Outcome> {
            apply_with(plan, &mut || Ok(()), &mut |_| Ok(()))
        }
    }

    #[test]
    fn assignment_harmonizes_sources_and_backups_restore_exact_bytes_and_modes() {
        let f = Fixture::new();
        let global = f.root.join("config/config.vdf");
        fs::set_permissions(&global, fs::Permissions::from_mode(0o640)).unwrap();
        let original = fs::read(&global).unwrap();
        let local = f.root.join("userdata/100/config/localconfig.vdf");
        let local_original = fs::read(&local).unwrap();
        let untouched = f.root.join("userdata/200/config/localconfig.vdf");
        let untouched_original = fs::read(&untouched).unwrap();
        let plan = plan(&f.target, &[f.assignment()]).unwrap();
        assert_eq!(plan.files, vec![global.clone(), local.clone()]);
        assert!(!f.root.join(".hyprdeck-compat.lock").exists());
        let result = f.apply(&plan).unwrap();
        assert_eq!(result.changed_games, 1);
        assert_eq!(result.backups.len(), 2);
        assert_eq!(fs::read(&result.backups[0]).unwrap(), original);
        assert_eq!(
            fs::metadata(&result.backups[0]).unwrap().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::metadata(&global).unwrap().mode() & 0o777, 0o640);
        assert_eq!(
            fs::metadata(&result.backups[0]).unwrap().gid(),
            fs::metadata(&global).unwrap().gid()
        );
        let text = fs::read_to_string(&global).unwrap();
        assert!(text.contains(r#""0" { "name" "default-unchanged" "priority" "75" }"#));
        assert!(text.contains(r#""43" { "name" "unrelated-game" }"#));
        assert!(text.contains(r#""config" """#));
        assert!(text.contains(r#""priority" "250""#));
        assert!(
            fs::read_to_string(&local)
                .unwrap()
                .contains(r#""LaunchOptions" "--preserve-me""#)
        );
        assert_eq!(fs::read(&untouched).unwrap(), untouched_original);
        // Restore the actual returned backups, not a reserialized substitute.
        fs::copy(&result.backups[0], &global).unwrap();
        fs::copy(&result.backups[1], &local).unwrap();
        assert_eq!(fs::read(&global).unwrap(), original);
        assert_eq!(fs::metadata(&global).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read(&local).unwrap(), local_original);
    }

    #[test]
    fn unassignment_removes_every_override_but_never_defaults_or_launch_options() {
        let f = Fixture::new();
        let outcome = f
            .apply(
                &plan(
                    &f.target,
                    &[Change {
                        app_id: 42,
                        tool: None,
                    }],
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(outcome.changed_games, 1);
        for relative in ["config/config.vdf", "userdata/100/config/localconfig.vdf"] {
            let text = fs::read_to_string(f.root.join(relative)).unwrap();
            for mapping in paths_to_key(&text, "CompatToolMapping").unwrap() {
                let mut path: Vec<_> = mapping.iter().map(String::as_str).collect();
                path.push("42");
                assert!(!contains_key(&text, &path).unwrap());
                path.pop();
                path.push("0");
                assert!(contains_key(&text, &path).unwrap());
            }
        }
        assert!(
            fs::read_to_string(f.root.join("userdata/100/config/localconfig.vdf"))
                .unwrap()
                .contains("--preserve-me")
        );
        assert_eq!(
            f.apply(
                &plan(
                    &f.target,
                    &[Change {
                        app_id: 42,
                        tool: None
                    }]
                )
                .unwrap()
            )
            .unwrap()
            .changed_games,
            0
        );
    }

    #[test]
    fn existing_uninstalled_game_overrides_can_be_removed_but_not_reassigned() {
        let f = Fixture::new();
        fs::remove_file(f.root.join("steamapps/appmanifest_42.acf")).unwrap();
        assert!(plan(&f.target, &[f.assignment()]).is_err());
        let preview = plan(
            &f.target,
            &[Change {
                app_id: 42,
                tool: None,
            }],
        )
        .unwrap();
        assert_eq!(f.apply(&preview).unwrap().changed_games, 1);
        let inventory = crate::steam::inventory(std::slice::from_ref(&f.target)).unwrap();
        assert!(
            !inventory
                .mappings
                .iter()
                .any(|mapping| mapping.app_id == 42)
        );
        assert!(inventory.mappings.iter().any(|mapping| mapping.app_id == 0));
    }

    #[test]
    fn rejects_unknown_apps_tools_defaults_duplicates_and_ambiguous_keys() {
        let f = Fixture::new();
        for change in [
            Change {
                app_id: 0,
                tool: None,
            },
            Change {
                app_id: 999,
                tool: None,
            },
            Change {
                app_id: 42,
                tool: Some("display-name-not-id".into()),
            },
        ] {
            assert!(plan(&f.target, &[change]).is_err());
        }
        assert!(plan(&f.target, &[f.assignment(), f.assignment()]).is_err());
        let global = f.root.join("config/config.vdf");
        let original = fs::read_to_string(&global).unwrap();
        fs::write(&global, original.replace("\"42\" {", "\"42\" {} \"42\" {")).unwrap();
        assert!(plan(&f.target, &[f.assignment()]).is_err());
        for malformed in [
            original.replace("\"42\" {", "\"042\" {"),
            original.replace("\"name\" \"old\"", "\"name\" \"old\" \"NAME\" \"other\""),
            format!("#include \"external-private-file.vdf\"\n{original}"),
        ] {
            fs::write(&global, malformed).unwrap();
            assert!(plan(&f.target, &[f.assignment()]).is_err());
        }
    }

    #[test]
    fn preview_snapshot_runtime_and_topology_races_refuse_without_writes() {
        let f = Fixture::new();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let global = f.root.join("config/config.vdf");
        let modified = format!(
            "{}\n// external change",
            fs::read_to_string(&global).unwrap()
        );
        fs::write(&global, &modified).unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read_to_string(&global).unwrap(), modified);
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        assert!(
            apply_with(
                &preview,
                &mut || bail!("synthetic Steam runtime"),
                &mut |_| Ok(())
            )
            .is_err()
        );
        fs::create_dir_all(f.root.join("userdata/300/config")).unwrap();
        fs::write(
            f.root.join("userdata/300/config/localconfig.vdf"),
            "root {}",
        )
        .unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read_to_string(&global).unwrap(), modified);
    }

    #[test]
    fn symlinks_hardlinks_and_shared_permissions_are_refused() {
        let f = Fixture::new();
        let global = f.root.join("config/config.vdf");
        let outside = f._temp.path().join("outside.vdf");
        fs::rename(&global, &outside).unwrap();
        symlink(&outside, &global).unwrap();
        assert!(plan(&f.target, &[f.assignment()]).is_err());
        fs::remove_file(&global).unwrap();
        fs::hard_link(&outside, &global).unwrap();
        assert!(plan(&f.target, &[f.assignment()]).is_err());
        fs::remove_file(&global).unwrap();
        fs::rename(&outside, &global).unwrap();
        fs::set_permissions(&global, fs::Permissions::from_mode(0o666)).unwrap();
        assert!(plan(&f.target, &[f.assignment()]).is_err());
    }

    #[test]
    fn multi_file_failure_restores_all_originals_and_preserves_backups() {
        let f = Fixture::new();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let originals: Vec<_> = preview
            .files
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let error = apply_with(&preview, &mut || Ok(()), &mut |index| {
            if index == 1 {
                bail!("synthetic second-file failure")
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("all configurations written by Hyprdeck were restored")
        );
        for (path, original) in preview.files.iter().zip(originals) {
            assert_eq!(fs::read(path).unwrap(), original);
        }
        let backups: Vec<_> = fs::read_dir(f.root.join("config"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .contains(".hyprdeck-backup-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
        assert_eq!(
            fs::read(&backups[0]).unwrap(),
            fs::read(&preview.files[0]).unwrap()
        );
    }

    #[test]
    fn steam_managed_ids_are_valid_but_tool_app_manifests_are_not_games() {
        let f = Fixture::new();
        let tool = f.root.join("steamapps/common/managed");
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("compatibilitytool.vdf"), r#""compatibilitytools" { "compat_tools" { "managed_internal" { "display_name" "Managed tool" "install_path" "." } } }"#).unwrap();
        fs::write(
            f.root.join("steamapps/appmanifest_1000.acf"),
            r#""AppState" { "appid" "1000" "name" "Managed tool" "installdir" "managed" }"#,
        )
        .unwrap();
        assert!(
            plan(
                &f.target,
                &[Change {
                    app_id: 42,
                    tool: Some("managed_internal".into())
                }]
            )
            .is_ok()
        );
        assert!(
            plan(
                &f.target,
                &[Change {
                    app_id: 1000,
                    tool: None
                }]
            )
            .is_err()
        );
    }

    #[test]
    fn mixed_batch_assigns_and_unassigns_without_touching_global_defaults() {
        let f = Fixture::new();
        let changes = [
            f.assignment(),
            Change {
                app_id: 43,
                tool: None,
            },
        ];
        let outcome = f.apply(&plan(&f.target, &changes).unwrap()).unwrap();
        assert_eq!(outcome.changed_games, 2);
        let text = fs::read_to_string(f.root.join("config/config.vdf")).unwrap();
        let mut path = GLOBAL.to_vec();
        path.push("43");
        assert!(!contains_key(&text, &path).unwrap());
        assert!(text.contains(r#""0" { "name" "default-unchanged" "priority" "75" }"#));
        let inventory = crate::steam::inventory(std::slice::from_ref(&f.target)).unwrap();
        let game = inventory
            .games
            .iter()
            .find(|game| game.app_id == 42)
            .unwrap();
        assert_eq!(game.tool.as_deref(), Some("custom\"\\id"));
    }

    #[test]
    fn absent_global_file_creation_is_atomic_and_rolled_back_to_absence() {
        let f = Fixture::new();
        let global = f.root.join("config/config.vdf");
        fs::remove_file(&global).unwrap();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let local = f.root.join("userdata/100/config/localconfig.vdf");
        let original = fs::read(&local).unwrap();
        assert!(
            apply_with(&preview, &mut || Ok(()), &mut |index| {
                if index == 1 {
                    bail!("synthetic failure after creating global config")
                }
                Ok(())
            })
            .is_err()
        );
        assert!(!global.exists());
        assert_eq!(fs::read(&local).unwrap(), original);
        let outcome = f
            .apply(&plan(&f.target, &[f.assignment()]).unwrap())
            .unwrap();
        assert_eq!(outcome.backups.len(), 1);
        assert!(contains_key(&fs::read_to_string(global).unwrap(), GLOBAL).unwrap());
    }

    #[test]
    fn directory_symlinks_and_post_preview_file_substitution_are_refused() {
        let f = Fixture::new();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let global = f.root.join("config/config.vdf");
        let original = fs::read(&global).unwrap();
        let outside = f._temp.path().join("outside.vdf");
        fs::write(&outside, &original).unwrap();
        fs::remove_file(&global).unwrap();
        symlink(&outside, &global).unwrap();
        assert!(f.apply(&preview).is_err());
        assert_eq!(fs::read(&outside).unwrap(), original);
        fs::remove_file(&global).unwrap();
        fs::write(&global, &original).unwrap();
        fs::rename(f.root.join("config"), f.root.join("real-config")).unwrap();
        symlink(f.root.join("real-config"), f.root.join("config")).unwrap();
        assert!(plan(&f.target, &[f.assignment()]).is_err());
    }

    #[test]
    fn runtime_recheck_after_first_write_rolls_it_back() {
        let f = Fixture::new();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let originals: Vec<_> = preview
            .files
            .iter()
            .map(|path| fs::read(path).unwrap())
            .collect();
        let mut calls = 0;
        let result = apply_with(
            &preview,
            &mut || {
                calls += 1;
                if calls == 4 {
                    bail!("synthetic Steam start before second write")
                }
                Ok(())
            },
            &mut |_| Ok(()),
        );
        assert!(result.is_err());
        assert_eq!(calls, 4);
        for (path, original) in preview.files.iter().zip(originals) {
            assert_eq!(fs::read(path).unwrap(), original);
        }
    }

    #[test]
    fn lock_serializes_tool_management_and_assignment_operations() {
        let f = Fixture::new();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let _guard = lock_steam_root(&f.root).unwrap();
        assert!(
            f.apply(&preview)
                .unwrap_err()
                .to_string()
                .contains("Another Steam compatibility operation")
        );
    }

    #[test]
    fn rollback_conflict_is_explicit_and_retains_original_staging_and_backup() {
        let f = Fixture::new();
        let preview = plan(&f.target, &[f.assignment()]).unwrap();
        let global = f.root.join("config/config.vdf");
        let original = fs::read(&global).unwrap();
        let error = apply_with(&preview, &mut || Ok(()), &mut |index| {
            if index == 1 {
                fs::write(&global, "// concurrent writer\nroot {}")?;
                bail!("synthetic failure with a concurrent writer")
            }
            Ok(())
        })
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("Automatic restoration was incomplete")
        );
        assert!(error.to_string().contains("Do not start Steam"));
        assert_eq!(
            fs::read_to_string(global).unwrap(),
            "// concurrent writer\nroot {}"
        );
        for kind in ["staging", "backup"] {
            let copies: Vec<_> = fs::read_dir(f.root.join("config"))
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| {
                    path.file_name()
                        .unwrap()
                        .to_string_lossy()
                        .contains(&format!(".hyprdeck-{kind}-"))
                })
                .collect();
            assert_eq!(copies.len(), 1);
            assert_eq!(fs::read(&copies[0]).unwrap(), original);
        }
    }
}
