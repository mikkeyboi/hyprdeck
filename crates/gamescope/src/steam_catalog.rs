//! Read-only official Proton catalog from the selected Steam client's appinfo cache.
//! Unknown cache layouts, old snapshots, and ambiguous series never produce actions.
use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail, ensure};

use super::Target;

const MAX_CACHE: u64 = 512 * 1024 * 1024;
const MAX_SECTION: usize = 16 * 1024 * 1024;
const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Default)]
struct Fields<'a> {
    name: Option<&'a str>,
    kind: Option<&'a str>,
    oslist: Option<&'a str>,
    osarch: Option<&'a str>,
    state: Option<&'a str>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Tool {
    app: u32,
    name: String,
    series: String,
    arch: Option<String>,
}

pub(super) struct Catalog {
    tools: Vec<Tool>,
}

impl Catalog {
    pub(super) fn load(target: &Target) -> Result<Self> {
        let root = super::steam_root(target)?;
        let file = OpenOptions::new().read(true).custom_flags(libc::O_NONBLOCK).open(root.join("appcache/appinfo.vdf"))
            .context("Steam app metadata unavailable; open the selected Steam client to refresh its Tools catalog")?;
        let before = file.metadata()?;
        ensure!(
            before.is_file(),
            "Steam app metadata must be a regular file"
        );
        check_age(before.modified()?, SystemTime::now())?;
        ensure!(
            before.len() <= MAX_CACHE,
            "Steam app metadata exceeds the safety limit"
        );
        let mut reader = BufReader::new(file);
        let catalog = Self::parse(&mut reader, before.len())?;
        let after = reader.get_ref().metadata()?;
        ensure!(
            before.len() == after.len() && before.modified()? == after.modified()?,
            "Steam app metadata changed while reading; refresh the release list"
        );
        Ok(catalog)
    }
    // v27/v28 use inline keys; v29 interns keys in a trailing string table.
    // Section sizes include the fixed header after the size word, not appid/size.
    fn parse(reader: &mut (impl Read + Seek), length: u64) -> Result<Self> {
        let magic = uint(reader)?;
        ensure!(
            matches!(magic, 0x07564427..=0x07564429),
            "Unsupported Steam appinfo format; no catalog guesses are used"
        );
        ensure!(
            uint(reader)? == 1,
            "Steam app metadata is not from the public universe"
        );
        let mut strings = Vec::new();
        let data_end = if magic == 0x07564429 {
            let mut bytes = [0; 8];
            reader.read_exact(&mut bytes)?;
            let offset = u64::from_le_bytes(bytes);
            ensure!(
                offset >= 20
                    && offset.checked_add(4).is_some_and(|end| end <= length)
                    && length - offset <= MAX_SECTION as u64,
                "Invalid Steam appinfo string table"
            );
            reader.seek(SeekFrom::Start(offset))?;
            let count = uint(reader)? as usize;
            ensure!(
                count <= MAX_SECTION / 4,
                "Steam appinfo string table is too large"
            );
            let mut table = Vec::new();
            (&mut *reader)
                .take(length - offset - 4)
                .read_to_end(&mut table)?;
            let mut cursor = Binary {
                bytes: &table,
                strings: None,
            };
            for _ in 0..count {
                strings.push(cursor.string()?.to_owned());
            }
            ensure!(
                cursor.bytes.is_empty(),
                "Unexpected data after Steam appinfo string table"
            );
            reader.seek(SeekFrom::Start(16))?;
            offset
        } else {
            length
        };
        let mut tools = Vec::new();
        let mut seen = BTreeSet::new();
        let mut section = Vec::new();
        loop {
            ensure!(
                reader.stream_position()? + 4 <= data_end,
                "Truncated Steam app catalog"
            );
            let app = uint(reader)?;
            if app == 0 {
                ensure!(
                    reader.stream_position()? == data_end,
                    "Unexpected data after Steam app catalog"
                );
                break;
            }
            ensure!(
                seen.insert(app),
                "Duplicate Steam app metadata; refresh the selected Steam client"
            );
            let size = uint(reader)? as usize;
            let header = if magic == 0x07564427 { 40 } else { 60 };
            ensure!(
                size > header
                    && size <= MAX_SECTION
                    && reader.stream_position()? + size as u64 <= data_end,
                "Invalid Steam appinfo section size"
            );
            section.resize(size, 0);
            reader.read_exact(&mut section)?;
            let mut binary = Binary {
                bytes: &section[header..],
                strings: (magic == 0x07564429).then_some(strings.as_slice()),
            };
            let mut fields = Fields::default();
            binary.object(0, false, &mut fields)?;
            ensure!(
                binary.bytes.is_empty(),
                "Unexpected data after Steam appinfo object"
            );
            if let Some(tool) = tool_from(app, &fields)? {
                tools.push(tool);
            }
        }
        Ok(Self { tools })
    }

