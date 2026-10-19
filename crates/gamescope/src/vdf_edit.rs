//! Lossless edits to individual Valve KeyValues entries. No document reserialization.

use anyhow::{Result, bail, ensure};
use std::ops::Range;

const LIMIT: usize = 16 * 1024 * 1024;

#[derive(Debug)]
struct Entry {
    key: String,
    key_span: Range<usize>,
    value: Value,
    conditional: bool,
}

#[derive(Debug)]
struct Value {
    span: Range<usize>,
    kind: Kind,
}

#[derive(Debug)]
enum Kind {
    Text(String),
    Object(Vec<Entry>),
}

#[derive(Clone, Debug)]
enum TokenKind {
    Text(String),
    Open,
    Close,
    Condition,
}

#[derive(Clone, Debug)]
struct Token {
    span: Range<usize>,
    kind: TokenKind,
}

struct Document {
    root: Value,
    tokens: Vec<Token>,
}

/// Set one string, creating missing enclosing objects. Duplicate or conditional
/// keys on the edited path are rejected instead of choosing an effective value.
pub fn set_string(source: &str, path: &[&str], value: &str) -> Result<String> {
    validate_path(path)?;
    ensure!(!value.contains('\0'), "NUL is not valid VDF text");
    let document = parse(source)?;
    let mut object = &document.root;
    for (index, key) in path.iter().enumerate() {
        let entries = object_entries(object)?;
        match unique(entries, key)? {
            Some(entry) => {
                ensure!(!entry.conditional, "Cannot edit a conditional VDF key");
                if index + 1 == path.len() {
                    let Kind::Text(current) = &entry.value.kind else {
                        bail!("Expected a VDF string at the edited key");
                    };
                    if current == value {
                        return Ok(source.to_owned());
                    }
                    let mut output = source.to_owned();
                    output.replace_range(entry.value.span.clone(), &quoted(value));
                    ensure!(output.len() <= LIMIT, "VDF exceeds 16 MiB after editing");
                    return Ok(output);
                }
                object = &entry.value;
            }
            None => {
                let newline = if source.contains("\r\n") {
                    "\r\n"
                } else {
                    "\n"
                };
                let insertion = if std::ptr::eq(object, &document.root) {
                    source.len()
                } else {
                    object.span.end - 1
                };
                let indent = indentation(source, entries, insertion, index);
                let mut fragment = String::new();
                if insertion > 0 && !source[..insertion].ends_with('\n') {
                    fragment.push_str(newline);
                }
                append_missing(&mut fragment, &path[index..], value, &indent, newline);
                let mut output = source.to_owned();
                output.insert_str(insertion, &fragment);
                ensure!(output.len() <= LIMIT, "VDF exceeds 16 MiB after editing");
                return Ok(output);
            }
        }
    }
    unreachable!()
}

/// Remove exactly one key. Comments and whitespace, including comments within
/// the removed entry, are retained verbatim; unrelated tokens are never touched.
pub fn remove_key(source: &str, path: &[&str]) -> Result<String> {
    validate_path(path)?;
    let document = parse(source)?;
    let mut object = &document.root;
    for (index, key) in path.iter().enumerate() {
        let Some(entry) = unique(object_entries(object)?, key)? else {
            return Ok(source.to_owned());
        };
        ensure!(!entry.conditional, "Cannot edit a conditional VDF key");
        if index + 1 == path.len() {
            let start = entry.key_span.start;
            let end = entry.value.span.end;
            let mut output = source.to_owned();
            for token in document.tokens.iter().rev() {
                if token.span.start >= start && token.span.end <= end {
                    output.replace_range(token.span.clone(), "");
                }
            }
            return Ok(output);
        }
        object = &entry.value;
    }
    unreachable!()
}

pub(crate) fn contains_key(source: &str, path: &[&str]) -> Result<bool> {
    validate_path(path)?;
    let document = parse(source)?;
    let mut object = &document.root;
    for (index, key) in path.iter().enumerate() {
        let Some(entry) = unique(object_entries(object)?, key)? else {
            return Ok(false);
        };
        ensure!(!entry.conditional, "Cannot edit a conditional VDF key");
        if index + 1 == path.len() {
            return Ok(true);
        }
        object = &entry.value;
    }
    unreachable!()
}

/// Locate mapping objects without assuming user-local configuration nesting.
pub(crate) fn paths_to_key(source: &str, key: &str) -> Result<Vec<Vec<String>>> {
    fn visit(
        value: &Value,
        key: &str,
        prefix: &mut Vec<String>,
        out: &mut Vec<Vec<String>>,
    ) -> Result<()> {
        let Kind::Object(entries) = &value.kind else {
            return Ok(());
        };
        for entry in entries {
            prefix.push(entry.key.clone());
            if entry.key.eq_ignore_ascii_case(key) {
                ensure!(!entry.conditional, "Cannot edit a conditional VDF mapping");
                object_entries(&entry.value)?;
                out.push(prefix.clone());
            }
            visit(&entry.value, key, prefix, out)?;
            prefix.pop();
        }
        Ok(())
    }
    let document = parse(source)?;
    let mut paths = Vec::new();
    visit(&document.root, key, &mut Vec::new(), &mut paths)?;
    // Resolve every path, so duplicate ancestors cannot disguise a mapping.
    for path in &paths {
        let refs: Vec<_> = path.iter().map(String::as_str).collect();
        contains_key(source, &refs)?;
    }
    Ok(paths)
}

