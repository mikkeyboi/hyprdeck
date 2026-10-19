//! Prefix recovery is an explicit, reversible rename, not a diagnosis or a repair.
//! No prefix is created or deleted here; Proton creates one on the next launch.

use anyhow::{Context, Result, bail};
use std::ffi::{CString, OsStr};
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::steam::{Game, Vdf, parse_vdf};

pub const RECOVERY_WARNING: &str = "Stop Steam, the game, Wine and Proton before continuing. This moves the entire compatdata directory to a same-filesystem backup; nothing is deleted. Savegames, settings, launchers and credentials may exist only inside that directory. Back up or synchronize saves first. Proton creates a fresh prefix on the next launch. A reset does not establish the cause of a launch failure or repair a broken compatibility-tool build, game files, drivers or launch options. Switching Proton versions can migrate a prefix. Restoring a backup first preserves any current prefix in another backup; it does not merge saves.";

#[derive(Clone, Debug)]
pub struct Backup {
    pub id: String,
    pub path: PathBuf,
    /// UTC timestamp; external/manual copies use the directory's modification time.
    pub created: String,
}

struct Directory {
    library: File,
    steamapps: File,
    compatdata: File,
    path: PathBuf,
    library_path: PathBuf,
}

/// List only recognized sibling backups. Manual `<appid>.before-*` copies must
/// contain a real pfx directory; symlinks are never offered for restoration.
pub fn backups(game: &Game) -> Result<Vec<Backup>> {
    let Some(directory) = open_game(game)? else {
        return Ok(Vec::new());
    };
    let mut result = Vec::new();
    for entry in fs::read_dir(format!(
        "/proc/self/fd/{}",
        directory.compatdata.as_raw_fd()
    ))
    .context("Cannot enumerate prefix backups")?
    {
        let entry = entry.context("Cannot read a prefix backup entry")?;
        let id = entry.file_name();
        let Some(id) = id.to_str() else { continue };
        if !recognized_backup(game.app_id, id) {
            continue;
        }
        let Some(stat) = stat_at(&directory.compatdata, OsStr::new(id))? else {
            continue;
        };
        if !is_directory(&stat) {
            continue;
        }
        let candidate = open_directory_at(&directory.compatdata, OsStr::new(id))?;
        if is_manual(game.app_id, id) && !has_real_pfx(&candidate)? {
            continue;
        }
        result.push(backup_record(&directory, id, &candidate)?);
    }
    result.sort_by(|a, b| b.created.cmp(&a.created).then(b.id.cmp(&a.id)));
    Ok(result)
}

/// Read just Proton's bounded version marker, never prefix contents or credentials.
pub fn prefix_version(game: &Game) -> Result<Option<String>> {
    let Some(directory) = open_game(game)? else {
        return Ok(None);
    };
    let name = game.app_id.to_string();
    let Some(stat) = stat_at(&directory.compatdata, OsStr::new(&name))? else {
        return Ok(None);
    };
    require_directory(&stat, "The selected prefix is not a real directory")?;
    let prefix = open_directory_at(&directory.compatdata, OsStr::new(&name))?;
    let mut version = match open_file_at(&prefix, OsStr::new("version")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).context("Cannot safely read the prefix version marker"),
    };
    let metadata = version.metadata()?;
    if !metadata.is_file() || metadata.len() > 1024 {
        bail!("Prefix version marker is not a small regular file");
    }
    let mut text = String::new();
    version.by_ref().take(1025).read_to_string(&mut text)?;
    let value = text.trim();
    if text.len() > 1024
        || value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        bail!("Prefix version marker is not a valid version string");
    }
    Ok((!value.is_empty()).then(|| value.to_owned()))
}

/// Conservative observational guard: any visible Steam/Wine/Proton process or
/// compatibility environment blocks recovery, even if its game cannot be identified.
pub fn ensure_stopped(game: &Game) -> Result<()> {
    ensure_stopped_in(game, Path::new("/proc"))
}

pub fn backup_and_reset(game: &Game) -> Result<Backup> {
    backup_and_reset_in(game, Path::new("/proc"))
}

