//! Human-readable labels and categories for keybind actions, plus a tiny
//! parser for the serialized Lua argument lists of `hl.dsp.*` dispatchers.

use std::borrow::Cow;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Category {
    Apps,
    Windows,
    Workspaces,
    Media,
    Noctalia,
    Other,
}

impl Category {
    pub const ALL: [Category; 6] = [
        Category::Apps,
        Category::Windows,
        Category::Workspaces,
        Category::Media,
        Category::Noctalia,
        Category::Other,
    ];

    pub fn title(self) -> &'static str {
        match self {
            Category::Apps => "Applications",
            Category::Windows => "Windows",
            Category::Workspaces => "Workspaces & Monitors",
            Category::Media => "Media & Hardware",
            Category::Noctalia => "Noctalia",
            Category::Other => "Other",
        }
    }
}

/// A literal from a serialized Lua argument list.
#[derive(Debug, Clone, PartialEq)]
pub enum Lit {
    Str(String),
    Num(String),
    Bool(bool),
    /// Anything else (identifiers, nested tables, expressions), as source text.
    Raw(String),
}

impl Lit {
    pub fn as_str(&self) -> Option<&str> {
        match self {
            Lit::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_int(&self) -> Option<i64> {
        match self {
            Lit::Num(n) => n.parse().ok(),
            _ => None,
        }
    }

    /// Display text (strings unquoted).
    pub fn text(&self) -> Cow<'_, str> {
        match self {
            Lit::Str(s) | Lit::Num(s) | Lit::Raw(s) => Cow::Borrowed(s),
            Lit::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
        }
    }
}

/// Positional values and `key = value` fields of an argument list. Fields of
/// a table argument are flattened into `fields`; its array items into `positional`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Args {
    pub positional: Vec<Lit>,
    pub fields: Vec<(String, Lit)>,
}

impl Args {
    pub fn get(&self, key: &str) -> Option<&Lit> {
        self.fields.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    pub fn first_str(&self) -> Option<&str> {
        self.positional.first().and_then(Lit::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.positional.is_empty() && self.fields.is_empty()
    }

    pub fn flag(&self, key: &str) -> bool {
        self.get(key) == Some(&Lit::Bool(true))
    }
}

struct Cursor<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Cursor<'a> {
    fn peek(&self) -> Option<char> {
        self.s[self.i..].chars().next()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += c.len_utf8();
        Some(c)
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.bump();
        }
    }

