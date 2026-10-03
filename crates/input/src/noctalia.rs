//! Noctalia IPC commands, enumerated from `noctalia msg --help`. Everything
//! Noctalia-specific is offered only when the `noctalia` binary is installed.

use std::sync::LazyLock;

use anyhow::{Result, bail};
use hyprdeck_core::cmd;

/// `noctalia msg ` prefix of IPC command lines.
pub const PREFIX: &str = "noctalia msg ";

static INSTALLED: LazyLock<bool> = LazyLock::new(|| cmd::which("noctalia").is_some());

/// Whether the Noctalia shell is installed (a `$PATH` lookup, cached).
pub fn installed() -> bool {
    *INSTALLED
}

#[derive(Debug, Clone, PartialEq)]
pub struct NoctCmd {
    pub name: String,
    /// Argument synopsis, e.g. `<id> [context]`.
    pub args: String,
    pub desc: String,
}

impl NoctCmd {
    /// Takes at least one mandatory argument.
    pub fn needs_args(&self) -> bool {
        self.args.contains('<')
    }
}

/// Run `noctalia msg --help` and parse its command table (blocking).
pub fn commands() -> Result<Vec<NoctCmd>> {
    let out = cmd::output("noctalia", ["msg", "--help"])?;
    let list = parse_help(&out.stdout);
    if list.is_empty() {
        bail!(
            "noctalia msg --help listed no commands: {}",
            out.stderr.trim()
        );
    }
    Ok(list)
}

pub fn parse_help(text: &str) -> Vec<NoctCmd> {
    let mut out = Vec::new();
    let mut in_cmds = false;
    for line in text.lines() {
        if line.trim_end() == "Commands:" {
            in_cmds = true;
            continue;
        }
        if !in_cmds {
            continue;
        }
        if !line.starts_with("  ") {
            if !line.trim().is_empty() {
                break;
            }
            continue;
        }
        let line = line.trim();
        // Synopsis and description are separated by a run of 2+ spaces.
        let (synopsis, desc) = match line.find("  ") {
            Some(i) => (&line[..i], line[i..].trim()),
            None => (line, ""),
        };
        let (name, args) = synopsis
            .split_once(' ')
            .map_or((synopsis, ""), |(n, a)| (n, a.trim()));
        out.push(NoctCmd {
            name: name.to_owned(),
            args: args.to_owned(),
            desc: desc.to_owned(),
        });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_command_table() {
        let help = "Usage: noctalia msg <command>\n\nCommands:\n  \
            annotate [path]                 Draw on the screen\n  \
            bluetooth-toggle                Toggle Bluetooth\n  \
            panel-toggle <id> [context]     Toggle a panel by id, optionally with context (e.g. launcher /emo)\n\n\
            Options:\n  -h, --help  Show this help message\n";
        let c = parse_help(help);
        assert_eq!(c.len(), 3);
        assert_eq!(
            c[0],
            NoctCmd {
                name: "annotate".into(),
                args: "[path]".into(),
                desc: "Draw on the screen".into()
            }
        );
        assert_eq!(c[1].args, "");
        assert!(c[2].needs_args());
        assert!(!c[0].needs_args());
        assert!(c[2].desc.starts_with("Toggle a panel"));
    }
}