fn backup_and_reset_in(game: &Game, proc_root: &Path) -> Result<Backup> {
    ensure_stopped_in(game, proc_root)?;
    let directory = open_game(game)?
        .context("No compatibility data exists for this game; there is nothing to back up")?;
    lock(&directory)?;
    directory.validate_anchor()?;
    backup_current(&directory, game, proc_root)
}

/// Preserve the current prefix (if any) before atomically restoring the selected
/// backup. Returns that newly preserved backup, never deletes or merges either.
pub fn restore(game: &Game, backup: &Backup) -> Result<Option<Backup>> {
    restore_in(game, backup, Path::new("/proc"))
}

fn restore_in(game: &Game, backup: &Backup, proc_root: &Path) -> Result<Option<Backup>> {
    ensure_stopped_in(game, proc_root)?;
    let directory =
        open_game(game)?.context("The game's compatibility-data directory is missing")?;
    lock(&directory)?;
    directory.validate_anchor()?;
    validate_backup(&directory, game, backup)?;
    let saved = match stat_at(&directory.compatdata, OsStr::new(&game.app_id.to_string()))? {
        None => None,
        Some(stat) => {
            require_directory(&stat, "The current prefix is not a real directory")?;
            Some(backup_current(&directory, game, proc_root)?)
        }
    };
    // Re-check after preserving the current prefix; Steam could have started meanwhile.
    ensure_stopped_in(game, proc_root).with_context(|| preserved_message(&saved))?;
    directory
        .validate_anchor()
        .with_context(|| preserved_message(&saved))?;
    validate_backup(&directory, game, backup).with_context(|| preserved_message(&saved))?;
    rename_no_replace(&directory.compatdata, &backup.id, &game.app_id.to_string()).with_context(
        || {
            format!(
                "{} No existing directory was overwritten",
                preserved_message(&saved)
            )
        },
    )?;
    Ok(saved)
}

fn preserved_message(saved: &Option<Backup>) -> String {
    match saved {
        Some(backup) => format!(
            "Restoration did not finish; the previous current prefix is preserved at {}.",
            backup.path.display()
        ),
        None => "Restoration did not finish; the selected backup remains preserved.".to_owned(),
    }
}

fn backup_current(directory: &Directory, game: &Game, proc_root: &Path) -> Result<Backup> {
    let source = game.app_id.to_string();
    let before = stat_at(&directory.compatdata, OsStr::new(&source))?
        .context("No prefix exists for this game")?;
    require_directory(
        &before,
        "The selected prefix is not a real directory; symlinks are not supported",
    )?;
    directory.validate_anchor()?;
    ensure_stopped_in(game, proc_root)?;
    static SERIAL: AtomicU64 = AtomicU64::new(0);
    for _ in 0..64 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .context("System clock predates Unix time")?
            .as_nanos();
        let id = format!(
            "{}.hyprdeck-backup-{nanos}-{}-{}",
            game.app_id,
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        );
        match rename_no_replace(&directory.compatdata, &source, &id) {
            Ok(()) => {
                let after = stat_at(&directory.compatdata, OsStr::new(&id))?
                    .context("Backup moved unexpectedly; no files were deleted")?;
                if !same_inode(&before, &after) || !is_directory(&after) {
                    bail!(
                        "The prefix changed during recovery; the moved entry is preserved at {}",
                        directory.path.join(&id).display()
                    );
                }
                let file = open_directory_at(&directory.compatdata, OsStr::new(&id))?;
                return backup_record(directory, &id, &file);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).context(
                    "Cannot atomically back up prefix without overwriting an existing entry",
                );
            }
        }
    }
    bail!("Cannot reserve a unique backup name; no directory was overwritten")
}

fn recognized_backup(app_id: u32, id: &str) -> bool {
    let own = format!("{app_id}.hyprdeck-backup-");
    let manual = format!("{app_id}.before-");
    !id.contains('/')
        && !id.contains('\0')
        && ((id.starts_with(&own) && id.len() > own.len())
            || (id.starts_with(&manual) && id.len() > manual.len()))
}

