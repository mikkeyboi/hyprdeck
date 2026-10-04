//! The privileged half of the in-app system update: `hyprdeck updates
//! root-helper --uid <uid> --cache <dir>`, started once per update through
//! `pkexec`. It speaks line-based JSON over stdin/stdout and only ever runs
//! three fixed pacman transactions with validated arguments — never a shell or
//! an arbitrary command. It exits when stdin closes.

use std::io::{BufRead, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::pkgbuild;

/// Requests from the unprivileged app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum Request {
    /// `pacman -Syu`.
    Upgrade,
    /// `pacman -S --needed --asdeps` of repository packages (AUR build dependencies).
    InstallDeps {
        names: Vec<String>,
    },
    /// `pacman -U` of packages built by the user in the AUR cache.
    InstallBuilt {
        files: Vec<String>,
    },
    Exit,
}

/// Messages to the app.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event")]
pub enum Event {
    /// Sent once after startup validation (i.e. after authentication).
    Ready { version: String },
    /// One line of pacman output (stdout or stderr).
    Line { text: String },
    /// pacman finished with this exit status.
    Done { status: i32 },
    /// The request was refused; nothing ran.
    Rejected { error: String },
}

const PACMAN: &str = "/usr/bin/pacman";
const STDBUF: &str = "/usr/bin/stdbuf";
const COMMON: [&str; 4] = ["--noconfirm", "--noprogressbar", "--color", "never"];
const PACKAGE_SUFFIXES: [&str; 2] = [".pkg.tar.zst", ".pkg.tar.xz"];

/// The validated session parameters.
#[derive(Debug, Clone)]
pub struct Session {
    pub uid: u32,
    /// The user's AUR cache (canonical).
    pub cache: PathBuf,
    /// Root-owned directory under which built packages are staged before `pacman -U`.
    pub staging: PathBuf,
}

/// Runs pacman (injected so request handling can be tested without root).
pub trait Exec {
    /// Run `pacman <args>`, passing each output line to `line`; returns the exit status.
    fn pacman(&self, args: &[String], line: &mut dyn FnMut(&str)) -> Result<i32>;
    /// `Err(reason)` unless every name resolves in the sync repositories.
    fn in_sync_repos(&self, names: &[String]) -> Result<(), String>;
}

/// Entry point for `hyprdeck updates root-helper …`.
pub fn main(args: &[String]) -> Result<()> {
    // SAFETY: geteuid has no preconditions.
    if unsafe { libc::geteuid() } != 0 {
        bail!(
            "the update helper must run as root; Hyprdeck starts it through pkexec during an update"
        );
    }
    let (uid, cache) = match args {
        [a, uid, c, cache] if a == "--uid" && c == "--cache" => (uid, cache),
        _ => bail!("usage: hyprdeck updates root-helper --uid <uid> --cache <dir>"),
    };
    let uid = validate_uid(
        uid,
        std::env::var("PKEXEC_UID").ok().as_deref(),
        std::env::var("SUDO_UID").ok().as_deref(),
    )?;
    let cache = validate_cache(Path::new(cache), uid)?;
    let staging = if Path::new("/run").is_dir() {
        PathBuf::from("/run")
    } else {
        std::env::temp_dir()
    };
    let session = Session {
        uid,
        cache,
        staging,
    };
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();
    serve(stdin.lock(), &mut stdout, &session, &SystemPacman)
}

/// Answer requests until `Exit` or end of input.
pub fn serve(
    input: impl BufRead,
    out: &mut dyn Write,
    session: &Session,
    exec: &dyn Exec,
) -> Result<()> {
    emit(
        out,
        &Event::Ready {
            version: hyprdeck_core::VERSION.to_owned(),
        },
    )?;
    for line in input.lines() {
        let line = line.context("reading a request")?;
        if line.trim().is_empty() {
            continue;
        }
        let request: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                emit(
                    out,
                    &Event::Rejected {
                        error: format!("malformed request: {e}"),
                    },
                )?;
                continue;
            }
        };
        if request == Request::Exit {
            break;
        }
        let prepared = match prepare(&request, session, exec) {
            Ok(p) => p,
            Err(e) => {
                emit(
                    out,
                    &Event::Rejected {
                        error: format!("{e:#}"),
                    },
                )?;
                continue;
            }
        };
        let mut write_err = None;
        let status = exec.pacman(&prepared.args, &mut |text| {
            if write_err.is_none()
                && let Err(e) = emit(
                    out,
                    &Event::Line {
                        text: text.to_owned(),
                    },
                )
            {
                write_err = Some(e);
            }
        });
        drop(prepared);
        if let Some(e) = write_err {
            return Err(e);
        }
        let status = match status {
            Ok(s) => s,
            Err(e) => {
                emit(
                    out,
                    &Event::Line {
                        text: format!("error: {e:#}"),
                    },
                )?;
                -1
            }
        };
        emit(out, &Event::Done { status })?;
    }
    Ok(())
}

