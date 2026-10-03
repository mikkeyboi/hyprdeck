//! XDG autostart entries (`~/.config/autostart`, `$XDG_CONFIG_DIRS/autostart`),
//! evaluated the way `systemd-xdg-autostart-generator` does it (systemd starts
//! them as `app-<escaped>@autostart.service` units).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use hyprdeck_core::store;

use crate::desktop::{self, DesktopFile};

pub fn user_dir() -> PathBuf {
    dirs::config_dir()
        .unwrap_or_else(|| store::home().join(".config"))
        .join("autostart")
}

pub fn system_dirs() -> Vec<PathBuf> {
    let dirs = std::env::var("XDG_CONFIG_DIRS")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "/etc/xdg".into());
    dirs.split(':')
        .filter(|d| !d.is_empty())
        .map(|d| Path::new(d).join("autostart"))
        .collect()
}

/// Desktops the session matches against `OnlyShowIn`/`NotShowIn`.
pub fn current_desktops() -> Vec<String> {
    match std::env::var("XDG_CURRENT_DESKTOP") {
        Ok(v) if !v.is_empty() => v.split(':').map(str::to_owned).collect(),
        _ => vec!["Hyprland".to_owned()],
    }
}

/// Whether the entry is switched on by the user, and what (if anything) keeps it
/// from starting in this session regardless of the switch.
#[derive(Debug, Clone, PartialEq)]
pub struct Eval {
    /// Not `Hidden=true` and not `X-GNOME-Autostart-enabled=false`.
    pub enabled: bool,
    pub blocker: Option<String>,
}

/// Mirror of the generator's checks (hidden, X-systemd-skip, Type, Exec,
/// TryExec, Exec binary) plus the `systemd-xdg-autostart-condition`
/// `OnlyShowIn`/`NotShowIn` test it attaches as `ExecCondition`.
pub fn evaluate(f: &DesktopFile, desktops: &[String], found: impl Fn(&str) -> bool) -> Eval {
    let enabled = f.get_bool("Hidden") != Some(true)
        && f.get_bool("X-GNOME-Autostart-enabled") != Some(false);
    let blocker = blocker(f, desktops, found);
    Eval { enabled, blocker }
}

fn blocker(f: &DesktopFile, desktops: &[String], found: impl Fn(&str) -> bool) -> Option<String> {
    if f.get_bool("X-systemd-skip") == Some(true) {
        return Some("Skipped by systemd (X-systemd-skip): started by the desktop itself".into());
    }
    if f.get("Type") != Some("Application") {
        return Some("Not an application entry".into());
    }
    let exec = f.get_string("Exec").unwrap_or_default();
    let Some(program) = desktop::exec_args(&exec).into_iter().next() else {
        return Some("No command (Exec) set".into());
    };
    if let Some(try_exec) = f.get_string("TryExec").filter(|t| !t.is_empty())
        && !found(&try_exec)
    {
        return Some(format!("Program not installed: {try_exec}"));
    }
    if !found(&program) {
        return Some(format!("Program not installed: {program}"));
    }
    let mut only = f.get_list("OnlyShowIn");
    let mut not = f.get_list("NotShowIn");
    // GNOME handles phased entries itself, so systemd excludes them there.
    if f.get("X-GNOME-Autostart-Phase")
        .is_some_and(|p| !p.is_empty())
    {
        only.retain(|d| d != "GNOME");
        not.push("GNOME".into());
    }
    if !only.is_empty() && !only.iter().any(|d| desktops.contains(d)) {
        return Some(format!("Only for {}", only.join(", ")));
    }
    if let Some(d) = not.iter().find(|d| desktops.contains(d)) {
        return Some(format!("Not shown in {d} (NotShowIn)"));
    }
    None
}

/// `systemd` `find_executable`: absolute/relative paths must be executable
/// files, bare names are searched in `$PATH`.
pub fn find_executable(name: &str) -> Option<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let is_exec = |p: &Path| {
        p.metadata()
            .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
    };
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_exec(&p).then_some(p);
    }
    let path = std::env::var("PATH").unwrap_or_else(|_| "/usr/local/bin:/usr/bin".into());
    path.split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| is_exec(p))
}

/// systemd `unit_name_escape`.
pub fn unit_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for (i, b) in s.bytes().enumerate() {
        match b {
            b'/' => out.push('-'),
            b'.' if i == 0 => out.push_str("\\x2e"),
            b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b':' | b'_' | b'.' => out.push(b as char),
            _ => out.push_str(&format!("\\x{b:02x}")),
        }
    }
    out
}

/// Unit the generator creates for `<id>` (`foo-bar.desktop`).
pub fn unit_for(id: &str) -> String {
    format!(
        "app-{}@autostart.service",
        unit_escape(id.strip_suffix(".desktop").unwrap_or(id))
    )
}