fn is_manual(app_id: u32, id: &str) -> bool {
    id.starts_with(&format!("{app_id}.before-"))
}

fn has_real_pfx(prefix: &File) -> Result<bool> {
    Ok(stat_at(prefix, OsStr::new("pfx"))?.is_some_and(|stat| is_directory(&stat)))
}

fn validate_backup(directory: &Directory, game: &Game, backup: &Backup) -> Result<()> {
    if !recognized_backup(game.app_id, &backup.id) || backup.path != directory.path.join(&backup.id)
    {
        bail!("Selected backup is not a recognized sibling of this game's prefix");
    }
    let stat = stat_at(&directory.compatdata, OsStr::new(&backup.id))?
        .context("The selected backup no longer exists")?;
    require_directory(
        &stat,
        "Selected backup is not a real directory; symlinks are not supported",
    )?;
    let candidate = open_directory_at(&directory.compatdata, OsStr::new(&backup.id))?;
    if is_manual(game.app_id, &backup.id) && !has_real_pfx(&candidate)? {
        bail!("Manual backup does not contain a real pfx directory");
    }
    Ok(())
}

fn backup_record(directory: &Directory, id: &str, file: &File) -> Result<Backup> {
    let from_id = id
        .split_once(".hyprdeck-backup-")
        .and_then(|(_, rest)| rest.split('-').next())
        .and_then(|nanos| nanos.parse::<u128>().ok())
        .and_then(|nanos| u64::try_from(nanos / 1_000_000_000).ok());
    let seconds = match from_id {
        Some(seconds) => seconds,
        None => file
            .metadata()?
            .modified()?
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs(),
    };
    Ok(Backup {
        id: id.to_owned(),
        path: directory.path.join(id),
        created: utc_timestamp(seconds),
    })
}

fn utc_timestamp(seconds: u64) -> String {
    let Ok(seconds) = libc::time_t::try_from(seconds) else {
        return "Unknown timestamp".to_owned();
    };
    let mut time = std::mem::MaybeUninit::<libc::tm>::uninit();
    // SAFETY: both pointers refer to live, correctly sized values; gmtime_r writes tm.
    if unsafe { libc::gmtime_r(&seconds, time.as_mut_ptr()) }.is_null() {
        return format!("{seconds} (Unix time)");
    }
    // SAFETY: a non-null gmtime_r result initialized the output.
    let time = unsafe { time.assume_init() };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02} UTC",
        time.tm_year + 1900,
        time.tm_mon + 1,
        time.tm_mday,
        time.tm_hour,
        time.tm_min,
        time.tm_sec
    )
}

fn open_game(game: &Game) -> Result<Option<Directory>> {
    if game.app_id == 0 || !game.library.is_absolute() {
        bail!("Invalid game or Steam library selection");
    }
    let canonical =
        fs::canonicalize(&game.library).context("Cannot resolve the selected Steam library")?;
    if canonical != game.library {
        bail!("Steam library must use its canonical, non-symlink path");
    }
    let expected = canonical
        .join("steamapps/compatdata")
        .join(game.app_id.to_string());
    if game.compatdata != expected {
        bail!(
            "Selected prefix is outside its canonical Steam library or does not match the application ID"
        );
    }
    let library = open_absolute_directory(&canonical)
        .context("Cannot safely open the canonical Steam library")?;
    let steamapps = open_directory_at(&library, OsStr::new("steamapps"))
        .context("Steam library steamapps must be a real directory")?;
    validate_manifest(&steamapps, game.app_id)?;
    let compatdata = match open_directory_at(&steamapps, OsStr::new("compatdata")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error)
                .context("Compatibility-data parent must be a real, non-symlink directory");
        }
    };
    let result = Directory {
        library,
        steamapps,
        compatdata,
        path: canonical.join("steamapps/compatdata"),
        library_path: canonical,
    };
    result.validate_anchor()?;
    Ok(Some(result))
}