    pub(super) fn series(&self) -> BTreeSet<&str> {
        self.tools.iter().map(|tool| tool.series.as_str()).collect()
    }

    pub(super) fn app_for(&self, tag: &str, arch: &str) -> Result<u32> {
        let series =
            release_series(tag).context("Release tag does not identify a Steam Proton series")?;
        ensure!(
            matches!(arch, "x86_64" | "aarch64"),
            "Host CPU architecture is not supported by this Proton manager"
        );
        let candidates = self.tools.iter().filter(|tool| tool.series == series);
        ensure!(
            candidates.clone().next().is_some(),
            "Selected Steam client's catalog does not advertise this Proton series; refresh Steam's Tools metadata"
        );
        let explicit = candidates
            .clone()
            .any(|tool| tool.arch.as_deref() == Some(arch));
        // Unqualified Steam tool names do not advertise a CPU architecture. Steam
        // performs its own compatibility check; explicit architecture metadata wins.
        let mut matching = candidates.filter(|tool| {
            if explicit {
                tool.arch.as_deref() == Some(arch)
            } else {
                tool.arch.is_none()
            }
        });
        let selected = matching
            .next()
            .context("Steam Proton metadata is unavailable for this host architecture")?;
        ensure!(
            matching.next().is_none(),
            "Steam Proton metadata is ambiguous; no install action is offered"
        );
        Ok(selected.app)
    }

    pub(super) fn name_for(&self, app: u32) -> Option<&str> {
        self.tools
            .iter()
            .find(|tool| tool.app == app)
            .map(|tool| tool.name.as_str())
    }

    pub(super) fn validate_app(&self, app: u32, arch: &str) -> Result<()> {
        let tool =
            self.tools.iter().find(|tool| tool.app == app).context(
                "Steam tool is no longer present in the selected client's current metadata",
            )?;
        ensure!(
            self.app_for(&tool.series, arch)? == app,
            "Steam tool no longer matches the selected host architecture"
        );
        Ok(())
    }
}

fn check_age(modified: SystemTime, now: SystemTime) -> Result<()> {
    let age = now
        .duration_since(modified)
        .context("Steam app metadata timestamp is in the future; refresh Steam's Tools catalog")?;
    ensure!(
        age <= MAX_AGE,
        "Steam app metadata is stale; open the selected Steam client to refresh its Tools catalog"
    );
    Ok(())
}

fn release_series(tag: &str) -> Option<&str> {
    if tag == "experimental" {
        return Some(tag);
    }
    let value = tag.strip_prefix("proton-").unwrap_or(tag);
    let series = value.split('-').next()?;
    (!series.is_empty()
        && series
            .split('.')
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit())))
    .then_some(series)
}

fn tool_from(app: u32, fields: &Fields<'_>) -> Result<Option<Tool>> {
    if !fields
        .kind
        .is_some_and(|value| value.eq_ignore_ascii_case("tool"))
        || !fields
            .oslist
            .is_some_and(|value| value.split(',').any(|os| os.trim() == "linux"))
        || fields.state.is_some_and(|value| value != "released")
    {
        return Ok(None);
    }
    let Some(name) = fields.name else {
        return Ok(None);
    };
    let Some(mut version) = name.strip_prefix("Proton ") else {
        return Ok(None);
    };
    let mut arch = None;
    if let Some((base, suffix)) = version.rsplit_once(" (") {
        version = base;
        arch = match suffix.strip_suffix(')') {
            Some("ARM64" | "aarch64" | "arm64") => Some("aarch64".into()),
            Some("x86_64" | "AMD64") => Some("x86_64".into()),
            _ => return Ok(None),
        };
    }
    let advertised = fields.osarch.and_then(|value| match value {
        "aarch64" | "arm64" => Some("aarch64"),
        "x86_64" | "amd64" => Some("x86_64"),
        _ => None,
    });
    if let Some(advertised) = advertised {
        ensure!(
            arch.as_deref().is_none_or(|arch| arch == advertised),
            "Conflicting architecture fields in Steam Proton metadata"
        );
        arch = Some(advertised.into());
    }
    let series = if version == "Experimental" {
        "experimental"
    } else {
        let Some(series) = release_series(version) else {
            return Ok(None);
        };
        series
    };
    Ok(Some(Tool {
        app,
        name: name.into(),
        series: series.into(),
        arch,
    }))
}