    fn string(&mut self, quote: char) -> String {
        let mut out = String::new();
        while let Some(c) = self.bump() {
            match c {
                c if c == quote => break,
                '\\' => match self.bump() {
                    Some('n') => out.push('\n'),
                    Some('t') => out.push('\t'),
                    Some('r') => out.push('\r'),
                    Some(d) if d.is_ascii_digit() => {
                        let mut code = d.to_digit(10).unwrap_or(0);
                        for _ in 0..2 {
                            match self.peek().and_then(|c| c.to_digit(10)) {
                                Some(n) => {
                                    code = code * 10 + n;
                                    self.bump();
                                }
                                None => break,
                            }
                        }
                        out.extend(char::from_u32(code));
                    }
                    Some(other) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        out
    }

    /// Raw text up to the next `,`/`}`/`)` at nesting depth 0.
    fn raw(&mut self) -> &'a str {
        let start = self.i;
        let mut depth = 0i32;
        while let Some(c) = self.peek() {
            match c {
                '{' | '(' | '[' => depth += 1,
                '}' | ')' | ']' if depth == 0 => break,
                '}' | ')' | ']' => depth -= 1,
                ',' if depth == 0 => break,
                '"' | '\'' => {
                    self.bump();
                    self.string(c);
                    continue;
                }
                _ => {}
            }
            self.bump();
        }
        self.s[start..self.i].trim()
    }

    fn literal(&mut self) -> Lit {
        self.ws();
        match self.peek() {
            Some(q @ ('"' | '\'')) => {
                self.bump();
                Lit::Str(self.string(q))
            }
            _ => {
                let raw = self.raw();
                match raw {
                    "true" => Lit::Bool(true),
                    "false" => Lit::Bool(false),
                    r if !r.is_empty() && r.parse::<f64>().is_ok() => Lit::Num(r.to_owned()),
                    r => Lit::Raw(r.to_owned()),
                }
            }
        }
    }

    /// `ident =` ahead? Consumes it when present.
    fn field_key(&mut self) -> Option<String> {
        let save = self.i;
        self.ws();
        let start = self.i;
        while self
            .peek()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
        {
            self.bump();
        }
        let key = &self.s[start..self.i];
        self.ws();
        if !key.is_empty()
            && !key.starts_with(|c: char| c.is_ascii_digit())
            && self.peek() == Some('=')
        {
            self.bump();
            if self.peek() != Some('=') {
                return Some(key.to_owned());
            }
        }
        self.i = save;
        None
    }
}

/// Parse a Lua argument list such as `{ workspace = "m+1" }` or `"togglesplit"`.
pub fn parse_args(text: &str) -> Args {
    let mut args = Args::default();
    let mut c = Cursor { s: text, i: 0 };
    loop {
        c.ws();
        match c.peek() {
            None => break,
            Some(',') => {
                c.bump();
            }
            Some('{') => {
                c.bump();
                loop {
                    c.ws();
                    match c.peek() {
                        None => break,
                        Some('}') => {
                            c.bump();
                            break;
                        }
                        Some(',') => {
                            c.bump();
                        }
                        _ => {
                            let before = c.i;
                            match c.field_key() {
                                Some(k) => {
                                    let v = c.literal();
                                    args.fields.push((k, v));
                                }
                                None => args.positional.push(c.literal()),
                            }
                            if c.i == before {
                                c.bump();
                            }
                        }
                    }
                }
            }
            Some(_) => {
                let before = c.i;
                args.positional.push(c.literal());
                if c.i == before {
                    c.bump();
                }
            }
        }
    }
    args
}

/// Argument text of a serialized dispatcher call: `hl.dsp.focus({ … })` → `{ … }`.
pub fn dsp_args<'a>(dispatcher: &str, action_lua: &'a str) -> &'a str {
    action_lua
        .strip_prefix("hl.dsp.")
        .and_then(|s| s.strip_prefix(dispatcher))
        .and_then(|s| s.strip_prefix('('))
        .and_then(|s| s.strip_suffix(')'))
        .unwrap_or("")
}

/// Resolves a program or desktop id to an application display name.
pub type AppNames<'a> = &'a dyn Fn(&str) -> Option<String>;

