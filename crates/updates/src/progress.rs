//! Progress and failure parsing for streamed `LC_ALL=C pacman --noconfirm
//! --noprogressbar --color never` and `makepkg --noconfirm --nocolor` output.
//!
//! Phrasing follows pacman 7: `src/pacman/callback.c` (`cb_event`,
//! `cb_question`, `dload_init_event`), `src/pacman/sync.c`
//! (`sync_prepare_execute`), `src/pacman/util.c` (`trans_init_error`,
//! `question`, `_display_targets`) and makepkg's `msg`/`msg2`/`error` helpers
//! in `scripts/libmakepkg/util/message.sh.in`.

use serde::{Deserialize, Serialize};

/// Stage of a pacman transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Phase {
    Sync,
    Resolve,
    Download,
    Check,
    Install,
    Hooks,
    Done,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Phase::Sync => "Synchronizing package databases",
            Phase::Resolve => "Resolving dependencies",
            Phase::Download => "Downloading packages",
            Phase::Check => "Checking keys, integrity and conflicts",
            Phase::Install => "Installing packages",
            Phase::Hooks => "Running hooks",
            Phase::Done => "Finished",
        }
    }
}

/// One package operation of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Op {
    Install,
    Upgrade,
    Reinstall,
    Downgrade,
    Remove,
}

impl Op {
    fn verb(self) -> &'static str {
        match self {
            Op::Install => "Installing",
            Op::Upgrade => "Upgrading",
            Op::Reinstall => "Reinstalling",
            Op::Downgrade => "Downgrading",
            Op::Remove => "Removing",
        }
    }
}

/// `ALPM_EVENT_PACKAGE_OPERATION_START` lines: `upgrading foo...`.
const OPS: [(&str, Op); 5] = [
    ("installing ", Op::Install),
    ("upgrading ", Op::Upgrade),
    ("reinstalling ", Op::Reinstall),
    ("downgrading ", Op::Downgrade),
    ("removing ", Op::Remove),
];

/// Check steps printed with `--noprogressbar`, in transaction order.
const CHECKS: [(&str, &str); 6] = [
    ("checking keyring...", "Checking keyring"),
    ("downloading required keys...", "Downloading required keys"),
    (
        "checking package integrity...",
        "Checking package integrity",
    ),
    ("loading package files...", "Loading package files"),
    (
        "checking for file conflicts...",
        "Checking for file conflicts",
    ),
    (
        "checking available disk space...",
        "Checking available disk space",
    ),
];

/// Fraction bands of the transaction stages.
const SYNC_END: f64 = 0.05;
const RESOLVE: f64 = 0.05;
const DOWNLOAD: (f64, f64) = (0.1, 0.5);
const CHECK: (f64, f64) = (0.5, 0.54);
const PRE_HOOKS: (f64, f64) = (0.54, 0.55);
const INSTALL: (f64, f64) = (0.55, 0.9);
const POST_HOOKS: (f64, f64) = (0.9, 1.0);

/// Progress of one pacman run, fed line by line.
#[derive(Debug, Default, Clone)]
pub struct PacmanProgress {
    phase: Option<Phase>,
    fraction: f64,
    detail: String,
    changes: Vec<(Op, String)>,
    pacnew: Vec<String>,
    nothing_to_do: bool,
    package_count: Option<usize>,
    /// `[removal]` entries of the package list (nothing to download).
    removals: usize,
    /// Inside the wrapped `Packages (N) …` list.
    in_package_list: bool,
    databases: i32,
    downloads: usize,
    post_hooks: bool,
}

