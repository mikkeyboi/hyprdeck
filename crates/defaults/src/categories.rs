//! Default-app categories (web browser, image viewer, …): the MIME types each
//! one covers, its current state, and changing it.

use std::collections::{BTreeMap, HashSet};

use anyhow::{Result, bail};
use gtk::gio;

use crate::apps::{self, App};
use crate::{mimeapps, terminals};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// XDG MIME associations; the first type is the category's main type.
    Mime(&'static [&'static str]),
    /// `xdg-terminal-exec`'s preferred terminal.
    Terminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Category {
    /// CLI name (`hyprdeck defaults set browser …`).
    pub key: &'static str,
    pub label: &'static str,
    /// Symbolic icon shown when no app is set.
    pub icon: &'static str,
    pub kind: Kind,
}

/// Several names per format are listed because shared-mime-info renamed some
/// types over time; types the installed database doesn't know are skipped.
pub const CATEGORIES: &[Category] = &[
    Category {
        key: "browser",
        label: "Web browser",
        icon: "web-browser-symbolic",
        kind: Kind::Mime(&[
            "x-scheme-handler/http",
            "x-scheme-handler/https",
            "text/html",
            "application/xhtml+xml",
        ]),
    },
    Category {
        key: "email",
        label: "Email",
        icon: "mail-unread-symbolic",
        kind: Kind::Mime(&["x-scheme-handler/mailto"]),
    },
    Category {
        key: "files",
        label: "File manager",
        icon: "folder-symbolic",
        kind: Kind::Mime(&["inode/directory"]),
    },
    Category {
        key: "editor",
        label: "Text editor",
        icon: "text-x-generic-symbolic",
        kind: Kind::Mime(&[
            "text/plain",
            "text/markdown",
            "text/x-log",
            "application/json",
            "application/xml",
            "application/toml",
            "application/yaml",
            "application/x-yaml",
            "text/css",
            "text/javascript",
            "application/javascript",
            "text/x-python",
            "text/x-python3",
            "text/rust",
            "text/x-rust",
            "text/x-csrc",
            "text/x-chdr",
            "text/x-c++src",
            "text/x-c++hdr",
            "text/x-java",
            "text/x-go",
            "text/x-makefile",
            "text/x-cmake",
            "text/x-shellscript",
            "application/x-shellscript",
        ]),
    },
    Category {
        key: "images",
        label: "Image viewer",
        icon: "image-x-generic-symbolic",
        kind: Kind::Mime(&[
            "image/png",
            "image/jpeg",
            "image/gif",
            "image/webp",
            "image/bmp",
            "image/tiff",
            "image/svg+xml",
            "image/avif",
            "image/heif",
            "image/jxl",
            "image/vnd.microsoft.icon",
        ]),
    },
    Category {
        key: "video",
        label: "Video player",
        icon: "video-x-generic-symbolic",
        kind: Kind::Mime(&[
            "video/mp4",
            "video/matroska",
            "video/x-matroska",
            "video/webm",
            "video/quicktime",
            "video/x-msvideo",
            "video/vnd.avi",
            "video/mpeg",
            "video/ogg",
            "video/x-ogm+ogg",
            "video/x-flv",
            "video/3gpp",
        ]),
    },
    Category {
        key: "music",
        label: "Music player",
        icon: "audio-x-generic-symbolic",
        kind: Kind::Mime(&[
            "audio/mpeg",
            "audio/flac",
            "audio/ogg",
            "audio/x-vorbis+ogg",
            "audio/x-opus+ogg",
            "audio/vnd.wave",
            "audio/x-wav",
            "audio/mp4",
            "audio/aac",
            "audio/webm",
            "audio/x-aiff",
            "audio/x-ms-wma",
        ]),
    },
    Category {
        key: "documents",
        label: "PDF & documents",
        icon: "x-office-document-symbolic",
        kind: Kind::Mime(&[
            "application/pdf",
            "application/epub+zip",
            "application/postscript",
            "image/vnd.djvu",
        ]),
    },
    Category {
        key: "archives",
        label: "Archives",
        icon: "package-x-generic-symbolic",
        kind: Kind::Mime(&[
            "application/zip",
            "application/x-tar",
            "application/x-compressed-tar",
            "application/x-xz-compressed-tar",
            "application/x-bzip2-compressed-tar",
            "application/x-zstd-compressed-tar",
            "application/zstd",
            "application/gzip",
            "application/x-xz",
            "application/x-bzip2",
            "application/x-7z-compressed",
            "application/vnd.rar",
            "application/x-rar",
        ]),
    },
    Category {
        key: "terminal",
        label: "Terminal",
        icon: "utilities-terminal-symbolic",
        kind: Kind::Terminal,
    },
];

