//! Lookup of any MIME type or URL scheme for the "Advanced" section.

use std::collections::BTreeSet;

use gtk::gio;
use gtk::gio::prelude::*;

use crate::categories;

/// Results shown at once.
pub const LIMIT: usize = 40;

#[derive(Debug, Clone)]
pub struct TypeInfo {
    pub mime: String,
    /// Lowercased description, for matching.
    pub description: String,
}

/// Every type in the shared-mime-info database plus every URL scheme an
/// installed app handles. Blocking.
pub fn all_types() -> Vec<TypeInfo> {
    let mut names: BTreeSet<String> = categories::registered().into_iter().collect();
    for app in gio::AppInfo::all() {
        names.extend(
            app.supported_types()
                .into_iter()
                .filter(|t| t.starts_with("x-scheme-handler/"))
                .map(Into::into),
        );
    }
    names
        .into_iter()
        .map(|mime| TypeInfo {
            description: categories::describe(&mime).to_lowercase(),
            mime,
        })
        .collect()
}

/// The type a query names exactly: `magnet:` → `x-scheme-handler/magnet`,
/// `text/x-foo` as written.
pub fn exact(query: &str) -> Option<String> {
    let q = query.trim().to_lowercase();
    let valid = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    };
    if let Some(scheme) = q.strip_suffix(':').or_else(|| q.strip_suffix("://")) {
        return valid(scheme).then(|| format!("x-scheme-handler/{scheme}"));
    }
    let (media, sub) = q.split_once('/')?;
    (valid(media)
        && !sub.is_empty()
        && sub
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "+-._".contains(c)))
    .then_some(q)
}

/// Matching types, best first, at most `limit`; also returns how many more matched.
pub fn search(types: &[TypeInfo], query: &str, limit: usize) -> (Vec<String>, usize) {
    let q = query.trim().to_lowercase();
    let q = q
        .strip_suffix("://")
        .or_else(|| q.strip_suffix(':'))
        .unwrap_or(&q)
        .to_owned();
    let mut ranked: Vec<(u8, &str)> = types
        .iter()
        .filter_map(|t| {
            let sub = t.mime.split_once('/').map_or("", |(_, s)| s);
            let rank = if t.mime == q || sub == q {
                0
            } else if t.mime.starts_with(&q) || sub.starts_with(&q) {
                1
            } else if t.mime.contains(&q) {
                2
            } else if t.description.contains(&q) {
                3
            } else {
                return None;
            };
            Some((rank, t.mime.as_str()))
        })
        .collect();
    ranked.sort();
    let mut out: Vec<String> = Vec::new();
    if let Some(e) = exact(query) {
        out.push(e);
    }
    for (_, m) in &ranked {
        if !out.iter().any(|o| o == m) {
            out.push((*m).to_owned());
        }
    }
    let more = out.len().saturating_sub(limit);
    out.truncate(limit);
    (out, more)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(mime: &str, description: &str) -> TypeInfo {
        TypeInfo {
            mime: mime.into(),
            description: description.to_lowercase(),
        }
    }

    #[test]
    fn exact_types_and_schemes() {
        assert_eq!(exact("magnet:").as_deref(), Some("x-scheme-handler/magnet"));
        assert_eq!(exact("https://").as_deref(), Some("x-scheme-handler/https"));
        assert_eq!(exact("Text/X-Foo").as_deref(), Some("text/x-foo"));
        assert_eq!(exact("markdown"), None);
        assert_eq!(exact("a b/c"), None);
    }

    #[test]
    fn ranks_matches() {
        let types = [
            t("text/markdown", "Markdown document"),
            t("text/x-markdown-extra", "Extra"),
            t("application/x-foo", "Contains markdown"),
            t("x-scheme-handler/magnet", "magnet: links"),
        ];
        let (r, more) = search(&types, "markdown", 10);
        assert_eq!(
            r,
            [
                "text/markdown",
                "text/x-markdown-extra",
                "application/x-foo"
            ]
        );
        assert_eq!(more, 0);
        let (r, _) = search(&types, "magnet:", 10);
        assert_eq!(r, ["x-scheme-handler/magnet"]);
        let (r, more) = search(&types, "markdown", 1);
        assert_eq!((r.len(), more), (1, 2));
    }
}