#[derive(Debug, Clone)]
pub struct Autostart {
    /// File basename, e.g. `steam.desktop`.
    pub id: String,
    /// The file that takes effect (user copy wins).
    pub path: PathBuf,
    pub user_path: Option<PathBuf>,
    pub system_path: Option<PathBuf>,
    pub file: DesktopFile,
    pub name: String,
    pub comment: Option<String>,
    /// `Exec` without field codes.
    pub command: String,
    pub icon: Option<String>,
    pub eval: Eval,
    pub unit: String,
}

impl Autostart {
    /// Only user files without a system counterpart can be deleted.
    pub fn deletable(&self) -> bool {
        self.user_path.is_some() && self.system_path.is_none()
    }
}

fn desktop_files(dir: &Path) -> Vec<(String, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    rd.flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            name.ends_with(".desktop").then(|| (name, e.path()))
        })
        .collect()
}

pub fn scan() -> Vec<Autostart> {
    let desktops = current_desktops();
    let mut map: BTreeMap<String, (Option<PathBuf>, Option<PathBuf>)> = BTreeMap::new();
    for (id, path) in desktop_files(&user_dir()) {
        map.entry(id).or_default().0 = Some(path);
    }
    for dir in system_dirs() {
        for (id, path) in desktop_files(&dir) {
            let slot = &mut map.entry(id).or_default().1;
            if slot.is_none() {
                *slot = Some(path);
            }
        }
    }
    let mut out: Vec<Autostart> = map
        .into_iter()
        .filter_map(|(id, (user_path, system_path))| {
            let path = user_path.clone().or_else(|| system_path.clone())?;
            let text = std::fs::read_to_string(&path).ok()?;
            let file = DesktopFile::parse(&text);
            let eval = evaluate(&file, &desktops, |p| find_executable(p).is_some());
            let exec = file.get_string("Exec").unwrap_or_default();
            Some(Autostart {
                name: file
                    .get_string("Name")
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| id.trim_end_matches(".desktop").to_owned()),
                comment: file.get_string("Comment").filter(|c| !c.is_empty()),
                command: desktop::strip_field_codes(&exec),
                icon: file.get_string("Icon").filter(|i| !i.is_empty()),
                unit: unit_for(&id),
                id,
                path,
                user_path,
                system_path,
                file,
                eval,
            })
        })
        .collect();
    out.sort_by_key(|a| a.name.to_lowercase());
    out
}

fn find(id: &str) -> Result<Autostart> {
    let id = if id.ends_with(".desktop") {
        id.to_owned()
    } else {
        format!("{id}.desktop")
    };
    scan()
        .into_iter()
        .find(|a| a.id == id)
        .with_context(|| format!("no autostart entry named {id}"))
}

/// Apply the enabled flag to a desktop file's text.
pub fn set_enabled_text(file: &mut DesktopFile, enabled: bool) {
    if enabled {
        file.remove("Hidden");
        if file.get_bool("X-GNOME-Autostart-enabled") == Some(false) {
            file.set("X-GNOME-Autostart-enabled", "true");
        }
    } else {
        file.set("Hidden", "true");
    }
}

/// Enable or disable `id` for the next login (edits/creates the user copy;
/// a user copy that ends up identical to the system file is removed).
pub fn set_enabled(id: &str, enabled: bool) -> Result<()> {
    let entry = find(id)?;
    let user = user_dir().join(&entry.id);
    let mut file = entry.file.clone();
    set_enabled_text(&mut file, enabled);
    let text = file.render();
    if let Some(sys) = &entry.system_path
        && std::fs::read_to_string(sys).is_ok_and(|s| s == text)
    {
        if entry.user_path.is_some() {
            std::fs::remove_file(&user).with_context(|| format!("removing {}", user.display()))?;
        }
        return Ok(());
    }
    store::write_atomic(&user, text.as_bytes())
}

/// `my app!` → `my-app`.
pub fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "startup-app".into()
    } else {
        out
    }
}

fn ensure_new(id: &str) -> Result<PathBuf> {
    let path = user_dir().join(id);
    if path.exists() || system_dirs().iter().any(|d| d.join(id).exists()) {
        bail!("{id} is already a startup entry");
    }
    Ok(path)
}

/// Copy an installed application's desktop file into the user autostart dir.
pub fn add_app(source: &Path) -> Result<PathBuf> {
    let id = source
        .file_name()
        .and_then(|n| n.to_str())
        .context("invalid desktop file name")?;
    let path = ensure_new(id)?;
    let text =
        std::fs::read_to_string(source).with_context(|| format!("reading {}", source.display()))?;
    let mut file = DesktopFile::parse(&text);
    set_enabled_text(&mut file, true);
    store::write_atomic(&path, file.render().as_bytes())?;
    Ok(path)
}

/// Render a new autostart entry for a custom command.
pub fn custom_entry(name: &str, command: &str) -> String {
    // Exec is a string value: escape backslashes first, then literal `%`.
    let exec = desktop::escape(command.trim()).replace('%', "%%");
    format!(
        "[Desktop Entry]\nType=Application\nName={}\nComment=Added by hyprdeck\nExec={exec}\nTerminal=false\nX-GNOME-Autostart-enabled=true\n",
        desktop::escape(name.trim())
    )
}