pub fn by_key(key: &str) -> Option<&'static Category> {
    CATEGORIES.iter().find(|c| c.key == key)
}

/// MIME types known to the shared-mime-info database. Blocking.
pub fn registered() -> HashSet<String> {
    gio::content_types_get_registered()
        .into_iter()
        .map(Into::into)
        .collect()
}

/// `types` the system knows (URL schemes are always kept).
pub fn known_types(types: &[&'static str], registered: &HashSet<String>) -> Vec<&'static str> {
    let known: Vec<&str> = types
        .iter()
        .copied()
        .filter(|t| t.starts_with("x-scheme-handler/") || registered.contains(*t))
        .collect();
    if known.is_empty() {
        types.to_vec()
    } else {
        known
    }
}

#[derive(Debug, Clone)]
pub struct TypeHandler {
    pub mime: String,
    /// Human-readable name ("PNG image").
    pub description: String,
    pub handler: Option<App>,
}

/// Handler of one type plus a readable description. Blocking.
pub fn type_handler(mime: &str) -> TypeHandler {
    TypeHandler {
        mime: mime.to_owned(),
        description: describe(mime),
        handler: apps::default_for(mime),
    }
}

/// "PNG image", or "https: links" for URL schemes.
pub fn describe(mime: &str) -> String {
    match mime.strip_prefix("x-scheme-handler/") {
        Some(scheme) => format!("{scheme}: links"),
        None => gio::content_type_get_description(mime).to_string(),
    }
}

#[derive(Debug, Clone)]
pub struct MimeState {
    pub types: Vec<TypeHandler>,
    /// Handler of the main type (or the most common handler when it has none).
    pub current: Option<App>,
    /// Apps declaring support for any of the types; most types first.
    pub candidates: Vec<App>,
    /// App id → the category's types it declares support for.
    pub support: BTreeMap<String, Vec<String>>,
}

impl MimeState {
    /// Types opened by another app although the current app supports them.
    pub fn fixable(&self) -> Vec<&str> {
        let Some(cur) = &self.current else {
            return Vec::new();
        };
        let supported = self
            .support
            .get(&cur.id)
            .map(Vec::as_slice)
            .unwrap_or_default();
        self.types
            .iter()
            .filter(|t| {
                t.handler.as_ref().map(|h| &h.id) != Some(&cur.id) && supported.contains(&t.mime)
            })
            .map(|t| t.mime.as_str())
            .collect()
    }

    /// Distinct handlers with the number of types each opens, most first.
    pub fn handlers(&self) -> Vec<(&App, usize)> {
        let mut out: Vec<(&App, usize)> = Vec::new();
        for h in self.types.iter().filter_map(|t| t.handler.as_ref()) {
            match out.iter_mut().find(|(a, _)| a.id == h.id) {
                Some((_, n)) => *n += 1,
                None => out.push((h, 1)),
            }
        }
        out.sort_by_key(|(_, n)| std::cmp::Reverse(*n));
        out
    }
}

/// Current handlers of `types`. Blocking.
pub fn mime_state(types: &[&str]) -> MimeState {
    let types_h: Vec<TypeHandler> = types.iter().map(|t| type_handler(t)).collect();
    let mut support: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut candidates: Vec<App> = Vec::new();
    for t in types {
        for app in apps::for_type(t) {
            support
                .entry(app.id.clone())
                .or_default()
                .push((*t).to_owned());
            if !candidates.iter().any(|c| c.id == app.id) {
                candidates.push(app);
            }
        }
    }
    candidates.sort_by_cached_key(|a| {
        (
            std::cmp::Reverse(support.get(&a.id).map_or(0, Vec::len)),
            a.name.to_lowercase(),
        )
    });
    let mut state = MimeState {
        current: None,
        types: types_h,
        candidates,
        support,
    };
    state.current = state
        .types
        .first()
        .and_then(|t| t.handler.clone())
        .or_else(|| state.handlers().first().map(|(a, _)| (*a).clone()));
    state
}

#[derive(Debug, Clone)]
pub enum State {
    Mime(MimeState),
    Terminal(terminals::TerminalState),
}

/// Blocking.
pub fn state(cat: &Category, registered: &HashSet<String>) -> State {
    match cat.kind {
        Kind::Mime(types) => State::Mime(mime_state(&known_types(types, registered))),
        Kind::Terminal => State::Terminal(terminals::state()),
    }
}

/// Make `app_id` the category's default for every type it supports (all
/// types when it declares none of them). Blocking.
pub fn set(cat: &Category, app_id: &str) -> Result<()> {
    let app_id = apps::desktop_id(app_id);
    if apps::find(&app_id).is_none() {
        bail!("{app_id} is not an installed application");
    }
    match cat.kind {
        Kind::Mime(types) => {
            let types = known_types(types, &registered());
            let st = mime_state(&types);
            let supported: Vec<&str> = st
                .support
                .get(&app_id)
                .map(|v| v.iter().map(String::as_str).collect())
                .unwrap_or_default();
            let targets = if supported.is_empty() {
                types
            } else {
                supported
            };
            mimeapps::write_defaults(&targets, &app_id)
        }
        Kind::Terminal => {
            if !terminals::launcher_installed() {
                bail!("{} is not installed", terminals::LAUNCHER);
            }
            terminals::set(&app_id)
        }
    }
}

/// Make the current app the handler of every type it supports. Blocking.
pub fn unify(cat: &Category) -> Result<()> {
    let Kind::Mime(types) = cat.kind else {
        return Ok(());
    };
    let st = mime_state(&known_types(types, &registered()));
    let Some(cur) = &st.current else {
        return Ok(());
    };
    mimeapps::write_defaults(&st.fixable(), &cur.id)
}

/// Drop the user's choice for the category. Blocking.
pub fn reset(cat: &Category) -> Result<()> {
    match cat.kind {
        Kind::Mime(types) => mimeapps::reset_defaults(types),
        Kind::Terminal => terminals::reset(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app(id: &str) -> App {
        App {
            id: id.into(),
            name: id.into(),
            icon: None,
            command: None,
            visible: true,
        }
    }

    fn th(mime: &str, handler: Option<&str>) -> TypeHandler {
        TypeHandler {
            mime: mime.into(),
            description: String::new(),
            handler: handler.map(app),
        }
    }

    #[test]
    fn mixed_only_counts_supported_types() {
        let st = MimeState {
            types: vec![
                th("image/png", Some("viewer.desktop")),
                th("image/jpeg", Some("other.desktop")),
                th("image/svg+xml", Some("vector.desktop")),
                th("image/jxl", None),
            ],
            current: Some(app("viewer.desktop")),
            candidates: vec![],
            support: BTreeMap::from([(
                "viewer.desktop".to_owned(),
                vec![
                    "image/png".to_owned(),
                    "image/jpeg".to_owned(),
                    "image/jxl".to_owned(),
                ],
            )]),
        };
        assert_eq!(st.fixable(), ["image/jpeg", "image/jxl"]);
        let h: Vec<(&str, usize)> = st
            .handlers()
            .iter()
            .map(|(a, n)| (a.id.as_str(), *n))
            .collect();
        assert_eq!(
            h,
            [
                ("viewer.desktop", 1),
                ("other.desktop", 1),
                ("vector.desktop", 1)
            ]
        );
    }

    #[test]
    fn filters_unknown_types() {
        let reg: HashSet<String> = ["image/png".to_owned()].into();
        assert_eq!(
            known_types(&["image/png", "image/x-nope"], &reg),
            ["image/png"]
        );
        assert_eq!(
            known_types(&["x-scheme-handler/http", "text/x-nope"], &reg),
            ["x-scheme-handler/http"]
        );
        assert_eq!(known_types(&["a/b"], &HashSet::new()), ["a/b"]);
        assert!(CATEGORIES.iter().all(|c| by_key(c.key) == Some(c)));
    }
}