fn validate_manifest(steamapps: &File, app_id: u32) -> Result<()> {
    let mut file = open_file_at(steamapps, OsStr::new(&format!("appmanifest_{app_id}.acf")))
        .context("Cannot safely verify the game's Steam manifest")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        bail!("Game manifest is not a bounded regular file");
    }
    let mut text = String::new();
    file.by_ref()
        .take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut text)?;
    let vdf = parse_vdf(&text).context("Cannot verify the game's Steam manifest")?;
    let verified = vdf
        .get("AppState")
        .and_then(|value| value.get("appid"))
        .and_then(Vdf::text)
        .and_then(|value| value.parse::<u32>().ok());
    if verified != Some(app_id) {
        bail!("The selected application ID does not match its Steam manifest");
    }
    Ok(())
}

impl Directory {
    fn validate_anchor(&self) -> Result<()> {
        let current_library = open_absolute_directory(&self.library_path)
            .context("Steam library changed during recovery")?;
        if !same_files(&current_library, &self.library)? {
            bail!("Steam library changed during recovery");
        }
        let steamapps = stat_at(&self.library, OsStr::new("steamapps"))?
            .context("Steam library layout changed")?;
        let compatdata = stat_at(&self.steamapps, OsStr::new("compatdata"))?
            .context("Compatibility-data parent changed")?;
        if !is_directory(&steamapps)
            || !matches_file(&steamapps, &self.steamapps)?
            || !is_directory(&compatdata)
            || !matches_file(&compatdata, &self.compatdata)?
        {
            bail!("Steam library directory identities changed during recovery");
        }
        Ok(())
    }
}

fn same_files(left: &File, right: &File) -> Result<bool> {
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok(left.dev() == right.dev() && left.ino() == right.ino())
}

fn matches_file(stat: &libc::stat, file: &File) -> Result<bool> {
    let metadata = file.metadata()?;
    Ok(stat.st_dev == metadata.dev() && stat.st_ino == metadata.ino())
}

fn same_inode(left: &libc::stat, right: &libc::stat) -> bool {
    left.st_dev == right.st_dev && left.st_ino == right.st_ino
}
fn is_directory(stat: &libc::stat) -> bool {
    stat.st_mode & libc::S_IFMT == libc::S_IFDIR
}
fn require_directory(stat: &libc::stat, message: &str) -> Result<()> {
    if !is_directory(stat) {
        bail!("{message}");
    }
    Ok(())
}

fn c_name(name: &OsStr) -> io::Result<CString> {
    if name.as_bytes().contains(&b'/')
        || name.as_bytes() == b"."
        || name.as_bytes() == b".."
        || name.is_empty()
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected a single directory entry",
        ));
    }
    CString::new(name.as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "Invalid directory entry"))
}

fn open_absolute_directory(path: &Path) -> io::Result<File> {
    if !path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Expected an absolute directory",
        ));
    }
    let mut directory = File::open("/")?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => directory = open_directory_at(&directory, name)?,
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Expected a canonical directory",
                ));
            }
        }
    }
    Ok(directory)
}

fn open_directory_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
    )
}

fn open_file_at(parent: &File, name: &OsStr) -> io::Result<File> {
    open_at(
        parent,
        name,
        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
    )
}

fn open_at(parent: &File, name: &OsStr, flags: libc::c_int) -> io::Result<File> {
    let name = c_name(name)?;
    // SAFETY: parent owns an open descriptor and name is a NUL-terminated single component.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful openat returns a new owned descriptor.
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn stat_at(parent: &File, name: &OsStr) -> io::Result<Option<libc::stat>> {
    let name = c_name(name)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: pointers are valid, and no symlink target is followed.
    let result = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        };
    }
    // SAFETY: successful fstatat initialized the stat value.
    Ok(Some(unsafe { stat.assume_init() }))
}

fn lock(directory: &Directory) -> Result<()> {
    // SAFETY: descriptor remains owned for the duration of the operation.
    if unsafe {
        libc::flock(
            directory.compatdata.as_raw_fd(),
            libc::LOCK_EX | libc::LOCK_NB,
        )
    } != 0
    {
        return Err(io::Error::last_os_error())
            .context("Another prefix recovery operation is active");
    }
    Ok(())
}