fn emit(out: &mut dyn Write, event: &Event) -> Result<()> {
    let mut line = serde_json::to_string(event)?;
    line.push('\n');
    out.write_all(line.as_bytes())?;
    out.flush()?;
    Ok(())
}

/// pacman arguments for a request, plus staged package copies that live until dropped.
struct Prepared {
    args: Vec<String>,
    _staged: Option<Staged>,
}

fn prepare(request: &Request, session: &Session, exec: &dyn Exec) -> Result<Prepared> {
    let with = |op: &[&str], targets: &[String]| -> Vec<String> {
        let mut args: Vec<String> = op.iter().chain(&COMMON).map(|s| (*s).to_owned()).collect();
        if !targets.is_empty() {
            args.push("--".into());
            args.extend(targets.iter().cloned());
        }
        args
    };
    Ok(match request {
        Request::Upgrade => Prepared {
            args: with(&["-Syu"], &[]),
            _staged: None,
        },
        Request::InstallDeps { names } => {
            validate_names(names)?;
            exec.in_sync_repos(names)
                .map_err(|e| anyhow!("not installable from the repositories: {e}"))?;
            Prepared {
                args: with(&["-S", "--needed", "--asdeps"], names),
                _staged: None,
            }
        }
        Request::InstallBuilt { files } => {
            let staged = stage(files, session)?;
            let targets: Vec<String> = staged
                .files
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            Prepared {
                args: with(&["-U"], &targets),
                _staged: Some(staged),
            }
        }
        Request::Exit => bail!("nothing to run"),
    })
}

/// The invoking user: `--uid` must name a non-root user and match the uid
/// pkexec (or sudo) reports for the caller.
pub fn validate_uid(uid: &str, pkexec_uid: Option<&str>, sudo_uid: Option<&str>) -> Result<u32> {
    let parsed: u32 = uid.parse().map_err(|_| anyhow!("invalid --uid {uid:?}"))?;
    if parsed == 0 {
        bail!("--uid must be the unprivileged user that builds AUR packages, not root");
    }
    let Some(caller) = pkexec_uid.or(sudo_uid) else {
        bail!("refusing to run: not started through pkexec (PKEXEC_UID is not set)");
    };
    if caller.parse::<u32>().ok() != Some(parsed) {
        bail!("--uid {parsed} is not the user who started the helper ({caller})");
    }
    Ok(parsed)
}

/// `path` must be absolute, free of symlinks and `..`, a directory owned by
/// `uid` that only its owner can write, and every ancestor must be owned by
/// root or `uid` and not writable by others unless sticky (like /tmp).
pub fn validate_cache(path: &Path, uid: u32) -> Result<PathBuf> {
    if !plain_absolute(path) {
        bail!(
            "cache dir {} must be an absolute, normalized path",
            path.display()
        );
    }
    let canonical = path
        .canonicalize()
        .with_context(|| format!("cache dir {}", path.display()))?;
    if canonical != path {
        bail!("cache dir {} must not involve symlinks", path.display());
    }
    let meta = std::fs::symlink_metadata(path)?;
    if !meta.is_dir() {
        bail!("cache dir {} is not a directory", path.display());
    }
    if meta.uid() != uid {
        bail!("cache dir {} is not owned by uid {uid}", path.display());
    }
    if meta.mode() & 0o022 != 0 {
        bail!("cache dir {} is writable by other users", path.display());
    }
    for ancestor in path.ancestors().skip(1) {
        let m = std::fs::symlink_metadata(ancestor)?;
        if m.uid() != 0 && m.uid() != uid {
            bail!(
                "{} (above the cache dir) belongs to another user",
                ancestor.display()
            );
        }
        if m.mode() & 0o022 != 0 && m.mode() & 0o1000 == 0 {
            bail!(
                "{} (above the cache dir) is writable by other users",
                ancestor.display()
            );
        }
    }
    Ok(canonical)
}

fn plain_absolute(path: &Path) -> bool {
    path.is_absolute()
        && path
            .components()
            .all(|c| matches!(c, Component::RootDir | Component::Normal(_)))
}