impl PacmanProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one output line (no trailing newline). Returns true when
    /// phase/fraction/detail (or changes, pacnew, package count) changed.
    pub fn feed(&mut self, line: &str) -> bool {
        let line = line.trim_end();
        if self.in_package_list {
            // `list_display` indents wrapped lines under the title.
            if line.starts_with(' ') && !line.trim_start().is_empty() {
                self.removals += count_removals(line);
                return false;
            }
            self.in_package_list = false;
        }
        if let Some(rest) = line.strip_prefix(":: ") {
            return self.colon(rest);
        }
        if let Some(rest) = line.strip_prefix("warning: ") {
            return self.warning(rest);
        }
        if let Some(rest) = line.strip_prefix("Packages (") {
            return self.package_list(rest);
        }
        match line {
            "resolving dependencies..." | "checking dependencies..." => {
                return self.set(Phase::Resolve, RESOLVE, Phase::Resolve.label());
            }
            "loading packages..." => return self.set(Phase::Resolve, RESOLVE, "Loading packages"),
            "looking for conflicting packages..." => {
                return self.set(Phase::Resolve, 0.06, "Looking for conflicting packages");
            }
            " there is nothing to do" => {
                self.nothing_to_do = true;
                self.set(Phase::Done, 1.0, "Nothing to do");
                return true;
            }
            _ => {}
        }
        if let Some(i) = CHECKS.iter().position(|(text, _)| *text == line) {
            let f = CHECK.0 + (CHECK.1 - CHECK.0) * i as f64 / CHECKS.len() as f64;
            return self.set(Phase::Check, f, CHECKS[i].1);
        }
        match self.phase {
            Some(Phase::Sync) => self.database(line),
            Some(Phase::Download) => self.download(line),
            Some(Phase::Install) => self.operation(line),
            Some(Phase::Hooks) => self.hook(line),
            _ => false,
        }
    }

    /// Mark a successful exit: phase [`Phase::Done`], fraction 1.0.
    pub fn finish(&mut self) {
        self.set(Phase::Done, 1.0, Phase::Done.label());
    }

    pub fn phase(&self) -> Option<Phase> {
        self.phase
    }

    /// Overall fraction 0.0..=1.0 of this transaction, never decreasing.
    pub fn fraction(&self) -> f64 {
        self.fraction
    }

    /// Short detail for the UI, e.g. "Upgrading 5 of 12: mesa".
    pub fn detail(&self) -> String {
        self.detail.clone()
    }

    /// Package changes seen (`upgrading foo...` etc.), in order.
    pub fn changes(&self) -> &[(Op, String)] {
        &self.changes
    }

    /// `.pacnew`/`.pacsave` paths reported in warnings.
    pub fn pacnew(&self) -> &[String] {
        &self.pacnew
    }

    /// `there is nothing to do` seen.
    pub fn nothing_to_do(&self) -> bool {
        self.nothing_to_do
    }

    fn set(&mut self, phase: Phase, fraction: f64, detail: &str) -> bool {
        let fraction = fraction.clamp(0.0, 1.0).max(self.fraction);
        let changed =
            self.phase != Some(phase) || fraction != self.fraction || self.detail != detail;
        self.phase = Some(phase);
        self.fraction = fraction;
        if self.detail != detail {
            detail.clone_into(&mut self.detail);
        }
        changed
    }

    /// `colon_printf` lines and `question` prompts.
    fn colon(&mut self, rest: &str) -> bool {
        match rest {
            "Synchronizing package databases..." => self.set(Phase::Sync, 0.0, Phase::Sync.label()),
            "Starting full system upgrade..." => {
                self.set(Phase::Resolve, RESOLVE, "Starting full system upgrade")
            }
            "Retrieving packages..." => {
                self.set(Phase::Download, DOWNLOAD.0, Phase::Download.label())
            }
            "Running pre-transaction hooks..." => {
                self.post_hooks = false;
                self.set(Phase::Hooks, PRE_HOOKS.0, "Running pre-transaction hooks")
            }
            "Processing package changes..." => {
                self.set(Phase::Install, INSTALL.0, Phase::Install.label())
            }
            "Running post-transaction hooks..." => {
                self.post_hooks = true;
                self.set(Phase::Hooks, POST_HOOKS.0, "Running post-transaction hooks")
            }
            "Proceed with installation? [Y/n]" | "Proceed with download? [Y/n]" => {
                self.set(Phase::Resolve, DOWNLOAD.0, "Starting transaction")
            }
            _ => match rest
                .strip_prefix("Replace ")
                .and_then(|r| r.strip_suffix("? [Y/n]"))
            {
                Some(what) => self.set(Phase::Resolve, 0.07, &format!("Replacing {what}")),
                None => false,
            },
        }
    }

    /// `ALPM_EVENT_PACNEW_CREATED`/`ALPM_EVENT_PACSAVE_CREATED` warnings.
    fn warning(&mut self, rest: &str) -> bool {
        let Some(path) = config_backup(rest, " installed as ", ".pacnew")
            .or_else(|| config_backup(rest, " saved as ", ".pacsave"))
        else {
            return false;
        };
        self.pacnew.push(path);
        true
    }

    /// `Packages (N) a-1-1  b-2-1  c-1-1 [removal]` (the text after `Packages (`).
    fn package_list(&mut self, rest: &str) -> bool {
        let Some((n, list)) = rest.split_once(") ") else {
            return false;
        };
        let Ok(n) = n.parse::<usize>() else {
            return false;
        };
        self.package_count = Some(n);
        self.removals = count_removals(list);
        self.in_package_list = true;
        let detail = format!("{n} package{} to change", if n == 1 { "" } else { "s" });
        self.set(Phase::Resolve, 0.08, &detail);
        true
    }

    /// ` core downloading...` while synchronizing.
    fn database(&mut self, line: &str) -> bool {
        let Some(rest) = line.strip_prefix(' ') else {
            return false;
        };
        if let Some(db) = name_before(rest, " downloading...") {
            self.databases += 1;
            let f = SYNC_END * (1.0 - 0.5f64.powi(self.databases));
            return self.set(Phase::Sync, f, &format!("Synchronizing {db}"));
        }
        match name_before(rest, " is up to date") {
            Some(db) => self.set(Phase::Sync, self.fraction, &format!("{db} is up to date")),
            None => false,
        }
    }

    /// ` mesa-1:24.2.4-1-x86_64 downloading...` after `:: Retrieving packages...`.
    fn download(&mut self, line: &str) -> bool {
        let Some(file) = line
            .strip_prefix(' ')
            .and_then(|r| name_before(r, " downloading..."))
        else {
            return false;
        };
        self.downloads += 1;
        let total = self
            .package_count
            .map_or(0, |n| n.saturating_sub(self.removals))
            .max(self.downloads);
        let f = DOWNLOAD.0 + (DOWNLOAD.1 - DOWNLOAD.0) * self.downloads as f64 / total as f64;
        let detail = format!("Downloading {} of {total}: {file}", self.downloads);
        self.set(Phase::Download, f, &detail)
    }

    /// `upgrading foo...` after `:: Processing package changes...`.
    fn operation(&mut self, line: &str) -> bool {
        let Some((op, name)) = OPS
            .iter()
            .find_map(|(prefix, op)| Some((*op, name_before(line.strip_prefix(prefix)?, "...")?)))
        else {
            return false;
        };
        self.changes.push((op, name.to_owned()));
        let k = self.changes.len();
        let total = self.package_count.unwrap_or(k).max(k);
        let f = INSTALL.0 + (INSTALL.1 - INSTALL.0) * k as f64 / total as f64;
        self.set(
            Phase::Install,
            f,
            &format!("{} {k} of {total}: {name}", op.verb()),
        );
        true
    }

    /// `( 2/12) Arming ConditionNeedsUpdate...` (`ALPM_EVENT_HOOK_RUN_START`).
    fn hook(&mut self, line: &str) -> bool {
        let Some((pos, desc)) = line.strip_prefix('(').and_then(|r| r.split_once(") ")) else {
            return false;
        };
        let Some((i, n)) = pos.split_once('/') else {
            return false;
        };
        let (Ok(i), Ok(n)) = (i.trim().parse::<usize>(), n.trim().parse::<usize>()) else {
            return false;
        };
        if n == 0 || i > n {
            return false;
        }
        let (start, end) = if self.post_hooks {
            POST_HOOKS
        } else {
            PRE_HOOKS
        };
        let f = start + (end - start) * i as f64 / n as f64;
        let desc = match desc.strip_suffix("...") {
            Some(d) => format!("{d}…"),
            None => desc.to_owned(),
        };
        self.set(Phase::Hooks, f, &format!("Hook {i} of {n}: {desc}"))
    }
}

