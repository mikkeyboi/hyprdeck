//! Read-only Steam inventory. A configured default is not proof that a game uses Proton.

use anyhow::{Context, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, OpenOptions};
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Component, Path, PathBuf};

use crate::proton::{Target, TargetKind};

#[derive(Clone, Debug)]
pub struct Game {
    pub app_id: u32,
    pub name: String,
    pub library: PathBuf,
    pub install_path: PathBuf,
    pub compatdata: PathBuf,
    /// Only an unambiguous explicit assignment, never an inferred default.
    pub tool: Option<String>,
    pub assignment: String,
    pub steam_root: PathBuf,
}

impl Game {
    pub fn protondb_url(&self) -> String {
        format!("https://www.protondb.com/app/{}", self.app_id)
    }
}

#[derive(Clone, Debug)]
pub struct ToolMapping {
    /// Zero denotes Steam's configured default, not a game's actual tool.
    pub app_id: u32,
    pub tool: Option<String>,
    pub source: String,
    pub steam_root: PathBuf,
}

#[derive(Clone, Debug)]
pub struct SteamTool {
    pub app_id: u32,
    pub name: String,
    pub internal_name: String,
    pub path: PathBuf,
    pub steam_root: PathBuf,
    pub target_id: String,
}

#[derive(Clone, Debug, Default)]
pub struct Inventory {
    pub games: Vec<Game>,
    pub mappings: Vec<ToolMapping>,
    pub steam_tools: Vec<SteamTool>,
    /// Malformed or inaccessible inputs are reported without configuration values.
    pub warnings: Vec<String>,
}

pub fn games(targets: &[Target]) -> Result<Vec<Game>> {
    let inventory = inventory(targets)?;
    require_complete(&inventory)?;
    Ok(inventory.games)
}

fn require_complete(inventory: &Inventory) -> Result<()> {
    if !inventory.warnings.is_empty() {
        bail!(
            "Steam inventory is incomplete: {}",
            inventory.warnings.join("; ")
        );
    }
    Ok(())
}