fn uint(reader: &mut impl Read) -> Result<u32> {
    let mut bytes = [0; 4];
    reader.read_exact(&mut bytes)?;
    Ok(u32::from_le_bytes(bytes))
}

struct Binary<'a> {
    bytes: &'a [u8],
    strings: Option<&'a [String]>,
}

impl<'a> Binary<'a> {
    fn take(&mut self, size: usize) -> Result<&'a [u8]> {
        ensure!(size <= self.bytes.len(), "Truncated Steam binary keyvalue");
        let (value, rest) = self.bytes.split_at(size);
        self.bytes = rest;
        Ok(value)
    }

    fn string(&mut self) -> Result<&'a str> {
        let end = self
            .bytes
            .iter()
            .position(|byte| *byte == 0)
            .context("Unterminated Steam binary string")?;
        let value = self.take(end + 1)?;
        Ok(std::str::from_utf8(&value[..end])?)
    }

    fn key(&mut self) -> Result<&'a str> {
        if let Some(strings) = self.strings {
            let index = u32::from_le_bytes(self.take(4)?.try_into()?) as usize;
            Ok(strings
                .get(index)
                .context("Invalid Steam appinfo key index")?
                .as_str())
        } else {
            self.string()
        }
    }

    fn object(&mut self, depth: usize, common: bool, fields: &mut Fields<'a>) -> Result<()> {
        ensure!(depth < 64, "Steam appinfo nesting exceeds the safety limit");
        loop {
            let kind = self.take(1)?[0];
            if matches!(kind, 8 | 11) {
                return Ok(());
            }
            let key = self.key()?;
            match kind {
                0 => self.object(depth + 1, key == "common" && depth == 1, fields)?,
                1 => {
                    let value = self.string()?;
                    if common {
                        let field = match key {
                            "name" => Some(&mut fields.name),
                            "type" => Some(&mut fields.kind),
                            "oslist" => Some(&mut fields.oslist),
                            "osarch" => Some(&mut fields.osarch),
                            "releasestate" => Some(&mut fields.state),
                            _ => None,
                        };
                        if let Some(field) = field {
                            ensure!(
                                field.replace(value).is_none(),
                                "Duplicate Steam tool metadata field"
                            );
                        }
                    }
                }
                2 | 3 | 4 | 6 => {
                    self.take(4)?;
                }
                7 | 10 => {
                    self.take(8)?;
                }
                _ => bail!(
                    "Unsupported Steam binary keyvalue type {kind}; refresh/update Steam metadata support"
                ),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn fields(name: &str) -> Fields<'_> {
        Fields {
            name: Some(name),
            kind: Some("Tool"),
            oslist: Some("linux"),
            state: Some("released"),
            ..Fields::default()
        }
    }

    fn catalog(entries: &[(u32, &str)]) -> Catalog {
        Catalog {
            tools: entries
                .iter()
                .filter_map(|(app, name)| tool_from(*app, &fields(name)).unwrap())
                .collect(),
        }
    }

    #[test]
    fn future_series_and_experimental_come_only_from_metadata() {
        let catalog = catalog(&[
            (701, "Proton 42.5"),
            (702, "Proton Experimental"),
            (703, "Proton 42.5 (ARM64)"),
        ]);
        assert_eq!(catalog.app_for("proton-42.5-7z", "x86_64").unwrap(), 701);
        assert_eq!(catalog.app_for("proton-42.5-7z", "aarch64").unwrap(), 703);
        assert_eq!(catalog.app_for("experimental", "x86_64").unwrap(), 702);
        assert!(catalog.app_for("proton-43.0-1", "x86_64").is_err());
        assert!(catalog.app_for("proton-42.50-1", "x86_64").is_err());
        catalog.validate_app(701, "x86_64").unwrap();
        assert!(catalog.validate_app(703, "x86_64").is_err());
    }

    #[test]
    fn ambiguous_unknown_arch_and_unavailable_catalogs_have_no_action() {
        assert!(catalog(&[]).app_for("experimental", "x86_64").is_err());
        assert!(
            catalog(&[(710, "Proton 42.5"), (711, "Proton 42.5")])
                .app_for("42.5", "x86_64")
                .is_err()
        );
        assert!(
            catalog(&[(710, "Proton 42.5 (ARM64)")])
                .app_for("42.5", "x86_64")
                .is_err()
        );
        assert!(
            catalog(&[(710, "Proton 42.5 (unknown)")])
                .app_for("42.5", "x86_64")
                .is_err()
        );
        let mut game = fields("Proton 42.5");
        game.kind = Some("Game");
        assert!(tool_from(710, &game).unwrap().is_none());
    }

    #[test]
    fn stale_and_future_cache_timestamps_are_unavailable() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(2_000_000);
        assert!(check_age(now - MAX_AGE, now).is_ok());
        assert!(check_age(now - MAX_AGE - Duration::from_secs(1), now).is_err());
        assert!(check_age(now + Duration::from_secs(1), now).is_err());
    }

    fn fixture(magic: u32) -> Vec<u8> {
        let strings = [
            "appinfo",
            "common",
            "name",
            "type",
            "oslist",
            "releasestate",
        ];
        let mut object = Vec::new();
        let mut key = |kind, value: &str, data: Option<&str>| {
            object.push(kind);
            if magic == 0x07564429 {
                object.extend_from_slice(
                    &(strings.iter().position(|key| *key == value).unwrap() as u32).to_le_bytes(),
                );
            } else {
                object.extend_from_slice(value.as_bytes());
                object.push(0);
            }
            if let Some(data) = data {
                object.extend_from_slice(data.as_bytes());
                object.push(0);
            }
        };
        key(0, "appinfo", None);
        key(0, "common", None);
        for (field, value) in [
            ("name", "Proton 42.5"),
            ("type", "Tool"),
            ("oslist", "linux"),
            ("releasestate", "released"),
        ] {
            key(1, field, Some(value));
        }
        object.extend_from_slice(&[8, 8, 8]);
        let header = if magic == 0x07564427 { 40 } else { 60 };
        let start = if magic == 0x07564429 { 16 } else { 8 };
        let table = start + 8 + header + object.len() + 4;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&magic.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        if magic == 0x07564429 {
            bytes.extend_from_slice(&(table as u64).to_le_bytes());
        }
        bytes.extend_from_slice(&701u32.to_le_bytes());
        bytes.extend_from_slice(&((header + object.len()) as u32).to_le_bytes());
        bytes.resize(bytes.len() + header, 0);
        bytes.extend_from_slice(&object);
        bytes.extend_from_slice(&0u32.to_le_bytes());
        if magic == 0x07564429 {
            bytes.extend_from_slice(&(strings.len() as u32).to_le_bytes());
            for string in strings {
                bytes.extend_from_slice(string.as_bytes());
                bytes.push(0);
            }
        }
        bytes
    }

    #[test]
    fn reads_real_binary_cache_layouts_without_network() {
        for magic in 0x07564427..=0x07564429 {
            let bytes = fixture(magic);
            let catalog = Catalog::parse(&mut Cursor::new(&bytes), bytes.len() as u64).unwrap();
            assert_eq!(catalog.app_for("proton-42.5-7z", "x86_64").unwrap(), 701);
            assert!(
                Catalog::parse(
                    &mut Cursor::new(&bytes[..bytes.len() - 1]),
                    bytes.len() as u64 - 1
                )
                .is_err()
            );
        }
        assert!(Catalog::parse(&mut Cursor::new([0u8; 8]), 8).is_err());
    }

    #[test]
    fn selected_owner_metadata_survives_external_tool_directory_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let owner = directory.path().join("client");
        let tools = directory.path().join("external-tools");
        std::fs::create_dir_all(owner.join("steamapps")).unwrap();
        std::fs::create_dir_all(owner.join("appcache")).unwrap();
        std::fs::create_dir(&tools).unwrap();
        std::os::unix::fs::symlink(&tools, owner.join("compatibilitytools.d")).unwrap();
        let target = Target {
            id: "fixture".into(),
            name: "Arbitrary name".into(),
            root: tools,
            kind: super::super::TargetKind::Steam,
            launcher_root: Some(owner.clone()),
            steam_packaging: Some(super::super::SteamPackaging::Native),
        };
        assert!(Catalog::load(&target).is_err());
        std::fs::write(owner.join("appcache/appinfo.vdf"), fixture(0x07564429)).unwrap();
        assert_eq!(
            Catalog::load(&target)
                .unwrap()
                .app_for("42.5", "x86_64")
                .unwrap(),
            701
        );
        let mut changed = target;
        changed.launcher_root = Some(directory.path().to_path_buf());
        assert!(Catalog::load(&changed).is_err());
    }
}