/// `name` of `"{name}{suffix}"` when it is a single token.
fn name_before<'a>(text: &'a str, suffix: &str) -> Option<&'a str> {
    text.strip_suffix(suffix)
        .filter(|name| !name.is_empty() && !name.contains(char::is_whitespace))
}

fn count_removals(list: &str) -> usize {
    list.split_whitespace()
        .filter(|t| *t == "[removal]")
        .count()
}

/// The new path of `"{path}{sep}{path}{ext}"` (pacman prints the same path twice).
fn config_backup(text: &str, sep: &str, ext: &str) -> Option<String> {
    let body = text.strip_suffix(ext)?;
    let doubled = body.len().checked_sub(sep.len())?;
    if doubled == 0 || doubled % 2 != 0 {
        return None;
    }
    let (path, tail) = body.split_at_checked(doubled / 2)?;
    (tail.strip_prefix(sep)? == path).then(|| format!("{path}{ext}"))
}

/// Why a pacman run failed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Failure {
    /// db.lck held by another package manager.
    Locked,
    /// --noconfirm answered a [y/N] prompt (conflict/removal/replacement) with
    /// "no"; `prompt` = the prompt line(s).
    NeedsConfirmation {
        prompt: String,
    },
    /// Files already on disk that the transaction would overwrite.
    FileConflicts {
        files: Vec<String>,
    },
    Download {
        message: String,
    },
    Signature {
        message: String,
    },
    /// Last `error:` lines joined.
    Other {
        message: String,
    },
}

impl Failure {
    /// User-facing explanation with the next step to take.
    pub fn explanation(&self) -> String {
        match self {
            Failure::Locked => "Another package manager is running (pacman's database is locked). \
                 Wait for it to finish, then try again. If none is running, a stale \
                 /var/lib/pacman/db.lck is left over from a crash."
                .to_owned(),
            Failure::NeedsConfirmation { prompt } => {
                format!(
                    "pacman needs an answer to: {prompt} Run the update in a terminal to decide."
                )
            }
            Failure::FileConflicts { files } if files.is_empty() => {
                "The update would overwrite files that already exist on disk. Run it in a \
                 terminal to see which, move them aside, then try again."
                    .to_owned()
            }
            Failure::FileConflicts { files } => {
                let mut list = files
                    .iter()
                    .take(5)
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ");
                if files.len() > 5 {
                    list.push_str(&format!(" and {} more", files.len() - 5));
                }
                format!(
                    "The update would overwrite files that already exist on disk: {list}. \
                     Check where they came from (pacman -Qo), move them aside, then try again."
                )
            }
            Failure::Download { message } => format!(
                "A download failed: {}. Check the network connection and the mirror list, \
                 then try again.",
                message.replace('\n', "; ")
            ),
            Failure::Signature { message } => format!(
                "A package signature could not be verified: {}. The keyring is probably \
                 outdated: run `sudo pacman -Sy archlinux-keyring && sudo pacman -Su` in a \
                 terminal, then try again.",
                message.replace('\n', "; ")
            ),
            Failure::Other { message } => format!("pacman failed: {}", message.replace('\n', "; ")),
        }
    }
}