pub fn inventory(targets: &[Target]) -> Result<Inventory> {
    let mut result = Inventory::default();
    let mut roots = HashSet::new();
    for target in targets
        .iter()
        .filter(|target| target.kind == TargetKind::Steam)
    {
        let root = match crate::proton::steam_root(target) {
            Ok(root) => root,
            Err(error) => {
                result.warnings.push(format!(
                    "Cannot establish this Steam target's installation owner: {error:#}"
                ));
                continue;
            }
        };
        // Explicit custom tool directories do not necessarily belong to Steam.
        match fs::metadata(root.join("steamapps")) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                result.warnings.push(format!(
                    "Steam library entry {} is not a directory",
                    root.join("steamapps").display()
                ));
                continue;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                result.warnings.push(format!(
                    "Cannot inspect Steam library {}: {error}",
                    root.display()
                ));
                continue;
            }
        }
        if !roots.insert(root.clone()) {
            continue;
        }
        let registrations = registered_tools(&root);
        let mut mappings = Vec::new();
        read_mappings(
            &root.join("config/config.vdf"),
            "Steam configuration",
            &root,
            &mut mappings,
            &mut result.warnings,
        );
        let userdata = root.join("userdata");
        match fs::read_dir(&userdata) {
            Ok(entries) => {
                for entry in entries {
                    match entry {
                        Ok(entry)
                            if entry.file_name().to_str().is_some_and(|name| {
                                !name.is_empty() && name.bytes().all(|byte| byte.is_ascii_digit())
                            }) =>
                        {
                            read_mappings(
                                &entry.path().join("config/localconfig.vdf"),
                                "User-local configuration",
                                &root,
                                &mut mappings,
                                &mut result.warnings,
                            );
                        }
                        Ok(_) => {}
                        Err(error) => result.warnings.push(format!(
                            "Cannot enumerate Steam user configurations: {error}"
                        )),
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => result.warnings.push(format!(
                "Cannot enumerate Steam user configurations: {error}"
            )),
        }
        let libraries = libraries(&root, &mut result.warnings);
        for library in libraries {
            let steamapps = library.join("steamapps");
            let entries = match fs::read_dir(&steamapps) {
                Ok(entries) => entries,
                Err(error) => {
                    result.warnings.push(format!(
                        "Cannot read Steam library {}: {error}",
                        library.display()
                    ));
                    continue;
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(error) => {
                        result
                            .warnings
                            .push(format!("Cannot enumerate Steam manifests: {error}"));
                        continue;
                    }
                };
                let filename = entry.file_name();
                let Some(filename) = filename.to_str() else {
                    continue;
                };
                let Some(id) = filename
                    .strip_prefix("appmanifest_")
                    .and_then(|name| name.strip_suffix(".acf"))
                else {
                    continue;
                };
                let Ok(app_id) = id.parse::<u32>() else {
                    continue;
                };
                match read_manifest(
                    &entry.path(),
                    app_id,
                    &library,
                    &root,
                    &target.id,
                    &mappings,
                    &registrations,
                ) {
                    Ok(Manifest::Game(game)) => result.games.push(game),
                    Ok(Manifest::Tools(tools)) => result.steam_tools.extend(tools),
                    Err(error) => result.warnings.push(format!(
                        "Cannot read manifest {}: {error:#}",
                        entry.path().display()
                    )),
                }
            }
        }
        result.mappings.extend(mappings);
    }
    result
        .games
        .sort_by_cached_key(|game| (game.name.to_lowercase(), game.app_id));
    result.steam_tools.sort_by(|a, b| {
        a.name
            .cmp(&b.name)
            .then(a.internal_name.cmp(&b.internal_name))
    });
    result.mappings.sort_by(|a, b| {
        a.steam_root
            .cmp(&b.steam_root)
            .then(a.app_id.cmp(&b.app_id))
            .then(a.source.cmp(&b.source))
            .then(a.tool.cmp(&b.tool))
    });
    Ok(result)
}

fn libraries(root: &Path, warnings: &mut Vec<String>) -> Vec<PathBuf> {
    let mut paths = vec![root.to_path_buf()];
    let current = root.join("steamapps/libraryfolders.vdf");
    let legacy = root.join("config/libraryfolders.vdf");
    let (path, contents) = match read_vdf_optional(&current) {
        Ok(None) => (&legacy, read_vdf_optional(&legacy)),
        contents => (&current, contents),
    };
    match contents {
        Ok(Some(vdf)) => {
            let folders = vdf.get("libraryfolders").unwrap_or(&vdf);
            match folders.entries() {
                Some(entries) => {
                    for (key, value) in entries {
                        if key.parse::<u32>().is_err() {
                            continue;
                        }
                        let value = value
                            .text()
                            .or_else(|| value.get("path").and_then(Vdf::text));
                        match value {
                            Some(path) if Path::new(path).is_absolute() => {
                                paths.push(PathBuf::from(path))
                            }
                            _ => warnings.push(
                                "Steam libraryfolders contains an invalid library path".to_owned(),
                            ),
                        }
                    }
                }
                None => warnings.push("Steam libraryfolders is not an object".to_owned()),
            }
        }
        Ok(None) => {}
        Err(error) => warnings.push(format!(
            "Cannot read Steam library list {}: {error:#}",
            path.display()
        )),
    }
    let mut seen = HashSet::new();
    paths
        .into_iter()
        .filter_map(|path| match fs::canonicalize(&path) {
            Ok(canonical) if seen.insert(canonical.clone()) => Some(canonical),
            Ok(_) => None,
            Err(error) => {
                warnings.push(format!(
                    "Cannot access Steam library {}: {error}",
                    path.display()
                ));
                None
            }
        })
        .collect()
}

fn read_mappings(
    path: &Path,
    source: &str,
    root: &Path,
    mappings: &mut Vec<ToolMapping>,
    warnings: &mut Vec<String>,
) {
    match read_vdf_optional(path) {
        Ok(Some(vdf)) => collect_mappings(&vdf, source, root, mappings, warnings),
        Ok(None) => {}
        Err(error) => warnings.push(format!(
            "Cannot read Steam compatibility assignments {}: {error:#}",
            path.display()
        )),
    }
}

fn collect_mappings(
    vdf: &Vdf,
    source: &str,
    root: &Path,
    mappings: &mut Vec<ToolMapping>,
    warnings: &mut Vec<String>,
) {
    let Some(entries) = vdf.entries() else { return };
    for (key, value) in entries {
        if !key.eq_ignore_ascii_case("CompatToolMapping") {
            collect_mappings(value, source, root, mappings, warnings);
            continue;
        }
        let Some(entries) = value.entries() else {
            warnings.push(format!(
                "{source} contains a malformed compatibility-tool mapping"
            ));
            continue;
        };
        for (key, value) in entries {
            let Ok(app_id) = key.parse::<u32>() else {
                warnings.push(format!(
                    "{source} contains an invalid compatibility application ID"
                ));
                continue;
            };
            let Some(name) = value.get("name").and_then(Vdf::text) else {
                warnings.push(format!(
                    "{source} contains a compatibility mapping without a tool name"
                ));
                continue;
            };
            mappings.push(ToolMapping {
                app_id,
                tool: (!name.is_empty()).then(|| name.to_owned()),
                source: source.to_owned(),
                steam_root: root.to_path_buf(),
            });
        }
    }
}

fn assignment(app_id: u32, mappings: &[ToolMapping]) -> (Option<String>, String) {
    let explicit: BTreeSet<_> = mappings
        .iter()
        .filter(|mapping| mapping.app_id == app_id)
        .map(|mapping| mapping.tool.clone())
        .collect();
    match explicit.len() {
        1 => {
            let tool = explicit.into_iter().next().flatten();
            let label = if tool.is_some() {
                "explicit"
            } else {
                "explicit no tool (native status unknown)"
            };
            return (tool, label.to_owned());
        }
        0 => {}
        _ => {
            return (
                None,
                "conflicting explicit assignments across Steam configurations".to_owned(),
            );
        }
    }
    let defaults: BTreeSet<_> = mappings
        .iter()
        .filter(|mapping| mapping.app_id == 0)
        .map(|mapping| mapping.tool.clone())
        .collect();
    match defaults.len() {
        0 => (None, "unknown automatic".to_owned()),
        1 => {
            let name = defaults
                .into_iter()
                .next()
                .flatten()
                .unwrap_or_else(|| "no tool".to_owned());
            (
                None,
                format!("configured default: {name} (actual use unknown)"),
            )
        }
        _ => (
            None,
            "conflicting configured defaults (actual use unknown)".to_owned(),
        ),
    }
}

enum Manifest {
    Game(Game),
    Tools(Vec<SteamTool>),
}

fn read_manifest(
    path: &Path,
    expected_id: u32,
    library: &Path,
    root: &Path,
    target_id: &str,
    mappings: &[ToolMapping],
    registrations: &BTreeMap<u32, String>,
) -> Result<Manifest> {
    let vdf = read_vdf_optional(path)?.context("Manifest disappeared")?;
    let app = vdf.get("AppState").context("Missing AppState")?;
    let app_id = app
        .get("appid")
        .and_then(Vdf::text)
        .context("Missing application ID")?
        .parse::<u32>()
        .context("Invalid application ID")?;
    if app_id == 0 || app_id != expected_id {
        bail!("Manifest application ID does not match its filename");
    }
    let name = app
        .get("name")
        .and_then(Vdf::text)
        .context("Missing application name")?
        .to_owned();
    let install_dir = app
        .get("installdir")
        .and_then(Vdf::text)
        .context("Missing installation directory")?;
    let install_dir = Path::new(install_dir);
    if install_dir.as_os_str().is_empty()
        || !install_dir
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        bail!("Installation directory is not a safe relative path");
    }
    let install_path = library.join("steamapps/common").join(install_dir);
    if let Some(tools) = read_vdf_optional(&install_path.join("compatibilitytool.vdf"))? {
        let compatibility = tools
            .get("compatibilitytools")
            .context("Invalid compatibilitytool.vdf")?;
        let entries = compatibility
            .get("compat_tools")
            .unwrap_or(compatibility)
            .entries()
            .context("Invalid compatibility tool definitions")?;
        let mut result = Vec::new();
        for (internal_name, details) in entries {
            if details.entries().is_none() {
                bail!("Invalid compatibility tool entry");
            }
            let tool_path = match details.get("install_path").and_then(Vdf::text) {
                Some(".") | None => install_path.clone(),
                Some(relative)
                    if !Path::new(relative).as_os_str().is_empty()
                        && Path::new(relative)
                            .components()
                            .all(|component| matches!(component, Component::Normal(_))) =>
                {
                    install_path.join(relative)
                }
                _ => bail!("Unsafe compatibility tool installation path"),
            };
            result.push(SteamTool {
                app_id,
                name: details
                    .get("display_name")
                    .and_then(Vdf::text)
                    .unwrap_or(&name)
                    .to_owned(),
                internal_name: internal_name.clone(),
                path: tool_path,
                steam_root: root.to_path_buf(),
                target_id: target_id.to_owned(),
            });
        }
        if result.is_empty() {
            bail!("Compatibility tool manifest contains no tools");
        }
        return Ok(Manifest::Tools(result));
    }
    // Steam-managed Proton and runtime packages ship toolmanifest.vdf without
    // the compatibilitytool.vdf used by third-party tools. Their actual internal
    // IDs are registered by Steam; never invent one from a display/folder name.
    if read_vdf_optional(&install_path.join("toolmanifest.vdf"))?
        .is_some_and(|vdf| vdf.get("manifest").is_some())
    {
        return Ok(Manifest::Tools(vec![SteamTool {
            app_id,
            name,
            internal_name: registrations.get(&app_id).cloned().unwrap_or_default(),
            path: install_path,
            steam_root: root.to_path_buf(),
            target_id: target_id.to_owned(),
        }]));
    }
    let (tool, assignment) = assignment(app_id, mappings);
    Ok(Manifest::Game(Game {
        app_id,
        name,
        library: library.to_path_buf(),
        install_path,
        compatdata: library
            .join("steamapps/compatdata")
            .join(app_id.to_string()),
        tool,
        assignment,
        steam_root: root.to_path_buf(),
    }))
}

