//! Line-preserving model of a freedesktop `.desktop` file. Only the
//! `[Desktop Entry]` group is read or edited; every other line (comments, other
//! groups, localized keys, ordering) is kept byte-for-byte.

const MAIN_GROUP: &str = "[Desktop Entry]";

#[derive(Debug, Clone, PartialEq)]
pub struct DesktopFile {
    lines: Vec<String>,
    trailing_newline: bool,
}

impl DesktopFile {
    pub fn parse(text: &str) -> Self {
        Self {
            lines: text.lines().map(str::to_owned).collect(),
            trailing_newline: text.ends_with('\n') || text.is_empty(),
        }
    }

    pub fn render(&self) -> String {
        let mut out = self.lines.join("\n");
        if self.trailing_newline && !self.lines.is_empty() {
            out.push('\n');
        }
        out
    }

    /// `(first line after the header, end exclusive)` of the main group.
    fn group(&self) -> Option<(usize, usize)> {
        let header = self.lines.iter().position(|l| l.trim() == MAIN_GROUP)?;
        let end = self.lines[header + 1..]
            .iter()
            .position(|l| l.trim_start().starts_with('['))
            .map_or(self.lines.len(), |i| header + 1 + i);
        Some((header + 1, end))
    }

    fn find(&self, key: &str) -> Option<usize> {
        let (start, end) = self.group()?;
        (start..end).find(|&i| split_kv(&self.lines[i]).is_some_and(|(k, _)| k == key))
    }

    /// Raw (still escaped) value of `key`, surrounding whitespace removed.
    pub fn get(&self, key: &str) -> Option<&str> {
        self.find(key)
            .and_then(|i| split_kv(&self.lines[i]))
            .map(|(_, v)| v)
    }

    /// String value with `\s \n \t \r \\` escapes resolved.
    pub fn get_string(&self, key: &str) -> Option<String> {
        self.get(key).map(unescape)
    }

    pub fn get_bool(&self, key: &str) -> Option<bool> {
        match self.get(key)? {
            "true" | "1" => Some(true),
            "false" | "0" => Some(false),
            _ => None,
        }
    }

    /// `;`-separated list (escaped `\;` kept inside items); empty items dropped.
    pub fn get_list(&self, key: &str) -> Vec<String> {
        let Some(raw) = self.get(key) else {
            return Vec::new();
        };
        let mut items = Vec::new();
        let mut cur = String::new();
        let mut chars = raw.chars();
        while let Some(c) = chars.next() {
            match c {
                '\\' => match chars.next() {
                    Some(';') => cur.push(';'),
                    Some(n) => {
                        cur.push('\\');
                        cur.push(n);
                    }
                    None => cur.push('\\'),
                },
                ';' => items.push(std::mem::take(&mut cur)),
                c => cur.push(c),
            }
        }
        items.push(cur);
        items
            .into_iter()
            .map(|s| unescape(&s))
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// Set `key=value` (value must already be escaped). Replaces the existing
    /// line in place, or appends after the group's last non-blank line.
    pub fn set(&mut self, key: &str, value: &str) {
        let line = format!("{key}={value}");
        if let Some(i) = self.find(key) {
            self.lines[i] = line;
            return;
        }
        match self.group() {
            Some((start, end)) => {
                let mut at = end;
                while at > start && self.lines[at - 1].trim().is_empty() {
                    at -= 1;
                }
                self.lines.insert(at, line);
            }
            None => {
                self.lines.insert(0, MAIN_GROUP.to_owned());
                self.lines.insert(1, line);
            }
        }
    }

    /// Remove every `key=` line of the main group; returns whether one existed.
    pub fn remove(&mut self, key: &str) -> bool {
        let mut removed = false;
        while let Some(i) = self.find(key) {
            self.lines.remove(i);
            removed = true;
        }
        removed
    }
}

fn split_kv(line: &str) -> Option<(&str, &str)> {
    let t = line.trim_start();
    if t.is_empty() || t.starts_with('#') || t.starts_with('[') {
        return None;
    }
    let (k, v) = t.split_once('=')?;
    Some((k.trim_end(), v.trim()))
}

/// Resolve desktop-entry string escapes.
pub fn unescape(s: &str) -> String {
    if !s.contains('\\') {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('s') => out.push(' '),
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('r') => out.push('\r'),
            Some('\\') => out.push('\\'),
            Some(o) => {
                out.push('\\');
                out.push(o);
            }
            None => out.push('\\'),
        }
    }
    out
}