/// Label and category of a bind action. `args` is the Lua argument text.
pub fn describe(dispatcher: Option<&str>, args: &str, app_names: AppNames) -> (String, Category) {
    let Some(d) = dispatcher else {
        return ("Custom Lua function".into(), Category::Other);
    };
    let a = parse_args(args);
    let dir = || a.get("direction").map(|l| direction(&l.text()).to_owned());
    match d {
        "exec_cmd" | "exec_raw" => match a.first_str() {
            Some(cmd) => describe_exec(cmd, app_names),
            None => (format!("Run {args}"), Category::Other),
        },
        "window.close" => ("Close window".into(), Category::Windows),
        "window.kill" => ("Kill window".into(), Category::Windows),
        "window.float" => {
            let label = match a.get("action").map(|l| l.text()).as_deref() {
                Some("enable" | "float" | "set") => "Float window",
                Some("disable" | "tile" | "unset") => "Tile window",
                _ => "Toggle floating",
            };
            (label.into(), Category::Windows)
        }
        "window.fullscreen" => {
            let label = match a.get("mode").and_then(Lit::as_int) {
                Some(1) => "Maximize window",
                _ => "Toggle fullscreen",
            };
            (label.into(), Category::Windows)
        }
        "window.pin" => ("Pin window to all workspaces".into(), Category::Windows),
        "window.center" => ("Center window".into(), Category::Windows),
        "window.pseudo" => ("Toggle pseudo-tiling".into(), Category::Windows),
        "window.cycle_next" => {
            let label = if a.flag("prev") || a.flag("previous") {
                "Cycle to previous window"
            } else {
                "Cycle to next window"
            };
            (label.into(), Category::Windows)
        }
        "window.drag" => ("Move window with mouse".into(), Category::Windows),
        "window.resize" if a.is_empty() => ("Resize window with mouse".into(), Category::Windows),
        "window.bring_to_top" => ("Bring window to top".into(), Category::Windows),
        "window.toggle_swallow" => ("Toggle window swallowing".into(), Category::Windows),
        "window.swap" if dir().is_some() => (
            format!("Swap window {}", dir().unwrap_or_default()),
            Category::Windows,
        ),
        "window.move" => {
            if let Some(ws) = a.get("workspace") {
                (
                    format!("Move window to {}", workspace(ws)),
                    Category::Workspaces,
                )
            } else if let Some(m) = a.get("monitor") {
                (
                    format!("Move window to {}", monitor(&m.text())),
                    Category::Workspaces,
                )
            } else if let Some(d) = dir() {
                (format!("Move window {d}"), Category::Windows)
            } else {
                (format!("Move window {args}"), Category::Windows)
            }
        }
        "focus" => {
            if let Some(ws) = a.get("workspace") {
                (format!("Go to {}", workspace(ws)), Category::Workspaces)
            } else if let Some(m) = a.get("monitor") {
                (
                    format!("Focus {}", monitor(&m.text())),
                    Category::Workspaces,
                )
            } else if let Some(d) = dir() {
                (format!("Focus window {d}"), Category::Windows)
            } else {
                (format!("Focus {args}"), Category::Windows)
            }
        }
        "workspace.toggle_special" => match a
            .first_str()
            .or_else(|| a.get("name").and_then(Lit::as_str))
        {
            Some(name) if !name.is_empty() => (
                format!("Toggle special workspace “{name}”"),
                Category::Workspaces,
            ),
            _ => ("Toggle special workspace".into(), Category::Workspaces),
        },
        "workspace.swap_monitors" => (
            "Swap workspaces between monitors".into(),
            Category::Workspaces,
        ),
        "workspace.move" => match a.get("monitor") {
            Some(m) => (
                format!("Move workspace to {}", monitor(&m.text())),
                Category::Workspaces,
            ),
            None => (format!("Move workspace {args}"), Category::Workspaces),
        },
        "layout" => match a.first_str() {
            Some(msg) => (
                format!("Layout: {}", msg.replace(['_', '-'], " ")),
                Category::Windows,
            ),
            None => ("Layout message".into(), Category::Windows),
        },
        "dpms" => {
            let action = a
                .get("action")
                .map(|l| l.text().into_owned())
                .unwrap_or_else(|| "toggle".into());
            (format!("Monitor power: {action}"), Category::Media)
        }
        "exit" => ("Exit Hyprland".into(), Category::Other),
        "submap" => match a.first_str() {
            Some("reset") => ("Leave submap".into(), Category::Other),
            Some(s) => (format!("Enter submap “{s}”"), Category::Other),
            None => ("Submap".into(), Category::Other),
        },
        "global" => (
            format!("Global shortcut {}", a.first_str().unwrap_or(args)),
            Category::Other,
        ),
        _ => {
            let label = if args.is_empty() {
                d.to_owned()
            } else {
                format!("{d} {args}")
            };
            let cat = if d.starts_with("window.") || d.starts_with("group.") {
                Category::Windows
            } else if d.starts_with("workspace.") {
                Category::Workspaces
            } else {
                Category::Other
            };
            (label, cat)
        }
    }
}

