//! Parsers for the output of pacman/checkupdates/AUR helpers/cargo/curl.

use serde::{Deserialize, Serialize};

/// One pending package upgrade.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    pub name: String,
    pub old: String,
    pub new: String,
    pub important: bool,
}

/// One package entry from `pacman -Si`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncPkg {
    pub repo: String,
    pub name: String,
    pub version: String,
}

/// Packages whose updates commonly affect a Hyprland desktop (`*` = prefix match).
/// Kernels are handled separately by [`is_kernel`].
const IMPORTANT: &[&str] = &[
    "hyprland*",
    "aquamarine",
    "nvidia*",
    "mesa",
    "xdg-desktop-portal*",
    "pipewire*",
    "wireplumber",
];

/// Kernel and firmware packages (`linux`, `linux-zen`, `linux-lts-nvidia`,
/// `linux-firmware`…). Headers and docs only follow their kernel, so a kernel
/// update is flagged once rather than two or three times.
fn is_kernel(name: &str) -> bool {
    (name == "linux" || name.starts_with("linux-"))
        && !name.ends_with("-headers")
        && !name.ends_with("-docs")
}

/// Generic importance, independent of what is installed.
pub fn is_important(name: &str) -> bool {
    is_kernel(name)
        || IMPORTANT.iter().any(|p| match p.strip_suffix('*') {
            Some(prefix) => name.starts_with(prefix),
            None => name == *p,
        })
}

/// Parse `checkupdates` / `paru -Qua` / `yay -Qua` lines: `name old -> new [ignored]`.
/// Ignored packages (pacman `IgnorePkg`) are skipped like checkupdates does.
pub fn parse_updates(text: &str) -> Vec<Update> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let (name, old, arrow, new) = (it.next()?, it.next()?, it.next()?, it.next()?);
            if arrow != "->" || it.next().is_some_and(|t| t.starts_with('[')) {
                return None;
            }
            Some(Update {
                name: name.into(),
                old: old.into(),
                new: new.into(),
                important: is_important(name),
            })
        })
        .collect()
}

/// Parse `pacman -Q a b c` stdout (`name version` per line; errors go to stderr).
pub fn parse_query(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let (name, version) = line.trim().split_once(' ')?;
            Some((name.to_owned(), version.trim().to_owned()))
        })
        .collect()
}

/// Parse `LC_ALL=C pacman -Si …` stdout into entries in repository priority order.
pub fn parse_sync_info(text: &str) -> Vec<SyncPkg> {
    let mut out = Vec::new();
    let (mut repo, mut name, mut version) = (None, None, None);
    let mut flush =
        |repo: &mut Option<String>, name: &mut Option<String>, version: &mut Option<String>| {
            // All three are taken so a partial block never leaks into the next one.
            if let (Some(r), Some(n), Some(v)) = (repo.take(), name.take(), version.take()) {
                out.push(SyncPkg {
                    repo: r,
                    name: n,
                    version: v,
                });
            }
        };
    for line in text.lines() {
        if line.trim().is_empty() {
            flush(&mut repo, &mut name, &mut version);
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match key.trim_end() {
            "Repository" => repo = Some(value.to_owned()),
            "Name" => name = Some(value.to_owned()),
            "Version" => version = Some(value.to_owned()),
            _ => {}
        }
    }
    flush(&mut repo, &mut name, &mut version);
    out
}

/// A semver-compatible lockfile update reported by `cargo update --dry-run`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CrateUpdate {
    pub name: String,
    pub old: String,
    pub new: String,
}

/// Parse `cargo update --dry-run` stderr: `Updating name vOLD -> vNEW`.
pub fn parse_cargo_dry_run(text: &str) -> Vec<CrateUpdate> {
    text.lines()
        .filter_map(|line| {
            let rest = line.trim_start().strip_prefix("Updating ")?;
            let mut it = rest.split_whitespace();
            let (name, old, arrow, new) = (it.next()?, it.next()?, it.next()?, it.next()?);
            (arrow == "->").then(|| CrateUpdate {
                name: name.into(),
                old: old.trim_start_matches('v').into(),
                new: new.trim_start_matches('v').into(),
            })
        })
        .collect()
}

/// Parse an RFC 3339 UTC timestamp as GitHub returns it (`2026-10-02T01:41:07Z`)
/// into Unix seconds.
pub fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.strip_suffix('Z')?;
    let (date, time) = s.split_once('T')?;
    let mut d = date.splitn(3, '-').map(str::parse::<i64>);
    let (y, m, day) = (d.next()?.ok()?, d.next()?.ok()?, d.next()?.ok()?);
    let mut t = time
        .splitn(3, ':')
        .map(|p| p.split('.').next().unwrap_or(p).parse::<i64>());
    let (hh, mm, ss) = (t.next()?.ok()?, t.next()?.ok()?, t.next()?.ok()?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) {
        return None;
    }
    Some(days_from_civil(y, m, day) * 86_400 + hh * 3600 + mm * 60 + ss)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// A raw HTTP response as printed by `curl -i` (no redirects followed).
pub struct HttpResponse<'a> {
    pub status: u16,
    headers: &'a str,
    pub body: &'a str,
}

impl HttpResponse<'_> {
    /// Case-insensitive header lookup.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.lines().find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim().eq_ignore_ascii_case(name).then(|| v.trim())
        })
    }
}