fn registered_tools(root: &Path) -> BTreeMap<u32, String> {
    let mut registrations = BTreeMap::new();
    for log in ["compat_log.previous.txt", "compat_log.txt"] {
        let Ok(mut file) = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(root.join("logs").join(log))
        else {
            continue;
        };
        let Ok(metadata) = file.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let offset = metadata.len().saturating_sub(2 * 1024 * 1024);
        if file.seek(SeekFrom::Start(offset)).is_err() {
            continue;
        }
        let mut bytes = Vec::new();
        if file.take(2 * 1024 * 1024).read_to_end(&mut bytes).is_err() {
            continue;
        }
        for line in String::from_utf8_lossy(&bytes).lines() {
            let registration = line
                .split_once("Posting queued tool registration callback ")
                .or_else(|| line.split_once("Posting tool registration callback "));
            let Some((_, value)) = registration else {
                continue;
            };
            let mut fields = value.split_whitespace();
            let Some(id) = fields
                .next()
                .and_then(|id| id.parse::<u32>().ok())
                .filter(|id| *id != 0)
            else {
                continue;
            };
            let Some(name) = fields.next().filter(|name| {
                name.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'_' | b'-' | b'.'))
            }) else {
                continue;
            };
            registrations.insert(id, name.to_owned());
        }
    }
    registrations
}