pub fn add_custom(name: &str, command: &str) -> Result<PathBuf> {
    if name.trim().is_empty() || command.trim().is_empty() {
        bail!("name and command are required");
    }
    let base = slug(name);
    let mut id = format!("{base}.desktop");
    let mut n = 2;
    while user_dir().join(&id).exists() || system_dirs().iter().any(|d| d.join(&id).exists()) {
        id = format!("{base}-{n}.desktop");
        n += 1;
    }
    let path = user_dir().join(&id);
    store::write_atomic(&path, custom_entry(name, command).as_bytes())?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(text: &str) -> Eval {
        evaluate(&DesktopFile::parse(text), &["Hyprland".to_owned()], |p| {
            p != "missing"
        })
    }

    const BASE: &str = "[Desktop Entry]\nType=Application\nName=X\nExec=x %U\n";

    #[test]
    fn plain_entry_runs() {
        assert_eq!(
            eval(BASE),
            Eval {
                enabled: true,
                blocker: None
            }
        );
    }

    #[test]
    fn hidden_and_gnome_disabled() {
        assert!(!eval(&format!("{BASE}Hidden=true\n")).enabled);
        assert!(eval(&format!("{BASE}Hidden=false\n")).enabled);
        assert!(!eval(&format!("{BASE}X-GNOME-Autostart-enabled=false\n")).enabled);
    }

    #[test]
    fn desktop_filters() {
        assert_eq!(
            eval(&format!("{BASE}OnlyShowIn=XFCE;\n")).blocker.unwrap(),
            "Only for XFCE"
        );
        assert!(
            eval(&format!("{BASE}OnlyShowIn=XFCE;Hyprland;\n"))
                .blocker
                .is_none()
        );
        assert!(
            eval(&format!("{BASE}NotShowIn=KDE;GNOME;\n"))
                .blocker
                .is_none()
        );
        assert!(
            eval(&format!("{BASE}NotShowIn=Hyprland;\n"))
                .blocker
                .is_some()
        );
        // Phase entries: GNOME moves from OnlyShowIn to NotShowIn.
        let e = eval(&format!(
            "{BASE}OnlyShowIn=GNOME;Unity;\nX-GNOME-Autostart-Phase=Initialization\n"
        ));
        assert_eq!(e.blocker.unwrap(), "Only for Unity");
        let gnome = evaluate(
            &DesktopFile::parse(&format!("{BASE}X-GNOME-Autostart-Phase=Panel\n")),
            &["GNOME".to_owned()],
            |_| true,
        );
        assert!(gnome.blocker.is_some());
    }

    #[test]
    fn generator_skips() {
        assert!(
            eval(&format!("{BASE}X-systemd-skip=true\n"))
                .blocker
                .is_some()
        );
        assert!(
            eval("[Desktop Entry]\nType=Link\nExec=x\n")
                .blocker
                .is_some()
        );
        assert!(
            eval("[Desktop Entry]\nType=Application\n")
                .blocker
                .is_some()
        );
        assert!(
            eval(&format!("{BASE}TryExec=missing\n"))
                .blocker
                .unwrap()
                .contains("missing")
        );
        assert!(
            eval("[Desktop Entry]\nType=Application\nExec=missing --x\n")
                .blocker
                .is_some()
        );
    }

    #[test]
    fn enable_disable_round_trip() {
        let original =
            "[Desktop Entry]\nType=Application\nName=E\nExec=e %u\n\n[Desktop Action A]\nName=A\n";
        let mut f = DesktopFile::parse(original);
        set_enabled_text(&mut f, false);
        assert!(!evaluate(&f, &[], |_| true).enabled);
        set_enabled_text(&mut f, true);
        assert_eq!(f.render(), original);

        let mut g =
            DesktopFile::parse("[Desktop Entry]\nX-GNOME-Autostart-enabled=false\nHidden=true\n");
        set_enabled_text(&mut g, true);
        assert_eq!(
            g.render(),
            "[Desktop Entry]\nX-GNOME-Autostart-enabled=true\n"
        );
    }

    #[test]
    fn unit_names_match_generator() {
        assert_eq!(
            unit_for("example-client.desktop"),
            "app-example\\x2dclient@autostart.service"
        );
        assert_eq!(
            unit_for("org.example.App.desktop"),
            "app-org.example.App@autostart.service"
        );
        assert_eq!(unit_escape(".x y"), "\\x2ex\\x20y");
    }

    #[test]
    fn custom_entries() {
        assert_eq!(slug("My App! 2"), "my-app-2");
        assert_eq!(slug("!!"), "startup-app");
        let text = custom_entry("Foo", "sh -c 'echo 50% \\o'");
        let f = DesktopFile::parse(&text);
        assert_eq!(f.get_string("Exec").unwrap(), "sh -c 'echo 50%% \\o'");
        assert_eq!(
            desktop::strip_field_codes(&f.get_string("Exec").unwrap()),
            "sh -c 'echo 50% \\o'"
        );
    }
}