pub fn parse_http_response(raw: &str) -> Option<HttpResponse<'_>> {
    let (head, body) = raw
        .split_once("\r\n\r\n")
        .or_else(|| raw.split_once("\n\n"))?;
    let (status_line, headers) = head.split_once('\n').unwrap_or((head, ""));
    let status = status_line.split_whitespace().nth(1)?.parse().ok()?;
    Some(HttpResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkupdates_lines() {
        let text = "hyprland 0.56.2-3 -> 0.56.2-4\nmesa 3:26.2.3-1 -> 3:26.2.4-1\nkitty 0.49.1-1 -> 0.49.2-1\n\n";
        let u = parse_updates(text);
        assert_eq!(u.len(), 3);
        assert_eq!(
            u[0],
            Update {
                name: "hyprland".into(),
                old: "0.56.2-3".into(),
                new: "0.56.2-4".into(),
                important: true
            }
        );
        assert_eq!(
            (u[1].old.as_str(), u[1].new.as_str(), u[1].important),
            ("3:26.2.3-1", "3:26.2.4-1", true)
        );
        assert!(!u[2].important);
    }

    #[test]
    fn aur_helper_lines_skip_ignored_and_noise() {
        let text = "example-app-bin 1.22.3-1 -> 1.23.0-1\n\
                    example-nightly-bin 0.0.45_nightly.20260930.2510-1 -> 0.0.46_nightly.20261003.2632-1\n\
                    linux-zen 7.2.8-1 -> 7.2.8-2 [ignored]\n\
                    :: Looking for devel upgrades...\n";
        let u = parse_updates(text);
        assert_eq!(
            u.iter().map(|u| u.name.as_str()).collect::<Vec<_>>(),
            ["example-app-bin", "example-nightly-bin"]
        );
        assert_eq!(u[1].new, "0.0.46_nightly.20261003.2632-1");
    }

    #[test]
    fn important_patterns() {
        for name in [
            "linux",
            "linux-zen",
            "linux-lts",
            "linux-firmware",
            "hyprland",
            "hyprland-git",
            "aquamarine",
            "nvidia-utils",
            "mesa",
            "pipewire-pulse",
            "wireplumber",
            "xdg-desktop-portal",
            "xdg-desktop-portal-hyprland",
        ] {
            assert!(is_important(name), "{name}");
        }
        for name in [
            "linux-headers",
            "linux-zen-headers",
            "linux-api-headers",
            "linux-docs",
            "linuxconsole",
            "lib32-mesa",
            "mesa-utils",
            "kitty",
            "noctalia",
        ] {
            assert!(!is_important(name), "{name}");
        }
    }

    #[test]
    fn pacman_query() {
        let q = parse_query("hyprland 0.56.2-3\nwaybar 0.15.0-3\n");
        assert_eq!(
            q,
            [
                ("hyprland".into(), "0.56.2-3".into()),
                ("waybar".into(), "0.15.0-3".into())
            ]
        );
        assert!(parse_query("").is_empty());
    }

    #[test]
    fn pacman_sync_info_blocks_in_priority_order() {
        let text = "Repository      : extra-testing\n\
                    Name            : waybar\n\
                    Version         : 0.15.1-1\n\
                    Description     : A sleek shell: with colons\n\
                    Conflicts With  : None\n\
                    \n\
                    Repository      : extra\n\
                    Name            : waybar\n\
                    Version         : 0.15.0-3\n\
                    \n\
                    Repository      : extra\n\
                    Name            : hyprland\n\
                    Version         : 0.56.2-4\n";
        let s = parse_sync_info(text);
        assert_eq!(s.len(), 3);
        assert_eq!(
            s[0],
            SyncPkg {
                repo: "extra-testing".into(),
                name: "waybar".into(),
                version: "0.15.1-1".into()
            }
        );
        assert_eq!(
            (s[1].repo.as_str(), s[1].version.as_str()),
            ("extra", "0.15.0-3")
        );
        assert_eq!(s[2].name, "hyprland");
    }

    #[test]
    fn cargo_dry_run() {
        let text = "    Updating crates.io index\n     Locking 2 packages to latest Rust 1.98.1 compatible versions\n    Updating anyhow v1.0.98 -> v1.0.99\n    Updating syn v2.0.100 -> v2.0.101\n      Adding foo v0.1.0\nwarning: not updating lockfile due to dry run\n";
        let u = parse_cargo_dry_run(text);
        assert_eq!(
            u,
            [
                CrateUpdate {
                    name: "anyhow".into(),
                    old: "1.0.98".into(),
                    new: "1.0.99".into()
                },
                CrateUpdate {
                    name: "syn".into(),
                    old: "2.0.100".into(),
                    new: "2.0.101".into()
                },
            ]
        );
    }

    #[test]
    fn iso8601() {
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_iso8601("2026-10-02T01:41:07Z"), Some(1_790_905_267));
        assert_eq!(parse_iso8601("2000-02-29T12:00:00Z"), Some(951_825_600));
        assert_eq!(parse_iso8601("2026-10-02"), None);
        assert_eq!(parse_iso8601("garbage"), None);
    }

    #[test]
    fn http_response() {
        let raw = "HTTP/2 403 \r\nx-ratelimit-remaining: 0\r\nX-RateLimit-Reset: 1791043455\r\n\r\n{\"message\":\"API rate limit exceeded\"}";
        let r = parse_http_response(raw).unwrap();
        assert_eq!(r.status, 403);
        assert_eq!(r.header("X-RateLimit-Remaining"), Some("0"));
        assert_eq!(r.header("x-ratelimit-reset"), Some("1791043455"));
        assert!(r.body.starts_with('{'));
    }
}