fn read_vdf_optional(path: &Path) -> Result<Option<Vdf>> {
    let file = match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > 16 * 1024 * 1024 {
        bail!("VDF must be a regular file no larger than 16 MiB");
    }
    let mut text = String::new();
    file.take(16 * 1024 * 1024 + 1)
        .read_to_string(&mut text)
        .context("Cannot read VDF text")?;
    parse_vdf(&text).map(Some)
}

/// Minimal Valve KeyValues parser; errors never echo configuration tokens.
#[derive(Clone, Debug)]
pub(crate) enum Vdf {
    String(String),
    Object(Vec<(String, Vdf)>),
}

impl Vdf {
    pub(crate) fn get(&self, key: &str) -> Option<&Vdf> {
        self.entries()?
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(key))
            .map(|(_, value)| value)
    }

    pub(crate) fn text(&self) -> Option<&str> {
        match self {
            Self::String(value) => Some(value),
            _ => None,
        }
    }

    pub(crate) fn entries(&self) -> Option<&[(String, Vdf)]> {
        match self {
            Self::Object(entries) => Some(entries),
            _ => None,
        }
    }
}

#[derive(Debug)]
enum Token {
    Text(String),
    Open,
    Close,
}

pub(crate) fn parse_vdf(input: &str) -> Result<Vdf> {
    if input.len() > 16 * 1024 * 1024 {
        bail!("VDF exceeds 16 MiB");
    }
    let mut parser = Parser {
        input: input
            .strip_prefix('\u{feff}')
            .unwrap_or(input)
            .chars()
            .peekable(),
        line: 1,
    };
    let result = parser.object(0, false)?;
    Ok(Vdf::Object(result))
}