fn direction(d: &str) -> &str {
    match d {
        "l" | "left" => "left",
        "r" | "right" => "right",
        "u" | "t" | "up" | "top" => "up",
        "d" | "b" | "down" | "bottom" => "down",
        other => other,
    }
}

fn monitor(m: &str) -> String {
    match m {
        "" => "monitor (not set)".into(),
        "+1" => "next monitor".into(),
        "-1" => "previous monitor".into(),
        "l" | "r" | "u" | "d" | "left" | "right" | "up" | "down" => {
            format!("monitor {}", direction(m))
        }
        other => format!("monitor {other}"),
    }
}

fn workspace(ws: &Lit) -> String {
    if let Some(n) = ws.as_int() {
        return format!("workspace {n}");
    }
    let s = ws.text();
    let s = s.as_ref();
    match s {
        "previous" => "previously used workspace".into(),
        "empty" => "first empty workspace".into(),
        "emptym" => "first empty workspace on monitor".into(),
        "emptyn" => "next empty workspace".into(),
        "special" => "special workspace".into(),
        "+1" | "r+1" => "next workspace".into(),
        "-1" | "r-1" => "previous workspace".into(),
        "m+1" => "next workspace on monitor".into(),
        "m-1" => "previous workspace on monitor".into(),
        "e+1" => "next open workspace".into(),
        "e-1" => "previous open workspace".into(),
        _ => {
            if let Some(name) = s.strip_prefix("special:") {
                format!("special workspace “{name}”")
            } else if let Some(n) = s.strip_prefix("m~") {
                format!("workspace {n} on monitor")
            } else if let Some(name) = s.strip_prefix("name:") {
                format!("workspace “{name}”")
            } else {
                format!("workspace {s}")
            }
        }
    }
}

/// Wrappers that start an app (or desktop id) in a launcher-managed way.
pub const LAUNCH_PREFIXES: [&str; 5] = [
    "uwsm app -- ",
    "uwsm-app -- ",
    "app2unit -- ",
    "uwsm app ",
    "gtk-launch ",
];
const MEDIA_TOOLS: [&str; 7] = [
    "playerctl",
    "wpctl",
    "pactl",
    "pamixer",
    "brightnessctl",
    "swayosd-client",
    "ddcutil",
];

/// Label and category of an `exec_cmd` command line.
pub fn describe_exec(cmd: &str, app_names: AppNames) -> (String, Category) {
    describe_command(cmd, app_names, crate::noctalia::installed())
}

/// [`describe_exec`] with Noctalia IPC recognized only when `noctalia` is set.
fn describe_command(cmd: &str, app_names: AppNames, noctalia: bool) -> (String, Category) {
    let cmd = cmd.trim();
    if let Some(rest) = LAUNCH_PREFIXES.iter().find_map(|p| cmd.strip_prefix(p)) {
        return (launch(rest.trim(), app_names), Category::Apps);
    }
    if noctalia && let Some(rest) = cmd.strip_prefix(crate::noctalia::PREFIX) {
        return describe_noctalia(rest.trim());
    }
    let mut words = cmd.split_whitespace();
    let prog = words.next().unwrap_or("");
    let prog_name = prog.rsplit('/').next().unwrap_or(prog);
    match (prog_name, words.next()) {
        ("hyprctl", Some("kill")) => ("Kill a window (click to pick)".into(), Category::Windows),
        ("hyprctl", Some("dispatch")) => {
            let rest = cmd.split_once("dispatch").map_or("", |(_, r)| r.trim());
            (format!("Hyprland: {rest}"), Category::Windows)
        }
        (p, _) if MEDIA_TOOLS.contains(&p) => (format!("Run {cmd}"), Category::Media),
        (_, None) if !prog.contains('/') => (launch(cmd, app_names), Category::Apps),
        _ => (format!("Run {cmd}"), Category::Other),
    }
}