/// Repository package names (`^[a-z0-9@._+-]+$`, no leading `-`).
pub fn validate_names(names: &[String]) -> Result<()> {
    if names.is_empty() {
        bail!("no packages given");
    }
    if let Some(bad) = names.iter().find(|n| !pkgbuild::valid_name(n)) {
        bail!("invalid package name {bad:?}");
    }
    Ok(())
}

/// Open a built package for staging: an absolute path inside `cache` without
/// symlinks, a regular file owned by `uid`, named `*.pkg.tar.zst|xz`.
pub fn open_package(path: &str, cache: &Path, uid: u32) -> Result<std::fs::File> {
    let p = Path::new(path);
    let name = p
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("{path}: not a file path"))?;
    if !PACKAGE_SUFFIXES.iter().any(|s| name.ends_with(s)) || name.starts_with(['-', '.']) {
        bail!("{path}: not a package file (*.pkg.tar.zst or *.pkg.tar.xz)");
    }
    if !plain_absolute(p) || !p.starts_with(cache) || p == cache {
        bail!("{path}: outside the AUR cache {}", cache.display());
    }
    let meta = std::fs::symlink_metadata(p).with_context(|| path.to_owned())?;
    if meta.file_type().is_symlink() {
        bail!("{path}: is a symlink");
    }
    if p.canonicalize().with_context(|| path.to_owned())? != p {
        bail!("{path}: path involves symlinks");
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(p)
        .with_context(|| path.to_owned())?;
    // Checked on the opened file so a swapped path can't slip through.
    let meta = file.metadata()?;
    if !meta.is_file() {
        bail!("{path}: not a regular file");
    }
    if meta.uid() != uid {
        bail!("{path}: not owned by uid {uid}");
    }
    Ok(file)
}

/// Root-owned copies of validated packages; removed on drop.
struct Staged {
    dir: PathBuf,
    files: Vec<PathBuf>,
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn stage(files: &[String], session: &Session) -> Result<Staged> {
    if files.is_empty() {
        bail!("no package files given");
    }
    let opened: Vec<(String, std::fs::File)> = files
        .iter()
        .map(|f| {
            let file = open_package(f, &session.cache, session.uid)?;
            let name = Path::new(f)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or_default()
                .to_owned();
            Ok((name, file))
        })
        .collect::<Result<_>>()?;
    let dir = session.staging.join(format!(
        "hyprdeck-update-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos())
    ));
    // Fails if the name exists, so nobody can pre-create it.
    std::fs::DirBuilder::new()
        .mode(0o700)
        .create(&dir)
        .with_context(|| format!("creating {}", dir.display()))?;
    let mut staged = Staged {
        dir,
        files: Vec::new(),
    };
    for (name, mut src) in opened {
        let dest = staged.dir.join(&name);
        if staged.files.contains(&dest) {
            bail!("{name} given twice");
        }
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o644)
            .open(&dest)
            .with_context(|| format!("staging {name}"))?;
        std::io::copy(&mut src, &mut out).with_context(|| format!("staging {name}"))?;
        staged.files.push(dest);
    }
    Ok(staged)
}

/// The real pacman, with line-buffered output when `stdbuf` is available.
struct SystemPacman;

fn pacman_command(args: &[String]) -> Command {
    let mut cmd = if Path::new(STDBUF).exists() {
        let mut c = Command::new(STDBUF);
        c.args(["-oL", "-eL", PACMAN]);
        c
    } else {
        Command::new(PACMAN)
    };
    cmd.args(args)
        .env_clear()
        .env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/bin:/usr/sbin:/bin:/sbin",
        )
        .env("LC_ALL", "C")
        .stdin(Stdio::null());
    cmd
}

impl Exec for SystemPacman {
    fn pacman(&self, args: &[String], line: &mut dyn FnMut(&str)) -> Result<i32> {
        crate::apply::run_streaming(pacman_command(args), line)
    }