struct Parser<'a> {
    input: std::iter::Peekable<std::str::Chars<'a>>,
    line: usize,
}

impl Parser<'_> {
    fn next(&mut self) -> Option<char> {
        let character = self.input.next()?;
        if character == '\n' {
            self.line += 1;
        }
        Some(character)
    }

    fn error(&self, message: &str) -> anyhow::Error {
        anyhow!("{message} at VDF line {}", self.line)
    }

    fn token(&mut self) -> Result<Option<Token>> {
        loop {
            let Some(character) = self.next() else {
                return Ok(None);
            };
            if character.is_whitespace() {
                continue;
            }
            if character == '/' && self.input.peek() == Some(&'/') {
                while self.next().is_some_and(|character| character != '\n') {}
                continue;
            }
            if character == '/' && self.input.peek() == Some(&'*') {
                self.next();
                loop {
                    let character = self
                        .next()
                        .ok_or_else(|| self.error("Unterminated comment"))?;
                    if character == '*' && self.input.peek() == Some(&'/') {
                        self.next();
                        break;
                    }
                }
                continue;
            }
            if character == '[' {
                loop {
                    match self.next() {
                        Some(']') => break,
                        Some(_) => {}
                        None => return Err(self.error("Unterminated VDF condition")),
                    }
                }
                continue;
            }
            return Ok(Some(match character {
                '{' => Token::Open,
                '}' => Token::Close,
                '"' => {
                    let mut text = String::new();
                    loop {
                        match self
                            .next()
                            .ok_or_else(|| self.error("Unterminated quoted string"))?
                        {
                            '"' => break,
                            '\\' => {
                                let escaped = self
                                    .next()
                                    .ok_or_else(|| self.error("Unterminated escape"))?;
                                match escaped {
                                    '"' => text.push('"'),
                                    '\\' => text.push('\\'),
                                    'n' => text.push('\n'),
                                    'r' => text.push('\r'),
                                    't' => text.push('\t'),
                                    other => {
                                        text.push('\\');
                                        text.push(other);
                                    }
                                }
                            }
                            character => text.push(character),
                        }
                    }
                    Token::Text(text)
                }
                character => {
                    let mut text = String::from(character);
                    while self.input.peek().is_some_and(|character| {
                        !character.is_whitespace() && !matches!(character, '{' | '}' | '[')
                    }) {
                        text.push(self.next().unwrap());
                    }
                    if text.starts_with('#') {
                        return Err(self.error("External VDF directives are unsupported"));
                    }
                    Token::Text(text)
                }
            }));
        }
    }

    fn object(&mut self, depth: usize, nested: bool) -> Result<Vec<(String, Vdf)>> {
        if depth > 128 {
            return Err(self.error("VDF nesting exceeds 128 levels"));
        }
        let mut entries = Vec::new();
        loop {
            let key = match self.token()? {
                None if !nested => return Ok(entries),
                Some(Token::Close) if nested => return Ok(entries),
                Some(Token::Text(key)) => key,
                _ => return Err(self.error("Expected VDF key")),
            };
            let value = match self.token()? {
                Some(Token::Text(text)) => Vdf::String(text),
                Some(Token::Open) => Vdf::Object(self.object(depth + 1, true)?),
                _ => return Err(self.error("Expected VDF value")),
            };
            entries.push((key, value));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(root: &Path) -> Target {
        Target {
            id: "steam-fixture".into(),
            name: "Fixture".into(),
            root: root.join("compatibilitytools.d"),
            kind: TargetKind::Steam,
            launcher_root: Some(root.to_path_buf()),
            steam_packaging: None,
        }
    }

    fn manifest(library: &Path, id: u32, name: &str) {
        fs::create_dir_all(library.join("steamapps")).unwrap();
        fs::write(
            library.join(format!("steamapps/appmanifest_{id}.acf")),
            format!(r#""AppState" {{ "appid" "{id}" "name" "{name}" "installdir" "game-{id}" }}"#),
        )
        .unwrap();
    }

    #[test]
    fn nested_escaped_vdf_and_conditions() {
        let parsed = parse_vdf(
            r#"// comment
            "root" { "quoted" "a\"b\\c" "nested" { unquoted value } [$LINUX] }
        "#,
        )
        .unwrap();
        let root = parsed.get("ROOT").unwrap();
        assert_eq!(root.get("quoted").unwrap().text(), Some("a\"b\\c"));
        assert_eq!(
            root.get("nested").unwrap().get("unquoted").unwrap().text(),
            Some("value")
        );
        assert!(parse_vdf("root { key").is_err());
        assert!(parse_vdf("key value [$LINUX").is_err());
        assert!(
            parse_vdf("key \"private-token")
                .unwrap_err()
                .to_string()
                .find("private-token")
                .is_none()
        );
    }

    #[test]
    fn libraries_assignments_and_managed_tools() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Steam");
        let library = temp.path().join("A library");
        fs::create_dir_all(root.join("steamapps")).unwrap();
        fs::create_dir_all(root.join("config")).unwrap();
        fs::write(
            root.join("steamapps/libraryfolders.vdf"),
            format!(
                r#""libraryfolders" {{ "0" {{ "path" "{}" }} "1" "{}" }}"#,
                root.display(),
                library.display()
            ),
        )
        .unwrap();
        fs::write(
            root.join("config/config.vdf"),
            r#""InstallConfigStore" { "Software" { "Valve" { "Steam" { "CompatToolMapping" {
            "0" { "name" "proton_experimental" }
            "42" { "name" "proton-cachyos" }
        } } } } }"#,
        )
        .unwrap();
        manifest(&root, 42, "Explicit game");
        manifest(&library, 43, "Unproven default game");
        manifest(&library, 44, "Proton Experimental");
        let tool = library.join("steamapps/common/game-44");
        fs::create_dir_all(&tool).unwrap();
        fs::write(tool.join("compatibilitytool.vdf"), r#""compatibilitytools" { "compat_tools" { "proton_experimental" { "display_name" "Proton Experimental" "install_path" "." } } }"#).unwrap();
        let inventory = inventory(&[target(&root)]).unwrap();
        assert!(inventory.warnings.is_empty(), "{:?}", inventory.warnings);
        assert_eq!(inventory.games.len(), 2);
        let explicit = inventory
            .games
            .iter()
            .find(|game| game.app_id == 42)
            .unwrap();
        assert_eq!(explicit.tool.as_deref(), Some("proton-cachyos"));
        let unproven = inventory
            .games
            .iter()
            .find(|game| game.app_id == 43)
            .unwrap();
        assert_eq!(unproven.tool, None);
        assert_eq!(unproven.library, fs::canonicalize(&library).unwrap());
        assert_eq!(
            inventory
                .mappings
                .iter()
                .filter(|mapping| mapping.app_id == 0)
                .count(),
            1
        );
        assert_eq!(
            inventory.steam_tools[0].internal_name,
            "proton_experimental"
        );
    }

    #[test]
    fn steam_managed_tools_do_not_require_third_party_manifests() {
        let temp = tempfile::tempdir().unwrap();
        manifest(temp.path(), 701, "Proton Experimental");
        let installed = temp.path().join("steamapps/common/game-701");
        fs::create_dir_all(&installed).unwrap();
        fs::write(
            installed.join("toolmanifest.vdf"),
            r#""manifest" { "commandline" "/proton %verb%" "compatmanager_layer_name" "proton" }"#,
        )
        .unwrap();
        fs::create_dir_all(temp.path().join("logs")).unwrap();
        fs::write(
            temp.path().join("logs/compat_log.txt"),
            "[date] Posting queued tool registration callback 701 proton_experimental\n",
        )
        .unwrap();
        let found = inventory(&[target(temp.path())]).unwrap();
        assert!(found.games.is_empty());
        assert_eq!(found.steam_tools[0].app_id, 701);
        assert_eq!(found.steam_tools[0].internal_name, "proton_experimental");
        assert_eq!(found.steam_tools[0].path, installed);
    }

    #[test]
    fn unknown_and_conflicting_user_assignments_are_not_guessed() {
        let temp = tempfile::tempdir().unwrap();
        manifest(temp.path(), 99, "Unknown game");
        let first = games(&[target(temp.path())]).unwrap();
        assert!(first[0].tool.is_none());
        for (user, tool) in [("1", "proton_a"), ("2", "proton_b")] {
            let path = temp.path().join(format!("userdata/{user}/config"));
            fs::create_dir_all(&path).unwrap();
            fs::write(path.join("localconfig.vdf"), format!(r#""UserLocalConfigStore" {{ "Software" {{ "Valve" {{ "Steam" {{ "CompatToolMapping" {{ "99" {{ "name" "{tool}" }} }} }} }} }} }}"#)).unwrap();
        }
        let games = games(&[target(temp.path())]).unwrap();
        assert!(games[0].tool.is_none());
    }

    #[test]
    fn corrupt_config_is_reported_without_secrets() {
        let temp = tempfile::tempdir().unwrap();
        manifest(temp.path(), 99, "Fixture");
        fs::create_dir_all(temp.path().join("config")).unwrap();
        fs::write(
            temp.path().join("config/config.vdf"),
            "secret_token { \"never-echo-this",
        )
        .unwrap();
        let inventory = inventory(&[target(temp.path())]).unwrap();
        assert_eq!(inventory.games.len(), 1);
        assert_eq!(inventory.warnings.len(), 1);
        assert!(!inventory.warnings[0].contains("never-echo-this"));
        assert!(games(&[target(temp.path())]).is_err());
    }
}