fn launch(target: &str, app_names: AppNames) -> String {
    let (prog, rest) = target
        .split_once(char::is_whitespace)
        .unwrap_or((target, ""));
    let name =
        app_names(prog).unwrap_or_else(|| prog.strip_suffix(".desktop").unwrap_or(prog).to_owned());
    let rest = rest.trim();
    if rest.is_empty() {
        format!("Launch {name}")
    } else {
        format!("Launch {name} ({rest})")
    }
}

fn is_media_command(cmd: &str) -> bool {
    ["volume-", "mic-", "brightness-", "keyboard-backlight-"]
        .iter()
        .any(|p| cmd.starts_with(p))
        || cmd == "media"
}

/// Label and category of `noctalia msg <rest>`.
pub fn describe_noctalia(rest: &str) -> (String, Category) {
    let (cmd, arg) = rest
        .split_once(char::is_whitespace)
        .map_or((rest, ""), |(c, a)| (c, a.trim()));
    let label: String = match (cmd, arg) {
        ("panel-toggle", a) => format!("toggle {}", panel(a)),
        ("panel-open", a) => format!("open {}", panel(a)),
        ("panel-close", "") => "close panel".into(),
        ("media", "toggle" | "play-pause") => "Play/pause".into(),
        ("media", "next") => "Next track".into(),
        ("media", "previous" | "prev") => "Previous track".into(),
        ("volume-up", _) => "Volume up".into(),
        ("volume-down", _) => "Volume down".into(),
        ("volume-mute", _) => "Mute speakers".into(),
        ("mic-mute", _) => "Mute microphone".into(),
        ("brightness-up", _) => "Brightness up".into(),
        ("brightness-down", _) => "Brightness down".into(),
        ("session", "lock") => "lock screen".into(),
        ("screenshot-region", _) => "region screenshot".into(),
        ("screenshot-fullscreen", "") => "full-screen screenshot".into(),
        ("settings-toggle", "") => "toggle settings".into(),
        ("window-switcher", "") => "window switcher".into(),
        (c, "") => c.replace('-', " "),
        (c, a) => format!("{} ({a})", c.replace('-', " ")),
    };
    if is_media_command(cmd) {
        let mut chars = label.chars();
        let label = match chars.next() {
            Some(f) => f.to_uppercase().chain(chars).collect(),
            None => label,
        };
        (label, Category::Media)
    } else {
        (format!("Noctalia: {label}"), Category::Noctalia)
    }
}

fn panel(arg: &str) -> String {
    let (id, ctx) = arg
        .split_once(char::is_whitespace)
        .map_or((arg, ""), |(i, c)| (i, c.trim()));
    let id = id.replace('-', " ");
    if ctx.is_empty() {
        id
    } else {
        format!("{id} ({ctx})")
    }
}