fn rename_no_replace(parent: &File, source: &str, destination: &str) -> io::Result<()> {
    let source = c_name(OsStr::new(source))?;
    let destination = c_name(OsStr::new(destination))?;
    // SAFETY: both names are single components in the same held directory. Linux
    // RENAME_NOREPLACE atomically refuses an existing destination, including symlinks.
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn ensure_stopped_in(_game: &Game, proc_root: &Path) -> Result<()> {
    let mut owned = Vec::new();
    for entry in
        fs::read_dir(proc_root).context("Cannot inspect running processes; recovery is blocked")?
    {
        let entry = entry.context("Cannot enumerate running processes; recovery is blocked")?;
        if !entry.file_name().as_bytes().iter().all(u8::is_ascii_digit)
            || entry.file_name().is_empty()
        {
            continue;
        }
        let process = entry.path();
        let metadata = match fs::metadata(&process) {
            Ok(metadata) => metadata,
            Err(error) if disappeared(&error) => continue,
            Err(error) => {
                return Err(error).context("Cannot inspect a running process; recovery is blocked");
            }
        };
        let comm = match bounded_proc_read(&process.join("comm")) {
            Ok(comm) => comm,
            Err(error) if disappeared(&error) => continue,
            Err(error) => {
                return Err(error).context("Cannot inspect process names; recovery is blocked");
            }
        };
        if runtime_name(String::from_utf8_lossy(&comm).trim()) {
            bail!(
                "Steam, Wine or Proton is running. Fully exit Steam and all games before prefix recovery"
            );
        }
        // Other users' environments may be unreadable and must never be reported.
        // Visible runtime process names above still conservatively block recovery.
        if metadata.uid() != unsafe { libc::geteuid() } {
            continue;
        }
        owned.push(process);
    }
    // Check all visible runtime names before probing environments. Known active
    // Steam/Wine is more actionable than an unrelated protected process, while
    // an incomplete second pass still fails closed.
    for process in owned {
        for name in ["cmdline", "environ"] {
            let bytes = match bounded_proc_read(&process.join(name)) {
                Ok(bytes) => bytes,
                Err(error) if disappeared(&error) => continue,
                Err(error) => return Err(error).context(
                    "Cannot verify that compatibility processes are stopped; recovery is blocked",
                ),
            };
            let running = if name == "cmdline" {
                bytes
                    .split(|byte| *byte == 0)
                    .filter_map(|argument| std::str::from_utf8(argument).ok())
                    .any(|argument| {
                        Path::new(argument)
                            .file_name()
                            .and_then(OsStr::to_str)
                            .is_some_and(runtime_name)
                    })
            } else {
                bytes
                    .split(|byte| *byte == 0)
                    .any(compatibility_environment)
            };
            if running {
                bail!(
                    "A Steam, Wine, Proton or game process is active. Fully stop it before prefix recovery"
                );
            }
        }
    }
    Ok(())
}

fn disappeared(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::NotFound || error.raw_os_error() == Some(libc::ESRCH)
}

fn bounded_proc_read(path: &Path) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(1024 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 1024 * 1024 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Process metadata exceeds safe inspection size",
        ));
    }
    Ok(bytes)
}

fn runtime_name(name: &str) -> bool {
    [
        "steam",
        "steam.exe",
        "steamservice.exe",
        "proton",
        "wine",
        "pressure-vessel",
        "pv-bwrap",
    ]
    .iter()
    .any(|expected| name.eq_ignore_ascii_case(expected))
        || [
            "steam-",
            "steamwebhelper",
            "proton-",
            "wine64",
            "wine-preloader",
            "wineserver",
            "wineboot",
            "pressure-vessel-",
        ]
        .iter()
        .any(|prefix| {
            name.get(..prefix.len())
                .is_some_and(|part| part.eq_ignore_ascii_case(prefix))
        })
}

