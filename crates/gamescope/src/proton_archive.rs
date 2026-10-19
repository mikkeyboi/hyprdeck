//! Private archive reader: no shell extraction, special files, or writes through links.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result, bail, ensure};

const MAX_EXPANDED: u64 = 24 * 1024 * 1024 * 1024;
const MAX_ENTRIES: usize = 400_000;
const RECEIPT: &str = ".hyprdeck-tool.json";

pub(super) fn cancelled(cancel: &AtomicBool) -> Result<()> {
    ensure!(!cancel.load(Ordering::Relaxed), "Installation cancelled");
    Ok(())
}

fn relative(path: &Path) -> Result<PathBuf> {
    ensure!(!path.as_os_str().is_empty(), "Empty archive path");
    let mut safe = PathBuf::new();
    for part in path.components() {
        match part {
            Component::Normal(value) => {
                ensure!(
                    value != RECEIPT && value != crate::proton_repair::RECORD,
                    "Archive contains reserved Hyprdeck installation metadata"
                );
                safe.push(value);
            }
            Component::CurDir => (),
            _ => bail!("Archive path is absolute or traverses a parent directory"),
        }
    }
    ensure!(!safe.as_os_str().is_empty(), "Empty archive path");
    Ok(safe)
}

fn reader(path: &Path, name: &str) -> Result<Box<dyn Read>> {
    let file = File::open(path)?;
    if name.ends_with(".tar.gz") || name.ends_with(".tgz") {
        Ok(Box::new(flate2::read::GzDecoder::new(file)))
    } else if name.ends_with(".tar.xz") {
        Ok(Box::new(xz2::read::XzDecoder::new(file)))
    } else if name.ends_with(".tar.zst") {
        Ok(Box::new(zstd::stream::read::Decoder::new(file)?))
    } else if name.ends_with(".tar") {
        Ok(Box::new(file))
    } else {
        bail!("Unsupported compatibility-tool archive format")
    }
}

fn copy_cancelled(
    input: &mut dyn Read,
    output: &mut File,
    cancel: &AtomicBool,
    size: u64,
) -> Result<()> {
    let mut buffer = [0; 128 * 1024];
    let mut copied = 0u64;
    loop {
        cancelled(cancel)?;
        let n = input.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        copied = copied
            .checked_add(n as u64)
            .context("Archive size overflow")?;
        ensure!(
            copied <= size && copied <= MAX_EXPANDED,
            "Archive entry exceeds its declared size"
        );
        output.write_all(&buffer[..n])?;
    }
    ensure!(copied == size, "Archive entry is truncated");
    Ok(())
}

// Resolve the complete symlink graph BEFORE creating any links. Parent components
// are evaluated after symlink expansion, not merely lexically normalized.
fn resolve_link(path: &Path, links: &BTreeMap<PathBuf, PathBuf>, top: &Path) -> Result<PathBuf> {
    let mut pending: VecDeque<_> = path
        .components()
        .map(|c| c.as_os_str().to_os_string())
        .collect();
    let mut result = PathBuf::new();
    let mut expansions = 0;
    while let Some(part) = pending.pop_front() {
        if part == "." {
            continue;
        }
        if part == ".." {
            ensure!(
                result != top && result.pop(),
                "Archive link escapes the tool directory"
            );
            continue;
        }
        ensure!(part != "/", "Archive contains an absolute link");
        result.push(&part);
        if let Some(target) = links.get(&result) {
            expansions += 1;
            ensure!(
                expansions <= 64,
                "Archive contains a cyclic or excessive link chain"
            );
            ensure!(!target.is_absolute(), "Archive contains an absolute link");
            result.pop();
            for part in target.components().rev() {
                pending.push_front(part.as_os_str().to_os_string());
            }
        }
    }
    ensure!(
        result.starts_with(top),
        "Archive link escapes the tool directory"
    );
    Ok(result)
}

pub(super) fn extract(
    path: &Path,
    name: &str,
    destination: &Path,
    cancel: &AtomicBool,
) -> Result<PathBuf> {
    cancelled(cancel)?;
    fs::create_dir(destination)?;
    if name.ends_with(".zip") {
        // GitHub Actions' wrapper contains ONE binary tar, never arbitrary source
        // archives or a second recursively nested zip.
        let mut zip = zip::ZipArchive::new(File::open(path)?)?;
        ensure!(zip.len() <= 32, "Unexpected number of files in CI artifact");
        let mut selected = None;
        let mut names = BTreeSet::new();
        for i in 0..zip.len() {
            let entry = zip.by_index(i)?;
            let safe = relative(Path::new(entry.name()))?;
            ensure!(names.insert(safe.clone()), "Duplicate CI archive entry");
            let mode = entry.unix_mode().unwrap_or(0o100644) & libc::S_IFMT;
            ensure!(
                mode == 0 || mode == libc::S_IFREG || mode == libc::S_IFDIR,
                "CI archive contains a link or special file"
            );
            if entry.is_dir() {
                continue;
            }
            let name = safe
                .file_name()
                .context("Missing CI archive filename")?
                .to_string_lossy();
            ensure!(
                name.ends_with(".tar")
                    || name.ends_with(".tar.gz")
                    || name.ends_with(".tar.xz")
                    || name.ends_with(".tar.zst"),
                "CI artifact contains an unexpected non-binary-archive file"
            );
            ensure!(
                selected.is_none(),
                "CI artifact contains multiple binary archives"
            );
            ensure!(entry.size() <= MAX_EXPANDED, "CI archive is too large");
            selected = Some((i, name.into_owned()));
        }
        let (index, name) = selected.context("CI artifact contains no binary tar archive")?;
        let nested = destination.join("binary-archive");
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&nested)?;
        let mut entry = zip.by_index(index)?;
        let size = entry.size();
        copy_cancelled(&mut entry, &mut output, cancel, size)?;
        drop(output);
        return extract_tar(
            reader(&nested, &name)?,
            &destination.join("payload"),
            cancel,
        );
    }
    extract_tar(reader(path, name)?, &destination.join("payload"), cancel)
}