/// Keycap label for one part of a combo (`SUPER` → `Super`, `mouse:272` → `Left click`).
pub fn key_label(part: &str) -> Cow<'_, str> {
    let fixed = match part.to_ascii_uppercase().as_str() {
        "SUPER" => "Super",
        "CTRL" | "CONTROL" => "Ctrl",
        "ALT" => "Alt",
        "SHIFT" => "Shift",
        "RETURN" => "Enter",
        "SPACE" => "Space",
        "ESCAPE" => "Esc",
        "PRINT" => "PrtSc",
        "TAB" => "Tab",
        "PERIOD" => ".",
        "COMMA" => ",",
        "SLASH" => "/",
        "MINUS" => "-",
        "EQUAL" => "=",
        "SEMICOLON" => ";",
        "GRAVE" => "`",
        "BACKSPACE" => "Backspace",
        "DELETE" => "Del",
        "LEFT" => "←",
        "RIGHT" => "→",
        "UP" => "↑",
        "DOWN" => "↓",
        "MOUSE:272" => "Left click",
        "MOUSE:273" => "Right click",
        "MOUSE:274" => "Middle click",
        "MOUSE_UP" => "Scroll up",
        "MOUSE_DOWN" => "Scroll down",
        "MOUSE_LEFT" => "Scroll left",
        "MOUSE_RIGHT" => "Scroll right",
        "XF86AUDIORAISEVOLUME" => "Volume Up",
        "XF86AUDIOLOWERVOLUME" => "Volume Down",
        "XF86AUDIOMUTE" => "Mute",
        "XF86AUDIOMICMUTE" => "Mic Mute",
        "XF86AUDIOPLAY" => "Play",
        "XF86AUDIOPAUSE" => "Pause",
        "XF86AUDIONEXT" => "Next",
        "XF86AUDIOPREV" => "Previous",
        "XF86MONBRIGHTNESSUP" => "Brightness Up",
        "XF86MONBRIGHTNESSDOWN" => "Brightness Down",
        _ => {
            return part
                .strip_prefix("XF86")
                .map_or(Cow::Borrowed(part), Cow::Borrowed);
        }
    };
    Cow::Borrowed(fixed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn no_apps(_: &str) -> Option<String> {
        None
    }

    fn d(dsp: &str, args: &str) -> (String, Category) {
        describe(Some(dsp), args, &no_apps)
    }

    #[test]
    fn parses_tables_strings_and_escapes() {
        let a = parse_args(r#"{ mode = 1, workspace = "m~2", silent = true, x = MONITOR1 }"#);
        assert_eq!(a.get("mode"), Some(&Lit::Num("1".into())));
        assert_eq!(a.get("workspace").and_then(Lit::as_str), Some("m~2"));
        assert!(a.flag("silent"));
        assert_eq!(a.get("x"), Some(&Lit::Raw("MONITOR1".into())));
        let a = parse_args(r#""say \"hi\"\n", 3"#);
        assert_eq!(a.first_str(), Some("say \"hi\"\n"));
        assert_eq!(a.positional[1].as_int(), Some(3));
        let a = parse_args(r#"{ device = { list = { "a", "b" } }, locked = true }"#);
        assert_eq!(
            a.get("device"),
            Some(&Lit::Raw(r#"{ list = { "a", "b" } }"#.into()))
        );
        assert!(a.flag("locked"));
        assert!(parse_args("").is_empty());
        assert_eq!(parse_args(r#""\65\066""#).first_str(), Some("AB"));
    }

    #[test]
    fn strips_dispatcher_call() {
        assert_eq!(
            dsp_args("window.move", r#"hl.dsp.window.move({ direction = "l" })"#),
            r#"{ direction = "l" }"#
        );
        assert_eq!(dsp_args("window.close", "hl.dsp.window.close()"), "");
        assert_eq!(dsp_args("focus", "<lua function>"), "");
    }

    #[test]
    fn humanizes_noctalia_commands() {
        let n = |c: &str| describe_command(&format!("noctalia msg {c}"), &no_apps, true);
        assert_eq!(
            n("panel-toggle launcher"),
            ("Noctalia: toggle launcher".into(), Category::Noctalia)
        );
        assert_eq!(
            n("panel-toggle launcher /emo").0,
            "Noctalia: toggle launcher (/emo)"
        );
        assert_eq!(
            n("panel-toggle control-center notifications").0,
            "Noctalia: toggle control center (notifications)"
        );
        assert_eq!(n("session lock").0, "Noctalia: lock screen");
        assert_eq!(n("volume-up"), ("Volume up".into(), Category::Media));
        assert_eq!(n("media toggle"), ("Play/pause".into(), Category::Media));
        assert_eq!(n("mic-mute"), ("Mute microphone".into(), Category::Media));
        assert_eq!(n("screenshot-region").0, "Noctalia: region screenshot");
        assert_eq!(
            n("wallpaper-random DP-1").0,
            "Noctalia: wallpaper random (DP-1)"
        );
        assert_eq!(
            describe_command("noctalia msg session lock", &no_apps, false),
            ("Run noctalia msg session lock".into(), Category::Other)
        );
    }

    #[test]
    fn humanizes_exec_commands() {
        let apps = |p: &str| {
            (p == "firefox" || p == "org.gnome.Calculator.desktop").then(|| "Firefox".to_owned())
        };
        assert_eq!(
            describe_exec("uwsm app -- firefox", &apps),
            ("Launch Firefox".into(), Category::Apps)
        );
        assert_eq!(
            describe_exec("app2unit -- firefox", &apps),
            ("Launch Firefox".into(), Category::Apps)
        );
        assert_eq!(
            describe_exec("uwsm app -- kitty -e btop", &no_apps).0,
            "Launch kitty (-e btop)"
        );
        assert_eq!(
            describe_exec("uwsm app -- firefox.desktop", &no_apps).0,
            "Launch firefox"
        );
        assert_eq!(describe_exec("hyprctl kill", &no_apps).1, Category::Windows);
        assert_eq!(
            describe_exec("playerctl next", &no_apps),
            ("Run playerctl next".into(), Category::Media)
        );
        assert_eq!(
            describe_exec("hyprpicker -a", &no_apps),
            ("Run hyprpicker -a".into(), Category::Other)
        );
        assert_eq!(
            describe_exec("/opt/tools/shot", &no_apps).1,
            Category::Other
        );
        assert_eq!(
            describe(Some("exec_cmd"), r#""kitty""#, &no_apps),
            ("Launch kitty".into(), Category::Apps)
        );
    }

    #[test]
    fn humanizes_dispatchers() {
        assert_eq!(
            d("window.close", ""),
            ("Close window".into(), Category::Windows)
        );
        assert_eq!(d("window.fullscreen", "{ mode = 1 }").0, "Maximize window");
        assert_eq!(d("window.fullscreen", "").0, "Toggle fullscreen");
        assert_eq!(
            d("window.float", r#"{ action = "toggle" }"#).0,
            "Toggle floating"
        );
        assert_eq!(
            d("window.move", r#"{ direction = "l" }"#),
            ("Move window left".into(), Category::Windows)
        );
        assert_eq!(
            d("window.move", r#"{ workspace = "m~3" }"#),
            (
                "Move window to workspace 3 on monitor".into(),
                Category::Workspaces
            )
        );
        assert_eq!(
            d("window.move", r#"{ workspace = "special" }"#).0,
            "Move window to special workspace"
        );
        assert_eq!(
            d("window.move", r#"{ monitor = "+1" }"#).0,
            "Move window to next monitor"
        );
        assert_eq!(
            d("window.move", r#"{ monitor = "" }"#).0,
            "Move window to monitor (not set)"
        );
        assert_eq!(
            d("focus", "{ workspace = 4 }"),
            ("Go to workspace 4".into(), Category::Workspaces)
        );
        assert_eq!(
            d("focus", r#"{ workspace = "m-1" }"#).0,
            "Go to previous workspace on monitor"
        );
        assert_eq!(
            d("focus", r#"{ workspace = "emptym" }"#).0,
            "Go to first empty workspace on monitor"
        );
        assert_eq!(
            d("focus", r#"{ direction = "up" }"#),
            ("Focus window up".into(), Category::Windows)
        );
        assert_eq!(
            d("workspace.toggle_special", "").0,
            "Toggle special workspace"
        );
        assert_eq!(d("layout", r#""togglesplit""#).0, "Layout: togglesplit");
        assert_eq!(d("window.drag", "").0, "Move window with mouse");
        assert_eq!(d("group.toggle", "").1, Category::Windows);
        assert_eq!(describe(None, "", &no_apps).1, Category::Other);
    }

    #[test]
    fn keycap_labels() {
        assert_eq!(key_label("SUPER"), "Super");
        assert_eq!(key_label("mouse:272"), "Left click");
        assert_eq!(key_label("XF86AudioRaiseVolume"), "Volume Up");
        assert_eq!(key_label("XF86Calculator"), "Calculator");
        assert_eq!(key_label("Q"), "Q");
        assert_eq!(key_label("period"), ".");
    }
}