/// Error texts from libalpm's `signing.c`/`error.c` that mean a key or signature problem.
const SIGNATURE_ERRORS: [&str; 10] = [
    "signature from \"",
    ": key \"",
    "missing required signature",
    "(PGP signature)",
    "invalid PGP signature",
    "missing PGP signature",
    "required key missing from keyring",
    "keyring is not writable",
    "Public keyring not found",
    "signature format error",
];

/// Error texts from libalpm's `dload.c`/`error.c` that mean a failed download.
const DOWNLOAD_ERRORS: [&str; 4] = [
    "failed retrieving file ",
    "failed to retrieve some files",
    "failed to synchronize all databases",
    "download library error",
];

/// Classify a failed pacman run (exit != 0) from its output lines.
pub fn classify_failure(lines: &[String]) -> Failure {
    let lines: Vec<&str> = lines.iter().map(|l| l.trim_end()).collect();
    let errors = || lines.iter().filter_map(|l| l.strip_prefix("error: "));

    // `trans_init_error`: "failed to init transaction (unable to lock database)".
    if errors().any(|e| {
        e.contains("(unable to lock database)") || e.starts_with("could not lock database")
    }) {
        return Failure::Locked;
    }
    if let Some(prompt) = declined_prompts(&lines) {
        return Failure::NeedsConfirmation { prompt };
    }
    if let Some(at) = lines
        .iter()
        .position(|l| *l == "error: failed to commit transaction (conflicting files)")
    {
        let files = lines[at + 1..]
            .iter()
            .filter_map(|l| conflicting_file(l))
            .collect();
        return Failure::FileConflicts { files };
    }
    let matching = |patterns: &[&str]| {
        let mut found: Vec<&str> = Vec::new();
        for e in errors().filter(|e| patterns.iter().any(|p| e.contains(p))) {
            if !found.contains(&e) {
                found.push(e);
            }
        }
        found.join("\n")
    };
    let message = matching(&SIGNATURE_ERRORS);
    if !message.is_empty() {
        return Failure::Signature { message };
    }
    let message = matching(&DOWNLOAD_ERRORS);
    if !message.is_empty() {
        return Failure::Download { message };
    }
    let all: Vec<&str> = errors().collect();
    let message = if all.is_empty() {
        lines
            .iter()
            .rev()
            .find(|l| !l.trim().is_empty())
            .map_or_else(
                || "pacman exited with an error and printed nothing".to_owned(),
                |l| l.trim().to_owned(),
            )
    } else {
        all[all.len().saturating_sub(3)..].join("\n")
    };
    Failure::Other { message }
}

/// `[y/N]` prompts (`noyes`), which `--noconfirm` answers with "no".
fn declined_prompts(lines: &[&str]) -> Option<String> {
    let mut prompts = Vec::new();
    for (at, line) in lines.iter().enumerate() {
        let Some(question) = line
            .strip_prefix(":: ")
            .and_then(|l| l.strip_suffix(" [y/N]"))
        else {
            continue;
        };
        // `ALPM_QUESTION_REMOVE_PKGS` lists the packages before asking.
        if question.starts_with("Do you want to skip the above package")
            && let Some(header) = lines[..at]
                .iter()
                .rposition(|l| l.starts_with(":: The following package"))
        {
            let names: Vec<&str> = lines[header + 1..at]
                .iter()
                .flat_map(|l| l.split_whitespace())
                .collect();
            prompts.push(format!("{} {}.", &lines[header][3..], names.join(", ")));
        }
        prompts.push(question.to_owned());
    }
    (!prompts.is_empty()).then(|| prompts.join(" "))
}

/// Path of `pkg: /path exists in filesystem[ (owned by other)]` or
/// `/path exists in both 'a' and 'b'`.
fn conflicting_file(line: &str) -> Option<String> {
    if let Some(at) = line.rfind(" exists in filesystem") {
        let tail = &line[at + " exists in filesystem".len()..];
        if !tail.is_empty() && !tail.starts_with(" (owned by ") {
            return None;
        }
        let (_, path) = line[..at].split_once(": ")?;
        return path.starts_with('/').then(|| path.to_owned());
    }
    let at = line.find(" exists in both '")?;
    let path = &line[..at];
    path.starts_with('/').then(|| path.to_owned())
}

/// makepkg stage bands: `==> ` messages starting each stage, and the fraction
/// range the stage covers.
const MAKEPKG_STAGES: [(&[&str], f64, f64); 7] = [
    (
        &[
            "Making package: ",
            "Checking runtime dependencies...",
            "Checking buildtime dependencies...",
            "Installing missing dependencies...",
            "Retrieving sources...",
        ],
        0.0,
        0.15,
    ),
    (
        &[
            "Validating ",
            "Verifying source file signatures",
            "Starting verify()...",
            "Removing existing $srcdir/ directory...",
            "Extracting sources...",
        ],
        0.15,
        0.25,
    ),
    (
        &[
            "Starting prepare()...",
            "Starting pkgver()...",
            "Updated version: ",
            "Sources are ready.",
        ],
        0.25,
        0.3,
    ),
    (&["Starting build()..."], 0.3, 0.8),
    (&["Starting check()..."], 0.8, 0.85),
    (
        &[
            "Entering fakeroot environment...",
            "Removing existing $pkgdir/ directory...",
            "Package directory is ready.",
            "Starting package",
            "Tidying install...",
            "Checking for packaging issues...",
            "Creating package ",
            "Leaving fakeroot environment.",
            "Signing package",
        ],
        0.85,
        0.97,
    ),
    (&["Finished making: ", "Source package created: "], 1.0, 1.0),
];