fn compatibility_environment(entry: &[u8]) -> bool {
    let Some(index) = entry.iter().position(|byte| *byte == b'=') else {
        return false;
    };
    let (key, value) = (&entry[..index], &entry[index + 1..]);
    if value.is_empty() {
        return false;
    }
    match key {
        b"WINEPREFIX"
        | b"STEAM_COMPAT_DATA_PATH"
        | b"STEAM_COMPAT_CLIENT_INSTALL_PATH"
        | b"STEAM_COMPAT_TOOL_PATHS" => true,
        b"SteamAppId" | b"SteamGameId" | b"STEAM_COMPAT_APP_ID" => value != b"0",
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    fn fixture() -> (tempfile::TempDir, Game, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let library = fs::canonicalize(temp.path()).unwrap();
        let steamapps = library.join("steamapps");
        fs::create_dir_all(steamapps.join("compatdata/42/pfx/drive_c/users/steamuser/Saved Games"))
            .unwrap();
        fs::write(
            steamapps.join("appmanifest_42.acf"),
            r#""AppState" { "appid" "42" }"#,
        )
        .unwrap();
        fs::write(steamapps.join("compatdata/42/version"), "11.0-100\n").unwrap();
        fs::write(
            steamapps.join("compatdata/42/pfx/drive_c/users/steamuser/Saved Games/save.dat"),
            "original-save",
        )
        .unwrap();
        let proc_root = library.join("proc-fixture");
        fs::create_dir(&proc_root).unwrap();
        let game = Game {
            app_id: 42,
            name: "Fixture".into(),
            library: library.clone(),
            install_path: steamapps.join("common/Fixture"),
            compatdata: steamapps.join("compatdata/42"),
            tool: None,
            assignment: "unknown automatic".into(),
            steam_root: library,
        };
        (temp, game, proc_root)
    }

    #[test]
    fn reset_and_restore_preserve_entire_prefix_and_current_state() {
        let (_temp, game, proc_root) = fixture();
        assert_eq!(prefix_version(&game).unwrap().as_deref(), Some("11.0-100"));
        let original = backup_and_reset_in(&game, &proc_root).unwrap();
        assert!(!game.compatdata.exists());
        assert!(original.path.join("version").is_file());
        assert_eq!(backups(&game).unwrap().len(), 1);
        fs::create_dir_all(game.compatdata.join("pfx")).unwrap();
        fs::write(game.compatdata.join("new-save"), "newer-save").unwrap();
        let preserved = restore_in(&game, &original, &proc_root).unwrap().unwrap();
        assert_eq!(
            fs::read_to_string(preserved.path.join("new-save")).unwrap(),
            "newer-save"
        );
        assert_eq!(
            fs::read_to_string(
                game.compatdata
                    .join("pfx/drive_c/users/steamuser/Saved Games/save.dat")
            )
            .unwrap(),
            "original-save"
        );
        assert!(!original.path.exists());
        assert!(restore_in(&game, &original, &proc_root).is_err());
        assert!(game.compatdata.join("version").exists());
    }

    #[test]
    fn rename_never_clobbers_existing_directory_or_symlink() {
        let (_temp, game, _proc_root) = fixture();
        let directory = open_game(&game).unwrap().unwrap();
        fs::create_dir(directory.path.join("occupied")).unwrap();
        fs::write(directory.path.join("occupied/keep"), "keep").unwrap();
        assert!(rename_no_replace(&directory.compatdata, "42", "occupied").is_err());
        assert!(game.compatdata.exists());
        assert_eq!(
            fs::read_to_string(directory.path.join("occupied/keep")).unwrap(),
            "keep"
        );
        symlink("occupied", directory.path.join("linked")).unwrap();
        assert!(rename_no_replace(&directory.compatdata, "42", "linked").is_err());
        assert!(game.compatdata.exists());
    }

    #[test]
    fn symlink_library_alias_is_not_a_mutation_anchor() {
        let (_temp, mut game, proc_root) = fixture();
        let alias = game.library.join("library-alias");
        symlink(&game.library, &alias).unwrap();
        game.library = alias.clone();
        game.compatdata = alias.join("steamapps/compatdata/42");
        assert!(backup_and_reset_in(&game, &proc_root).is_err());
        assert!(game.compatdata.join("version").exists());
    }

    #[test]
    fn symlink_prefix_parent_backup_and_outside_paths_are_rejected() {
        let (_temp, mut game, proc_root) = fixture();
        let valid = game.compatdata.clone();
        game.compatdata = game.library.join("outside");
        assert!(backup_and_reset_in(&game, &proc_root).is_err());
        game.compatdata = valid;
        let original = backup_and_reset_in(&game, &proc_root).unwrap();
        symlink(&original.path, &game.compatdata).unwrap();
        assert!(backup_and_reset_in(&game, &proc_root).is_err());
        assert!(restore_in(&game, &original, &proc_root).is_err());
        fs::remove_file(&game.compatdata).unwrap();
        let linked = original.path.with_file_name("42.before-linked");
        symlink(&original.path, &linked).unwrap();
        let backup = Backup {
            id: "42.before-linked".into(),
            path: linked,
            created: String::new(),
        };
        assert!(restore_in(&game, &backup, &proc_root).is_err());
        assert_eq!(backups(&game).unwrap().len(), 1);
        let outside = Backup {
            id: original.id.clone(),
            path: game.library.join(&original.id),
            created: String::new(),
        };
        assert!(restore_in(&game, &outside, &proc_root).is_err());
        let compatdata = game.library.join("steamapps/compatdata");
        let moved = game.library.join("old-compatdata");
        fs::rename(&compatdata, &moved).unwrap();
        symlink(&moved, &compatdata).unwrap();
        assert!(backups(&game).is_err());
    }

    #[test]
    fn manual_before_backups_require_real_prefix_structure() {
        let (_temp, game, _proc_root) = fixture();
        let parent = game.compatdata.parent().unwrap();
        fs::create_dir_all(parent.join("42.before-switch-fixture/pfx")).unwrap();
        fs::create_dir(parent.join("42.before-not-a-prefix")).unwrap();
        fs::create_dir_all(parent.join("142.before-other-game/pfx")).unwrap();
        let backups = backups(&game).unwrap();
        assert_eq!(backups.len(), 1);
        assert_eq!(backups[0].id, "42.before-switch-fixture");
    }

    #[test]
    fn running_guard_blocks_names_commands_and_game_environment() {
        let (_temp, game, proc_root) = fixture();
        let process = proc_root.join("1234");
        fs::create_dir(&process).unwrap();
        fs::write(process.join("comm"), "ordinary\n").unwrap();
        fs::write(process.join("cmdline"), b"ordinary\0").unwrap();
        fs::write(process.join("environ"), b"UNRELATED=value\0").unwrap();
        ensure_stopped_in(&game, &proc_root).unwrap();
        fs::write(
            process.join("environ"),
            b"SteamAppId=42\0PRIVATE_TOKEN=not-for-output\0",
        )
        .unwrap();
        let error = backup_and_reset_in(&game, &proc_root)
            .unwrap_err()
            .to_string();
        assert!(!error.contains("not-for-output"));
        assert!(game.compatdata.exists());
        fs::write(process.join("environ"), b"WINEPREFIX=/some/other/game\0").unwrap();
        assert!(ensure_stopped_in(&game, &proc_root).is_err());
        fs::write(process.join("environ"), b"").unwrap();
        fs::write(
            process.join("cmdline"),
            b"python3\0/tools/Proton/proton\0run\0",
        )
        .unwrap();
        assert!(ensure_stopped_in(&game, &proc_root).is_err());
        fs::write(process.join("cmdline"), b"").unwrap();
        fs::write(process.join("comm"), "steam\n").unwrap();
        assert!(ensure_stopped_in(&game, &proc_root).is_err());
    }

    #[test]
    fn app_id_must_match_manifest() {
        let (_temp, game, proc_root) = fixture();
        fs::write(
            game.library.join("steamapps/appmanifest_42.acf"),
            r#""AppState" { "appid" "99" }"#,
        )
        .unwrap();
        assert!(backup_and_reset_in(&game, &proc_root).is_err());
        assert!(game.compatdata.exists());
    }
}