/// Escape a plain string for a desktop-entry string value.
pub fn escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

/// Split an (unescaped) `Exec` value into arguments per the Desktop Entry
/// spec quoting rules. Field codes are kept verbatim.
pub fn exec_args(exec: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut cur = String::new();
    let mut in_arg = false;
    let mut chars = exec.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                in_arg = true;
                while let Some(q) = chars.next() {
                    match q {
                        '"' => break,
                        '\\' => {
                            if let Some(n) = chars.next() {
                                cur.push(n);
                            }
                        }
                        q => cur.push(q),
                    }
                }
            }
            c if c.is_whitespace() => {
                if in_arg {
                    args.push(std::mem::take(&mut cur));
                    in_arg = false;
                }
            }
            c => {
                cur.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(cur);
    }
    args
}

/// The command line without field codes (`%U`, `%f`, …), `%%` → `%`.
pub fn strip_field_codes(exec: &str) -> String {
    let mut out = Vec::new();
    for word in exec.split_whitespace() {
        if word.len() == 2 && word.starts_with('%') && word != "%%" {
            continue;
        }
        out.push(word.replace("%%", "%"));
    }
    out.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    const EXAMPLE: &str = "[Desktop Entry]\nType=Application\nName=Example App\n# comment\nExec=env X=1 /opt/x %u\nIcon=example-app\n\n[Desktop Action New]\nName=New\nHidden=true\n";

    #[test]
    fn get_reads_main_group_only() {
        let f = DesktopFile::parse(EXAMPLE);
        assert_eq!(f.get("Name"), Some("Example App"));
        assert_eq!(f.get("Hidden"), None);
        assert_eq!(f.get_bool("Hidden"), None);
        assert_eq!(f.render(), EXAMPLE);
    }

    #[test]
    fn set_then_remove_round_trips() {
        let mut f = DesktopFile::parse(EXAMPLE);
        f.set("Hidden", "true");
        let text = f.render();
        assert!(text.contains("Icon=example-app\nHidden=true\n\n[Desktop Action New]"));
        assert_eq!(DesktopFile::parse(&text).get_bool("Hidden"), Some(true));
        assert!(f.remove("Hidden"));
        assert_eq!(f.render(), EXAMPLE);
        assert!(!f.remove("Hidden"));
    }

    #[test]
    fn set_replaces_in_place_and_handles_spaces() {
        let mut f = DesktopFile::parse("[Desktop Entry]\nHidden = false\nName=A\n");
        assert_eq!(f.get_bool("Hidden"), Some(false));
        f.set("Hidden", "true");
        assert_eq!(f.render(), "[Desktop Entry]\nHidden=true\nName=A\n");
    }

    #[test]
    fn set_without_group_creates_it() {
        let mut f = DesktopFile::parse("");
        f.set("Name", "x");
        assert_eq!(f.render(), "[Desktop Entry]\nName=x\n");
    }

    #[test]
    fn localized_keys_are_distinct() {
        let f = DesktopFile::parse("[Desktop Entry]\nName[de]=Hallo\nName=Hello\n");
        assert_eq!(f.get("Name"), Some("Hello"));
    }

    #[test]
    fn lists_and_escapes() {
        let f = DesktopFile::parse(
            "[Desktop Entry]\nOnlyShowIn=GNOME;Unity;\nComment=a\\sb\\\\c\nX=a\\;b;c\n",
        );
        assert_eq!(f.get_list("OnlyShowIn"), ["GNOME", "Unity"]);
        assert_eq!(f.get_string("Comment").unwrap(), "a b\\c");
        assert_eq!(f.get_list("X"), ["a;b", "c"]);
        assert!(f.get_list("NotShowIn").is_empty());
        assert_eq!(escape("a\\b\nc"), "a\\\\b\\nc");
    }

    #[test]
    fn exec_parsing() {
        assert_eq!(
            exec_args(r#""/opt/my app/bin" --x "a \"q\"" %U"#),
            ["/opt/my app/bin", "--x", "a \"q\"", "%U"]
        );
        assert_eq!(strip_field_codes("/usr/bin/steam %U"), "/usr/bin/steam");
        assert_eq!(strip_field_codes("x 100%% %f"), "x 100%");
    }
}