    fn in_sync_repos(&self, names: &[String]) -> Result<(), String> {
        let mut args: Vec<String> = ["-Sddp", "--print-format", "%n", "--"]
            .map(String::from)
            .to_vec();
        args.extend(names.iter().cloned());
        let out = pacman_command(&args)
            .output()
            .map_err(|e| format!("failed to run pacman: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_owned())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    struct FakePacman {
        calls: RefCell<Vec<Vec<String>>>,
        /// Contents of the staged files seen by `pacman -U`.
        staged: RefCell<Vec<String>>,
    }

    impl FakePacman {
        fn new() -> Self {
            FakePacman {
                calls: RefCell::default(),
                staged: RefCell::default(),
            }
        }
    }

    impl Exec for FakePacman {
        fn pacman(&self, args: &[String], line: &mut dyn FnMut(&str)) -> Result<i32> {
            self.calls.borrow_mut().push(args.to_vec());
            if args[0] == "-U" {
                let files = args.iter().skip_while(|a| *a != "--").skip(1);
                for f in files {
                    self.staged
                        .borrow_mut()
                        .push(std::fs::read_to_string(f).unwrap());
                }
            }
            line(":: Synchronizing package databases...");
            line("error: example");
            Ok(0)
        }

        fn in_sync_repos(&self, names: &[String]) -> Result<(), String> {
            match names.iter().find(|n| n.starts_with("aur-only")) {
                Some(n) => Err(format!("error: target not found: {n}")),
                None => Ok(()),
            }
        }
    }

    fn uid() -> u32 {
        // SAFETY: getuid has no preconditions.
        unsafe { libc::getuid() }
    }

    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("hd-helper-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::DirBuilder::new()
                .mode(0o755)
                .recursive(true)
                .create(&dir)
                .unwrap();
            TempDir(dir.canonicalize().unwrap())
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn run(session: &Session, exec: &FakePacman, input: &str) -> Vec<Event> {
        let mut out = Vec::new();
        serve(input.as_bytes(), &mut out, session, exec).unwrap();
        String::from_utf8(out)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }

    #[test]
    fn uid_must_match_the_caller() {
        assert_eq!(validate_uid("1000", Some("1000"), None).unwrap(), 1000);
        assert_eq!(validate_uid("1000", None, Some("1000")).unwrap(), 1000);
        assert!(validate_uid("1000", Some("1001"), None).is_err());
        assert!(validate_uid("0", Some("0"), None).is_err());
        assert!(validate_uid("1000", None, None).is_err());
        assert!(validate_uid("-1", Some("-1"), None).is_err());
        assert!(validate_uid("1000x", Some("1000"), None).is_err());
    }

    #[test]
    fn cache_dir_rules() {
        let t = TempDir::new("cache");
        let cache = t.0.join("aur");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&cache)
            .unwrap();
        assert_eq!(validate_cache(&cache, uid()).unwrap(), cache);
        assert!(validate_cache(&cache, uid() + 1).is_err(), "wrong owner");
        assert!(validate_cache(Path::new("relative/aur"), uid()).is_err());
        assert!(validate_cache(&t.0.join("aur/../aur"), uid()).is_err());
        let link = t.0.join("link");
        std::os::unix::fs::symlink(&cache, &link).unwrap();
        assert!(validate_cache(&link, uid()).is_err(), "symlink");
        let open = t.0.join("open");
        std::fs::DirBuilder::new()
            .mode(0o777)
            .create(&open)
            .unwrap();
        std::fs::set_permissions(&open, std::os::unix::fs::PermissionsExt::from_mode(0o777))
            .unwrap();
        assert!(validate_cache(&open, uid()).is_err(), "world-writable");
        let nested = open.join("aur");
        std::fs::DirBuilder::new()
            .mode(0o755)
            .create(&nested)
            .unwrap();
        assert!(
            validate_cache(&nested, uid()).is_err(),
            "world-writable ancestor without sticky bit"
        );
    }

    #[test]
    fn package_files_must_be_owned_regular_files_inside_the_cache() {
        let t = TempDir::new("files");
        let cache = t.0.join("aur");
        std::fs::create_dir_all(cache.join("foo")).unwrap();
        let good = cache.join("foo/foo-1.0-1-x86_64.pkg.tar.zst");
        std::fs::write(&good, "pkg").unwrap();
        let g = good.to_str().unwrap();
        assert!(open_package(g, &cache, uid()).is_ok());
        assert!(open_package(g, &cache, uid() + 1).is_err(), "wrong owner");

        let outside = t.0.join("foo-1.0-1-x86_64.pkg.tar.zst");
        std::fs::write(&outside, "pkg").unwrap();
        assert!(open_package(outside.to_str().unwrap(), &cache, uid()).is_err());
        let dotdot = format!("{}/foo/../../foo-1.0-1-x86_64.pkg.tar.zst", cache.display());
        assert!(open_package(&dotdot, &cache, uid()).is_err());

        let link = cache.join("foo/link-1.0-1-x86_64.pkg.tar.zst");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        assert!(open_package(link.to_str().unwrap(), &cache, uid()).is_err());
        let dir_link = cache.join("bar");
        std::os::unix::fs::symlink(&t.0, &dir_link).unwrap();
        let via = format!("{}/foo-1.0-1-x86_64.pkg.tar.zst", dir_link.display());
        assert!(open_package(&via, &cache, uid()).is_err(), "symlinked dir");

        let wrong_ext = cache.join("foo/foo.tar.zst");
        std::fs::write(&wrong_ext, "x").unwrap();
        assert!(open_package(wrong_ext.to_str().unwrap(), &cache, uid()).is_err());
        let dir = cache.join("foo/d.pkg.tar.zst");
        std::fs::create_dir(&dir).unwrap();
        assert!(open_package(dir.to_str().unwrap(), &cache, uid()).is_err());
        assert!(open_package("foo-1-1-x86_64.pkg.tar.zst", &cache, uid()).is_err());
    }

    #[test]
    fn names_are_validated() {
        let ok =
            |n: &[&str]| validate_names(&n.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>());
        assert!(ok(&["cmake", "libc++", "python-pytest", "qt6-base"]).is_ok());
        assert!(ok(&[]).is_err());
        for bad in [
            "--overwrite=*",
            "-Syu",
            "Foo",
            "a b",
            "x;rm",
            "../x",
            "/usr/bin/x",
        ] {
            assert!(ok(&["cmake", bad]).is_err(), "{bad}");
        }
    }

    #[test]
    fn serves_fixed_pacman_transactions() {
        let t = TempDir::new("serve");
        let cache = t.0.join("aur");
        std::fs::create_dir_all(cache.join("foo")).unwrap();
        let staging = t.0.join("staging");
        std::fs::create_dir(&staging).unwrap();
        let pkg = cache.join("foo/foo-1.0-1-x86_64.pkg.tar.zst");
        std::fs::write(&pkg, "built package").unwrap();
        let session = Session {
            uid: uid(),
            cache: cache.clone(),
            staging: staging.clone(),
        };
        let exec = FakePacman::new();
        let input = format!(
            "{}\n{}\n{}\n{}\n{}\nnot json\n{}\n{}\n",
            r#"{"op":"Upgrade"}"#,
            r#"{"op":"InstallDeps","names":["cmake","ninja"]}"#,
            r#"{"op":"InstallDeps","names":["--overwrite=*"]}"#,
            r#"{"op":"InstallDeps","names":["aur-only-dep"]}"#,
            serde_json::json!({"op": "InstallBuilt", "files": [pkg]}),
            serde_json::json!({"op": "InstallBuilt", "files": ["/etc/shadow"]}),
            r#"{"op":"Exit"}"#,
        );
        let events = run(&session, &exec, &(input + r#"{"op":"Upgrade"}"#));
        assert!(matches!(events[0], Event::Ready { .. }));
        let done = events
            .iter()
            .filter(|e| matches!(e, Event::Done { status: 0 }))
            .count();
        let rejected: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Rejected { error } => Some(error.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(done, 3);
        assert_eq!(rejected.len(), 4, "{rejected:?}");
        assert!(rejected[0].contains("invalid package name"));
        assert!(rejected[1].contains("target not found: aur-only-dep"));
        assert!(rejected[2].starts_with("malformed request"));
        assert!(
            rejected[3].contains("not a package file"),
            "{}",
            rejected[3]
        );
        assert!(events.contains(&Event::Line {
            text: ":: Synchronizing package databases...".into()
        }));

        let calls = exec.calls.borrow();
        assert_eq!(calls.len(), 3, "nothing runs after Exit");
        let common = "--noconfirm --noprogressbar --color never";
        assert_eq!(calls[0].join(" "), format!("-Syu {common}"));
        assert_eq!(
            calls[1].join(" "),
            format!("-S --needed --asdeps {common} -- cmake ninja")
        );
        let staged = calls[2].last().unwrap();
        assert!(calls[2].join(" ").starts_with(&format!("-U {common} -- ")));
        assert!(Path::new(staged).starts_with(&staging));
        assert!(staged.ends_with("/foo-1.0-1-x86_64.pkg.tar.zst"));
        assert_eq!(*exec.staged.borrow(), ["built package"]);
        // Staged copies are removed after the transaction.
        assert_eq!(std::fs::read_dir(&staging).unwrap().count(), 0);
    }

    #[test]
    fn exits_when_input_closes() {
        let t = TempDir::new("eof");
        let session = Session {
            uid: uid(),
            cache: t.0.clone(),
            staging: t.0.clone(),
        };
        let events = run(&session, &FakePacman::new(), "");
        assert_eq!(events.len(), 1);
    }
}