fn extract_tar(input: Box<dyn Read>, destination: &Path, cancel: &AtomicBool) -> Result<PathBuf> {
    fs::create_dir(destination)?;
    let mut archive = tar::Archive::new(input);
    let mut seen = BTreeSet::new();
    let mut symlinks = BTreeMap::new();
    let mut hardlinks = Vec::new();
    let mut total = 0u64;
    let mut top: Option<PathBuf> = None;
    for entry in archive.entries()? {
        cancelled(cancel)?;
        let mut entry = entry?;
        let path = relative(&entry.path()?)?;
        let first = PathBuf::from(
            path.components()
                .next()
                .context("Empty archive path")?
                .as_os_str(),
        );
        if let Some(top) = &top {
            ensure!(
                *top == first,
                "Archive must contain a single top-level tool directory"
            );
        } else {
            top = Some(first);
        }
        ensure!(
            seen.len() < MAX_ENTRIES,
            "Archive contains too many entries"
        );
        ensure!(
            seen.insert(path.clone()),
            "Archive contains duplicate paths"
        );
        let kind = entry.header().entry_type();
        let absolute = destination.join(&path);
        if kind.is_dir() {
            fs::create_dir_all(&absolute)?;
        } else if kind.is_file() || kind.is_contiguous() {
            let size = entry.size();
            total = total.checked_add(size).context("Archive size overflow")?;
            ensure!(
                total <= MAX_EXPANDED,
                "Expanded archive exceeds the safety limit"
            );
            fs::create_dir_all(absolute.parent().context("Missing archive parent")?)?;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&absolute)?;
            copy_cancelled(&mut entry, &mut output, cancel, size)?;
            // Keep executable bits, never setuid/setgid or world-writable modes.
            let executable = entry.header().mode()? & 0o111;
            output.set_permissions(fs::Permissions::from_mode(0o644 | executable))?;
        } else if kind.is_symlink() || kind.is_hard_link() {
            let target = entry
                .link_name()?
                .context("Archive link has no target")?
                .into_owned();
            ensure!(!target.is_absolute(), "Archive contains an absolute link");
            if kind.is_symlink() {
                symlinks.insert(path, target);
            } else {
                hardlinks.push((path, relative(&target)?));
            }
        } else {
            bail!("Archive contains a device, FIFO, sparse entry, or unsupported special file");
        }
    }
    let top = top.context("Archive is empty")?;
    let tool = destination.join(&top);
    ensure!(
        fs::symlink_metadata(&tool)?.is_dir(),
        "Archive root is not a directory"
    );
    for path in &seen {
        for parent in path.ancestors().skip(1) {
            ensure!(
                !symlinks.contains_key(parent),
                "Archive entry would write through a symlink"
            );
        }
    }
    for (path, target) in &hardlinks {
        cancelled(cancel)?;
        ensure!(
            target.starts_with(&top),
            "Archive hardlink escapes the tool directory"
        );
        let resolved = resolve_link(target, &symlinks, &top)?;
        ensure!(
            resolved == *target,
            "Archive hardlink points through a symlink"
        );
        let source = destination.join(target);
        ensure!(
            fs::symlink_metadata(&source)?.is_file(),
            "Archive hardlink must point to a regular file"
        );
        let output = destination.join(path);
        fs::create_dir_all(output.parent().context("Missing hardlink parent")?)?;
        fs::hard_link(source, output)?;
    }
    for path in symlinks.keys() {
        resolve_link(path, &symlinks, &top)?;
    }
    for (path, target) in &symlinks {
        cancelled(cancel)?;
        let output = destination.join(path);
        fs::create_dir_all(output.parent().context("Missing symlink parent")?)?;
        symlink(target, output)?;
    }
    Ok(tool)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn tar(entries: &[(&str, u8, &str)]) -> Vec<u8> {
        let mut tar = tar::Builder::new(Vec::new());
        for (path, kind, data) in entries {
            let mut header = tar::Header::new_gnu();
            header.set_path(path).unwrap();
            header.set_mode(0o755);
            header.set_entry_type(tar::EntryType::new(*kind));
            if *kind == b'2' || *kind == b'1' {
                header.set_link_name(data).unwrap();
                header.set_size(0);
                header.set_cksum();
                tar.append(&header, std::io::empty()).unwrap();
            } else {
                header.set_size(data.len() as u64);
                header.set_cksum();
                tar.append(&header, data.as_bytes()).unwrap();
            }
        }
        tar.into_inner().unwrap()
    }

    fn unpack(bytes: Vec<u8>) -> Result<(tempfile::TempDir, PathBuf)> {
        let dir = tempfile::tempdir()?;
        let tool = extract_tar(
            Box::new(Cursor::new(bytes)),
            &dir.path().join("payload"),
            &AtomicBool::new(false),
        )?;
        Ok((dir, tool))
    }

    #[test]
    fn accepts_internal_links_and_preserves_executable_bits() {
        let (_dir, tool) = unpack(tar(&[
            ("tool/bin/proton", b'0', "exe"),
            ("tool/proton", b'2', "bin/proton"),
            ("tool/copy", b'1', "tool/bin/proton"),
        ]))
        .unwrap();
        assert_eq!(fs::read(tool.join("proton")).unwrap(), b"exe");
        assert_ne!(
            fs::metadata(tool.join("copy"))
                .unwrap()
                .permissions()
                .mode()
                & 0o111,
            0
        );
    }

    #[test]
    fn rejects_escaping_links_devices_and_duplicate_paths() {
        assert!(
            unpack(tar(&[
                ("tool/a", b'0', "x"),
                ("tool/link", b'2', "../../escape")
            ]))
            .is_err()
        );
        assert!(
            unpack(tar(&[
                ("tool/a", b'0', "x"),
                ("tool/link", b'2', "/etc/passwd")
            ]))
            .is_err()
        );
        assert!(unpack(tar(&[("tool/device", b'3', "")])).is_err());
        assert!(unpack(tar(&[("tool/a", b'0', "x"), ("tool/a", b'0', "y")])).is_err());
        assert!(
            unpack(tar(&[
                ("tool/a", b'0', "x"),
                ("tool/.hyprdeck-tool.json", b'0', "{}")
            ]))
            .is_err()
        );
        assert!(
            unpack(tar(&[
                ("tool/proton", b'0', "source"),
                ("tool/.hyprdeck-prefix-parser.json", b'0', "{}")
            ]))
            .is_err()
        );
    }

    #[test]
    fn rejects_symlink_parent_composition_and_cycles() {
        assert!(
            unpack(tar(&[
                ("tool/a", b'0', "x"),
                ("tool/dir", b'2', "."),
                ("tool/link", b'2', "dir/../outside")
            ]))
            .is_err()
        );
        assert!(
            unpack(tar(&[
                ("tool/a", b'0', "x"),
                ("tool/b", b'2', "c"),
                ("tool/c", b'2', "b")
            ]))
            .is_err()
        );
        assert!(unpack(tar(&[("tool/a", b'2', "b"), ("tool/a/exe", b'0', "bad")])).is_err());
    }

    #[test]
    fn rejects_path_traversal_and_pre_cancelled_extraction() {
        assert!(relative(Path::new("../outside")).is_err());
        assert!(relative(Path::new("/absolute")).is_err());
        let dir = tempfile::tempdir().unwrap();
        let mut archive = tar::Builder::new(Vec::new());
        archive.finish().unwrap();
        assert!(
            extract_tar(
                Box::new(Cursor::new(archive.into_inner().unwrap())),
                &dir.path().join("payload"),
                &AtomicBool::new(true)
            )
            .is_err()
        );
    }

    #[test]
    fn ci_wrapper_requires_one_safe_binary_tar() {
        fn wrapper(names: &[&str], payload: &[u8]) -> Vec<u8> {
            let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
            for name in names {
                zip.start_file(
                    *name,
                    zip::write::SimpleFileOptions::default().unix_permissions(0o644),
                )
                .unwrap();
                zip.write_all(payload).unwrap();
            }
            zip.finish().unwrap().into_inner()
        }
        let payload = tar(&[("tool/proton", b'0', "binary")]);
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("artifact.zip");
        fs::write(&input, wrapper(&["proton-build.tar"], &payload)).unwrap();
        let tool = extract(
            &input,
            "artifact.zip",
            &dir.path().join("valid"),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(fs::read(tool.join("proton")).unwrap(), b"binary");
        for (index, names) in [
            vec!["../escape.tar"],
            vec!["a.tar", "b.tar"],
            vec!["sources.zip"],
        ]
        .into_iter()
        .enumerate()
        {
            fs::write(&input, wrapper(&names, &payload)).unwrap();
            assert!(
                extract(
                    &input,
                    "artifact.zip",
                    &dir.path().join(format!("invalid-{index}")),
                    &AtomicBool::new(false)
                )
                .is_err()
            );
        }
    }
}