/// Progress of one makepkg run, fed line by line.
#[derive(Debug, Default, Clone)]
pub struct MakepkgProgress {
    fraction: f64,
    /// Upper end of the current stage band.
    stage_end: f64,
    step: Option<String>,
    error: Option<String>,
}

impl MakepkgProgress {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one output line. `==> ` step lines (makepkg `msg`) drive the
    /// stage; `  -> ` sub-steps (`msg2`) advance within it. Returns true when
    /// fraction, step or error changed.
    pub fn feed(&mut self, line: &str) -> bool {
        let line = line.trim_end();
        if line.starts_with("  -> ") {
            return self.advance(0.25);
        }
        let Some(text) = line.strip_prefix("==> ") else {
            return false;
        };
        if let Some(error) = text.strip_prefix("ERROR: ") {
            if self.error.is_some() {
                return false;
            }
            self.error = Some(error.to_owned());
            return true;
        }
        if text.starts_with("WARNING: ") {
            return false;
        }
        let stage = MAKEPKG_STAGES
            .iter()
            .find(|(starts, ..)| starts.iter().any(|s| text.starts_with(s)));
        let mut changed = self.step.as_deref() != Some(text);
        if changed {
            self.step = Some(text.to_owned());
        }
        if let Some(&(_, start, end)) = stage {
            if start > self.fraction || end > self.stage_end {
                changed |= start > self.fraction;
                self.fraction = self.fraction.max(start);
                self.stage_end = end;
            } else {
                changed |= self.advance(0.5);
            }
        }
        changed
    }

    /// Overall fraction 0.0..=1.0, never decreasing.
    pub fn fraction(&self) -> f64 {
        self.fraction
    }

    /// Last `==> ` message text, e.g. "Starting build()...".
    pub fn step(&self) -> Option<String> {
        self.step.clone()
    }

    /// `==> ERROR: ...` text (the first one; later ones are follow-ups).
    pub fn error(&self) -> Option<String> {
        self.error.clone()
    }