fn validate_path(path: &[&str]) -> Result<()> {
    ensure!(
        !path.is_empty() && path.len() <= 128,
        "Invalid VDF edit path"
    );
    ensure!(
        path.iter()
            .all(|key| !key.is_empty() && !key.contains('\0')),
        "Invalid VDF edit key"
    );
    Ok(())
}

fn object_entries(value: &Value) -> Result<&[Entry]> {
    match &value.kind {
        Kind::Object(entries) => Ok(entries),
        _ => bail!("Expected a VDF object on the edited path"),
    }
}

fn unique<'a>(entries: &'a [Entry], key: &str) -> Result<Option<&'a Entry>> {
    let mut matches = entries
        .iter()
        .filter(|entry| entry.key.eq_ignore_ascii_case(key));
    let found = matches.next();
    ensure!(
        matches.next().is_none(),
        "Duplicate VDF keys make the edit ambiguous"
    );
    Ok(found)
}

fn quoted(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

fn indentation(source: &str, entries: &[Entry], insertion: usize, depth: usize) -> String {
    if let Some(entry) = entries.first() {
        let line = source[..entry.key_span.start]
            .rfind('\n')
            .map_or(0, |at| at + 1);
        let indent = &source[line..entry.key_span.start];
        if indent.bytes().all(|byte| matches!(byte, b' ' | b'\t')) {
            return indent.to_owned();
        }
    }
    let line = source[..insertion].rfind('\n').map_or(0, |at| at + 1);
    let closing = &source[line..insertion];
    if closing.bytes().all(|byte| matches!(byte, b' ' | b'\t')) && depth > 0 {
        return format!("{closing}\t");
    }
    "\t".repeat(depth)
}

fn append_missing(output: &mut String, path: &[&str], value: &str, indent: &str, newline: &str) {
    output.push_str(indent);
    output.push_str(&quoted(path[0]));
    if path.len() == 1 {
        output.push('\t');
        output.push_str(&quoted(value));
        output.push_str(newline);
    } else {
        output.push_str(newline);
        output.push_str(indent);
        output.push('{');
        output.push_str(newline);
        append_missing(output, &path[1..], value, &format!("{indent}\t"), newline);
        output.push_str(indent);
        output.push('}');
        output.push_str(newline);
    }
}

fn parse(source: &str) -> Result<Document> {
    ensure!(source.len() <= LIMIT, "VDF exceeds 16 MiB");
    ensure!(!source.contains('\0'), "NUL is not valid VDF text");
    let mut tokens = Vec::new();
    let mut at = if source.starts_with('\u{feff}') { 3 } else { 0 };
    let bytes = source.as_bytes();
    while at < bytes.len() {
        let ch = source[at..].chars().next().unwrap();
        if ch.is_whitespace() {
            at += ch.len_utf8();
            continue;
        }
        if source[at..].starts_with("//") {
            at = source[at..]
                .find('\n')
                .map_or(bytes.len(), |end| at + end + 1);
            continue;
        }
        if source[at..].starts_with("/*") {
            let end = source[at + 2..]
                .find("*/")
                .ok_or_else(|| anyhow::anyhow!("Unterminated VDF comment"))?;
            at += end + 4;
            continue;
        }
        let start = at;
        let kind = match bytes[at] {
            b'{' => {
                at += 1;
                TokenKind::Open
            }
            b'}' => {
                at += 1;
                TokenKind::Close
            }
            b'[' => {
                let end = source[at + 1..]
                    .find(']')
                    .ok_or_else(|| anyhow::anyhow!("Unterminated VDF condition"))?;
                at += end + 2;
                TokenKind::Condition
            }
            b'"' => {
                at += 1;
                let mut text = String::new();
                loop {
                    let ch = source[at..]
                        .chars()
                        .next()
                        .ok_or_else(|| anyhow::anyhow!("Unterminated VDF string"))?;
                    at += ch.len_utf8();
                    match ch {
                        '"' => break,
                        '\\' => {
                            let escaped = source[at..]
                                .chars()
                                .next()
                                .ok_or_else(|| anyhow::anyhow!("Unterminated VDF escape"))?;
                            at += escaped.len_utf8();
                            match escaped {
                                '"' => text.push('"'),
                                '\\' => text.push('\\'),
                                'n' => text.push('\n'),
                                'r' => text.push('\r'),
                                't' => text.push('\t'),
                                ch => {
                                    text.push('\\');
                                    text.push(ch);
                                }
                            }
                        }
                        '\0' => bail!("NUL is not valid VDF text"),
                        ch => text.push(ch),
                    }
                }
                TokenKind::Text(text)
            }
            _ => {
                while at < bytes.len() {
                    let ch = source[at..].chars().next().unwrap();
                    if ch.is_whitespace() || matches!(ch, '{' | '}' | '[') {
                        break;
                    }
                    ensure!(ch != '\0', "NUL is not valid VDF text");
                    at += ch.len_utf8();
                }
                TokenKind::Text(source[start..at].to_owned())
            }
        };
        tokens.push(Token {
            span: start..at,
            kind,
        });
    }
    let mut cursor = 0;
    let entries = parse_object(&tokens, &mut cursor, 0, false)?;
    ensure!(cursor == tokens.len(), "Unexpected VDF closing brace");
    Ok(Document {
        root: Value {
            span: 0..source.len(),
            kind: Kind::Object(entries),
        },
        tokens,
    })
}

fn parse_object(
    tokens: &[Token],
    cursor: &mut usize,
    depth: usize,
    nested: bool,
) -> Result<Vec<Entry>> {
    ensure!(depth <= 128, "VDF nesting exceeds 128 levels");
    let mut entries = Vec::new();
    loop {
        let Some(token) = tokens.get(*cursor) else {
            ensure!(!nested, "Missing VDF closing brace");
            return Ok(entries);
        };
        if matches!(token.kind, TokenKind::Close) {
            ensure!(nested, "Unexpected VDF closing brace");
            *cursor += 1;
            return Ok(entries);
        }
        let TokenKind::Text(key) = &token.kind else {
            bail!("Expected VDF key")
        };
        ensure!(
            !key.starts_with('#'),
            "External VDF directives are unsupported"
        );
        let key_span = token.span.clone();
        let key = key.clone();
        *cursor += 1;
        let token = tokens
            .get(*cursor)
            .ok_or_else(|| anyhow::anyhow!("Missing VDF value"))?;
        *cursor += 1;
        let value = match &token.kind {
            TokenKind::Text(text) => Value {
                span: token.span.clone(),
                kind: Kind::Text(text.clone()),
            },
            TokenKind::Open => {
                let entries = parse_object(tokens, cursor, depth + 1, true)?;
                Value {
                    span: token.span.start..tokens[*cursor - 1].span.end,
                    kind: Kind::Object(entries),
                }
            }
            _ => bail!("Expected VDF value"),
        };
        let conditional = tokens
            .get(*cursor)
            .is_some_and(|token| matches!(token.kind, TokenKind::Condition));
        while tokens
            .get(*cursor)
            .is_some_and(|token| matches!(token.kind, TokenKind::Condition))
        {
            *cursor += 1;
        }
        entries.push(Entry {
            key,
            key_span,
            value,
            conditional,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edits_only_value_and_roundtrips_escaped_ids() {
        let source = "// before\n\"Root\" { /* between */ \"a\\\"b\\\\c\" \"old\" \"other\" \"untouched\" } // after\n";
        let updated = set_string(source, &["root", "a\"b\\c"], "new\"\\\n").unwrap();
        assert_eq!(updated, source.replace("\"old\"", "\"new\\\"\\\\\\n\""));
        assert_eq!(
            set_string(&updated, &["ROOT", "a\"b\\c"], "new\"\\\n").unwrap(),
            updated
        );
    }

    #[test]
    fn removal_keeps_every_comment_and_unrelated_byte() {
        let source = "// before\nroot { \"42\" /* key */ { name \"old\" // inner\n priority 250 } \"0\" { name default } } // end\n";
        let updated = remove_key(source, &["root", "42"]).unwrap();
        assert_eq!(
            updated,
            "// before\nroot {  /* key */    // inner\n    \"0\" { name default } } // end\n"
        );
        crate::steam::parse_vdf(&updated).unwrap();
    }

    #[test]
    fn creates_missing_objects_and_refuses_ambiguous_input() {
        let updated = set_string("// keep\r\n", &["root", "tools", "id", "name"], "Name").unwrap();
        assert!(updated.starts_with("// keep\r\n"));
        assert!(updated.contains("\r\n"));
        assert!(contains_key(&updated, &["ROOT", "tools", "id", "name"]).unwrap());
        for source in [
            "root { name one NAME two }",
            "root { name one } ROOT {}",
            "root { name old } [$LINUX]",
            "#include \"outside.vdf\"",
            "root { name",
            "root { name \"unterminated",
        ] {
            assert!(set_string(source, &["root", "name"], "new").is_err());
        }
        assert_eq!(
            set_string(
                "unrelated value [$LINUX]\nroot { name old }",
                &["root", "name"],
                "new"
            )
            .unwrap(),
            "unrelated value [$LINUX]\nroot { name \"new\" }"
        );
    }
}