    /// Move `share` of the remaining way towards the current stage's end.
    fn advance(&mut self, share: f64) -> bool {
        let next = self.fraction + (self.stage_end - self.fraction) * share;
        let changed = next > self.fraction;
        self.fraction = self.fraction.max(next);
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `pacman -Syu` with a replacement (`vulkan-foo` → `vulkan-bar`), three
    /// downloads, pre/post hooks and a `.pacnew`.
    const SYU: &str = "\
:: Synchronizing package databases...
 core downloading...
 extra downloading...
 multilib downloading...
:: Starting full system upgrade...
:: Replace vulkan-foo with extra/vulkan-bar? [Y/n] 
resolving dependencies...
looking for conflicting packages...

Packages (4) linux-6.11.1.arch1-1  mesa-1:24.2.4-1  vulkan-bar-1.1-1  vulkan-foo-1.0-1 [removal]

Total Download Size:   171.25 MiB
Total Installed Size:  412.80 MiB
Net Upgrade Size:        2.13 MiB

:: Proceed with installation? [Y/n] 
:: Retrieving packages...
 linux-6.11.1.arch1-1-x86_64 downloading...
 mesa-1:24.2.4-1-x86_64 downloading...
 vulkan-bar-1.1-1-x86_64 downloading...
checking keyring...
checking package integrity...
loading package files...
checking for file conflicts...
checking available disk space...
:: Running pre-transaction hooks...
(1/1) Performing snapper pre snapshots for the following configurations...
==> root: 126
:: Processing package changes...
removing vulkan-foo...
upgrading linux...
upgrading mesa...
installing vulkan-bar...
Optional dependencies for vulkan-bar
    vulkan-tools: vulkaninfo [installed]
warning: /etc/mkinitcpio.d/linux.preset installed as /etc/mkinitcpio.d/linux.preset.pacnew
:: Running post-transaction hooks...
( 1/10) Arming ConditionNeedsUpdate...
( 2/10) Updating module dependencies...
( 3/10) Updating linux initcpios...
( 4/10) Reloading system manager configuration...
( 5/10) Creating temporary files...
( 6/10) Reloading device manager configuration...
( 7/10) Updating udev hardware database...
( 8/10) Restarting marked services...
( 9/10) Updating the MIME type database...
(10/10) Checking which packages need to be rebuilt";

    const NOTHING: &str = "\
:: Synchronizing package databases...
 core downloading...
 extra downloading...
:: Starting full system upgrade...
 there is nothing to do";

    /// `pacman -U` of freshly built packages: no download phase.
    const UPGRADE_FILES: &str = "\
loading packages...
resolving dependencies...
looking for conflicting packages...

Packages (2) hyprpicker-0.4.1-1  hyprpicker-debug-0.4.1-1

Total Installed Size:  0.42 MiB
Net Upgrade Size:      0.01 MiB

:: Proceed with installation? [Y/n] 
checking keyring...
checking package integrity...
loading package files...
checking for file conflicts...
checking available disk space...
:: Processing package changes...
upgrading hyprpicker...
upgrading hyprpicker-debug...
:: Running post-transaction hooks...
(1/1) Arming ConditionNeedsUpdate...";

    const CONFLICT: &str = "\
:: Synchronizing package databases...
 core downloading...
:: Starting full system upgrade...
resolving dependencies...
looking for conflicting packages...
:: pipewire-jack-1:1.2.5-1 and jack2-1.9.22-1 are in conflict (jack). Remove jack2? [y/N] 
error: unresolvable package conflicts detected
error: failed to prepare transaction (conflicting dependencies)
:: pipewire-jack-1:1.2.5-1 and jack2-1.9.22-1 are in conflict (jack)";

    const LOCKED: &str = "\
error: failed to init transaction (unable to lock database)
error: could not lock database: File exists
  if you're sure a package manager is not already
  running, you can remove /var/lib/pacman/db.lck";

    const FILE_CONFLICTS: &str = "\
checking for file conflicts...
error: failed to commit transaction (conflicting files)
python-foo: /usr/lib/python3.12/site-packages/foo/__init__.py exists in filesystem
python-foo: /usr/bin/foo exists in filesystem (owned by foo-git)
Errors occurred, no packages were upgraded.";

    const SIGNATURE: &str = "\
checking keyring...
checking package integrity...
error: mesa: signature from \"Some Packager <packager@archlinux.org>\" is unknown trust
:: File /var/cache/pacman/pkg/mesa-1:24.2.4-1-x86_64.pkg.tar.zst is corrupted (invalid or corrupted package (PGP signature)).
Do you want to delete it? [Y/n] 
error: failed to commit transaction (invalid or corrupted package (PGP signature))
Errors occurred, no packages were upgraded.";

    const DOWNLOAD: &str = "\
:: Retrieving packages...
 mesa-1:24.2.4-1-x86_64 downloading...
error: failed retrieving file 'mesa-1:24.2.4-1-x86_64.pkg.tar.zst' from mirror.example.org : The requested URL returned error: 404
warning: failed to retrieve some files
error: failed to commit transaction (failed to retrieve some files)
Errors occurred, no packages were upgraded.";

    const SKIP: &str = "\
:: Starting full system upgrade...
:: The following packages cannot be upgraded due to unresolvable dependencies:
      foo  bar

:: Do you want to skip the above packages for this upgrade? [y/N] 
error: failed to prepare transaction (could not satisfy dependencies)
:: unable to satisfy dependency 'libbaz.so=2-64' required by foo";

    const MAKEPKG: &str = "\
==> Making package: hyprpicker 0.4.1-1 (Sat Oct  4 12:00:00 2026)
==> Checking runtime dependencies...
==> Checking buildtime dependencies...
==> Retrieving sources...
  -> Downloading hyprpicker-0.4.1.tar.gz...
==> Validating source files with sha256sums...
    hyprpicker-0.4.1.tar.gz ... Passed
==> Removing existing $srcdir/ directory...
==> Extracting sources...
  -> Extracting hyprpicker-0.4.1.tar.gz with bsdtar
==> Starting prepare()...
==> Starting build()...
-- The CXX compiler identification is GNU 14.2.1
[100%] Built target hyprpicker
==> Starting check()...
==> Entering fakeroot environment...
==> Starting package()...
==> Tidying install...
  -> Removing libtool files...
  -> Purging unwanted files...
  -> Stripping unneeded symbols from binaries and libraries...
==> Checking for packaging issues...
==> WARNING: Package contains reference to $srcdir
==> Creating package \"hyprpicker\"...
  -> Generating .PKGINFO file...
  -> Generating .BUILDINFO file...
  -> Generating .MTREE file...
  -> Compressing package...
==> Leaving fakeroot environment.
==> Finished making: hyprpicker 0.4.1-1 (Sat Oct  4 12:01:00 2026)";

    const MAKEPKG_FAIL: &str = "\
==> Making package: hyprpicker 0.4.1-1 (Sat Oct  4 12:00:00 2026)
==> Retrieving sources...
==> Extracting sources...
==> Starting build()...
make: *** [Makefile:10: all] Error 1
==> ERROR: A failure occurred in build().
    Aborting...";

    /// Feed a transcript; returns the distinct phases in order and checks monotonicity.
    fn run(text: &str) -> (PacmanProgress, Vec<Phase>) {
        let mut p = PacmanProgress::new();
        let mut phases = Vec::new();
        let mut last = 0.0;
        for line in text.lines() {
            p.feed(line);
            assert!(p.fraction() >= last, "fraction decreased at {line:?}");
            assert!((0.0..=1.0).contains(&p.fraction()));
            last = p.fraction();
            if let Some(phase) = p.phase()
                && phases.last() != Some(&phase)
            {
                phases.push(phase);
            }
        }
        (p, phases)
    }

    fn lines(text: &str) -> Vec<String> {
        text.lines().map(str::to_owned).collect()
    }

    #[test]
    fn full_upgrade() {
        let mut p = PacmanProgress::new();
        assert_eq!(p.phase(), None);
        assert!(p.feed(":: Synchronizing package databases..."));
        assert!(!p.feed("Total Download Size:   171.25 MiB"));

        let (mut p, phases) = run(SYU);
        use Phase::*;
        assert_eq!(
            phases,
            [Sync, Resolve, Download, Check, Hooks, Install, Hooks]
        );
        assert_eq!(p.package_count, Some(4));
        assert_eq!(
            p.changes(),
            [
                (Op::Remove, "vulkan-foo".to_owned()),
                (Op::Upgrade, "linux".to_owned()),
                (Op::Upgrade, "mesa".to_owned()),
                (Op::Install, "vulkan-bar".to_owned()),
            ]
        );
        assert_eq!(p.pacnew(), ["/etc/mkinitcpio.d/linux.preset.pacnew"]);
        assert!(!p.nothing_to_do());
        assert_eq!(
            p.detail(),
            "Hook 10 of 10: Checking which packages need to be rebuilt"
        );
        assert_eq!(p.fraction(), 1.0);
        p.finish();
        assert_eq!(p.phase(), Some(Done));
        assert_eq!(p.detail(), "Finished");
    }

    #[test]
    fn details_along_the_way() {
        let mut p = PacmanProgress::new();
        let mut seen = Vec::new();
        for line in SYU.lines() {
            if p.feed(line) {
                seen.push((p.detail(), p.fraction()));
            }
        }
        let at = |d: &str, expected: f64| {
            let (_, f) = seen
                .iter()
                .find(|(s, _)| s == d)
                .unwrap_or_else(|| panic!("missing detail {d:?} in {seen:#?}"));
            assert!((f - expected).abs() < 1e-9, "{d:?}: {f} != {expected}");
        };
        at("Synchronizing core", 0.025);
        at("Synchronizing multilib", 0.05 * 0.875);
        at("Replacing vulkan-foo with extra/vulkan-bar", 0.07);
        at("4 packages to change", 0.08);
        at("Starting transaction", 0.1);
        // Three downloads: the `[removal]` entry is not downloaded.
        at(
            "Downloading 1 of 3: linux-6.11.1.arch1-1-x86_64",
            0.1 + 0.4 / 3.0,
        );
        at("Downloading 3 of 3: vulkan-bar-1.1-1-x86_64", 0.5);
        at("Checking package integrity", 0.5 + 0.04 * 2.0 / 6.0);
        at("Checking available disk space", 0.5 + 0.04 * 5.0 / 6.0);
        at(
            "Hook 1 of 1: Performing snapper pre snapshots for the following configurations…",
            0.55,
        );
        at("Removing 1 of 4: vulkan-foo", 0.55 + 0.35 / 4.0);
        at("Upgrading 3 of 4: mesa", 0.55 + 0.35 * 3.0 / 4.0);
        at("Installing 4 of 4: vulkan-bar", 0.9);
        at("Hook 2 of 10: Updating module dependencies…", 0.92);
    }

    #[test]
    fn nothing_to_do() {
        let (p, phases) = run(NOTHING);
        assert_eq!(phases, [Phase::Sync, Phase::Resolve, Phase::Done]);
        assert!(p.nothing_to_do());
        assert_eq!(p.fraction(), 1.0);
        assert!(p.changes().is_empty());
        assert_eq!(p.package_count, None);
    }

    #[test]
    fn local_packages_skip_download() {
        let (p, phases) = run(UPGRADE_FILES);
        use Phase::*;
        assert_eq!(phases, [Resolve, Check, Install, Hooks]);
        assert_eq!(p.package_count, Some(2));
        assert_eq!(
            p.changes(),
            [
                (Op::Upgrade, "hyprpicker".to_owned()),
                (Op::Upgrade, "hyprpicker-debug".to_owned()),
            ]
        );
        assert_eq!(p.fraction(), 1.0);
    }

    #[test]
    fn wrapped_package_list() {
        let mut p = PacmanProgress::new();
        p.feed("Packages (3) a-1-1  b-1-1");
        p.feed("             c-1-1 [removal]");
        p.feed("");
        p.feed(":: Retrieving packages...");
        p.feed(" a-1-1-x86_64 downloading...");
        assert_eq!(p.detail(), "Downloading 1 of 2: a-1-1-x86_64");
    }

    #[test]
    fn ops_only_while_processing() {
        let mut p = PacmanProgress::new();
        assert!(!p.feed("removing old packages from cache..."));
        p.feed(":: Processing package changes...");
        assert!(!p.feed("removing old packages from cache..."));
        assert!(p.feed("downgrading foo..."));
        assert!(p.feed("reinstalling bar..."));
        assert_eq!(
            p.changes(),
            [
                (Op::Downgrade, "foo".to_owned()),
                (Op::Reinstall, "bar".to_owned())
            ]
        );
    }

    #[test]
    fn pacsave_and_odd_warnings() {
        let mut p = PacmanProgress::new();
        assert!(p.feed("warning: /etc/foo.conf saved as /etc/foo.conf.pacsave"));
        assert!(!p.feed("warning: /etc/a installed as /etc/b.pacnew"));
        assert!(!p.feed("warning: failed to retrieve some files"));
        assert_eq!(p.pacnew(), ["/etc/foo.conf.pacsave"]);
    }

    #[test]
    fn classify_conflict_prompt() {
        let (p, _) = run(CONFLICT);
        assert!(p.changes().is_empty());
        let failure = classify_failure(&lines(CONFLICT));
        let prompt =
            "pipewire-jack-1:1.2.5-1 and jack2-1.9.22-1 are in conflict (jack). Remove jack2?";
        assert_eq!(
            failure,
            Failure::NeedsConfirmation {
                prompt: prompt.to_owned()
            }
        );
        assert_eq!(
            failure.explanation(),
            format!("pacman needs an answer to: {prompt} Run the update in a terminal to decide.")
        );
    }

    #[test]
    fn classify_skip_prompt() {
        assert_eq!(
            classify_failure(&lines(SKIP)),
            Failure::NeedsConfirmation {
                prompt: "The following packages cannot be upgraded due to unresolvable \
                         dependencies: foo, bar. Do you want to skip the above packages for \
                         this upgrade?"
                    .to_owned()
            }
        );
    }

    #[test]
    fn classify_lock() {
        let failure = classify_failure(&lines(LOCKED));
        assert_eq!(failure, Failure::Locked);
        assert!(failure.explanation().contains("/var/lib/pacman/db.lck"));
    }

    #[test]
    fn classify_file_conflicts() {
        let failure = classify_failure(&lines(FILE_CONFLICTS));
        assert_eq!(
            failure,
            Failure::FileConflicts {
                files: vec![
                    "/usr/lib/python3.12/site-packages/foo/__init__.py".to_owned(),
                    "/usr/bin/foo".to_owned(),
                ]
            }
        );
        assert!(failure.explanation().contains("/usr/bin/foo."));
        assert_eq!(
            conflicting_file("/usr/bin/x exists in both 'a' and 'b'").as_deref(),
            Some("/usr/bin/x")
        );
    }

    #[test]
    fn classify_signature_download_other() {
        assert_eq!(
            classify_failure(&lines(SIGNATURE)),
            Failure::Signature {
                message: "mesa: signature from \"Some Packager <packager@archlinux.org>\" is \
                          unknown trust\nfailed to commit transaction (invalid or corrupted \
                          package (PGP signature))"
                    .to_owned()
            }
        );
        let Failure::Download { message } = classify_failure(&lines(DOWNLOAD)) else {
            panic!("not a download failure");
        };
        assert!(message.starts_with("failed retrieving file 'mesa-1:24.2.4-1-x86_64.pkg"));
        assert!(message.ends_with("failed to commit transaction (failed to retrieve some files)"));
        assert_eq!(
            classify_failure(&lines(
                "error: failed to commit transaction (not enough free disk space)\n\
                 Errors occurred, no packages were upgraded."
            )),
            Failure::Other {
                message: "failed to commit transaction (not enough free disk space)".to_owned()
            }
        );
        assert_eq!(
            classify_failure(&[]),
            Failure::Other {
                message: "pacman exited with an error and printed nothing".to_owned()
            }
        );
    }

    #[test]
    fn makepkg_success() {
        let mut m = MakepkgProgress::new();
        let mut last = 0.0;
        let mut build = None;
        for line in MAKEPKG.lines() {
            m.feed(line);
            assert!(m.fraction() >= last, "fraction decreased at {line:?}");
            last = m.fraction();
            if line == "==> Starting build()..." {
                build = Some(m.fraction());
            }
            if line == "==> Validating source files with sha256sums..." {
                assert_eq!(m.fraction(), 0.15);
            }
            if line.starts_with("==> Creating package") {
                assert!((0.85..0.97).contains(&m.fraction()));
            }
        }
        assert_eq!(build, Some(0.3));
        assert_eq!(m.fraction(), 1.0);
        assert_eq!(
            m.step().as_deref(),
            Some("Finished making: hyprpicker 0.4.1-1 (Sat Oct  4 12:01:00 2026)")
        );
        assert_eq!(m.error(), None);
    }

    #[test]
    fn makepkg_failure() {
        let mut m = MakepkgProgress::new();
        for line in MAKEPKG_FAIL.lines() {
            m.feed(line);
        }
        assert_eq!(m.step().as_deref(), Some("Starting build()..."));
        assert_eq!(m.error().as_deref(), Some("A failure occurred in build()."));
        assert!((0.3..0.8).contains(&m.fraction()));
        assert!(!m.feed("==> ERROR: Makepkg was unable to build hyprpicker."));
        assert_eq!(m.error().as_deref(), Some("A failure occurred in build()."));
    }
}
