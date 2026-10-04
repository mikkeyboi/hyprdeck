//! Arch Linux news (RSS feed), fetched with curl and cached in the state dir.

use anyhow::{Result, bail};
use hyprdeck_core::{cmd, store};
use serde::{Deserialize, Serialize};

use crate::github;

pub const FEED_URL: &str = "https://archlinux.org/feeds/news/";

const CACHE_TTL_SECS: i64 = 3600;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NewsItem {
    pub title: String,
    pub link: String,
    /// Unix seconds.
    pub published: i64,
    /// Plain text: tags stripped, entities decoded, paragraphs separated by a
    /// blank line, links kept as `text (url)` when the text differs from the url.
    pub summary: String,
}

pub struct News {
    pub items: Vec<NewsItem>,
    /// Set when only stale cached data could be served.
    pub warning: Option<String>,
}

/// Fetch the news feed, served from cache when younger than one hour.
/// Blocking (spawns curl).
pub fn fetch(now: i64) -> Result<News> {
    let path = store::state_dir().join("arch-news.json");
    let lookup = github::cached(&path, now, CACHE_TTL_SECS, || {
        let out = cmd::output(
            "curl",
            ["-sS", "-f", "--max-time", "20", "-A", "hyprdeck", FEED_URL],
        )?;
        if !out.ok() {
            bail!("fetching Arch news failed: {}", out.stderr.trim());
        }
        let items = parse_rss(&out.stdout);
        if items.is_empty() {
            bail!("Arch news feed contained no items");
        }
        Ok(items)
    })?;
    Ok(News {
        items: lookup.release,
        warning: lookup.warning,
    })
}

/// Items of an RSS 2.0 document in feed order; items without a parseable
/// `pubDate` are dropped.
pub fn parse_rss(xml: &str) -> Vec<NewsItem> {
    let mut items = Vec::new();
    let mut rest = xml;
    while let Some((block, after)) = element(rest, "item") {
        rest = after;
        let Some(published) = field(block, "pubDate").and_then(|d| parse_rfc2822(&d)) else {
            continue;
        };
        items.push(NewsItem {
            title: field(block, "title").unwrap_or_default().trim().to_owned(),
            link: field(block, "link").unwrap_or_default().trim().to_owned(),
            published,
            summary: html_to_text(&field(block, "description").unwrap_or_default()),
        });
    }
    items
}

/// Items published strictly after `since`, newest first.
pub fn unread(items: &[NewsItem], since: i64) -> Vec<NewsItem> {
    let mut out: Vec<NewsItem> = items
        .iter()
        .filter(|i| i.published > since)
        .cloned()
        .collect();
    out.sort_by_key(|i| std::cmp::Reverse(i.published));
    out
}

/// Inner text of the first `<tag>…</tag>` in `s` and the remainder after it.
fn element<'a>(s: &'a str, tag: &str) -> Option<(&'a str, &'a str)> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut from = 0;
    loop {
        let start = from + s[from..].find(&open)?;
        let after_name = start + open.len();
        // Reject longer tag names such as `<items>` for `<item>`.
        match s[after_name..].chars().next()? {
            '>' | ' ' | '\t' | '\n' | '\r' | '/' => {}
            _ => {
                from = after_name;
                continue;
            }
        }
        let gt = after_name + s[after_name..].find('>')?;
        if s[..gt].ends_with('/') {
            return Some(("", &s[gt + 1..]));
        }
        let body_start = gt + 1;
        let end = body_start + s[body_start..].find(&close)?;
        return Some((&s[body_start..end], &s[end + close.len()..]));
    }
}

/// Text content of `<tag>` within `block`: CDATA sections taken verbatim,
/// everything else entity-decoded.
fn field(block: &str, tag: &str) -> Option<String> {
    let (raw, _) = element(block, tag)?;
    let mut out = String::new();
    let mut rest = raw;
    while let Some(i) = rest.find("<![CDATA[") {
        out.push_str(&decode_entities(&rest[..i]));
        let inner = &rest[i + 9..];
        let end = inner.find("]]>").unwrap_or(inner.len());
        out.push_str(&inner[..end]);
        rest = inner.get(end + 3..).unwrap_or("");
    }
    out.push_str(&decode_entities(rest));
    Some(out)
}

/// Decode XML and common HTML entities; unknown ones are kept literally.
fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let decoded = rest[1..]
            .find(';')
            .filter(|&n| n > 0 && n <= 10)
            .and_then(|n| entity(&rest[1..1 + n]).map(|c| (c, n + 2)));
        match decoded {
            Some((c, len)) => {
                out.push(c);
                rest = &rest[len..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

fn entity(name: &str) -> Option<char> {
    if let Some(num) = name.strip_prefix('#') {
        let code = match num.strip_prefix(['x', 'X']) {
            Some(hex) => u32::from_str_radix(hex, 16).ok()?,
            None => num.parse().ok()?,
        };
        return char::from_u32(code);
    }
    Some(match name {
        "lt" => '<',
        "gt" => '>',
        "amp" => '&',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => '\u{a0}',
        "hellip" => '…',
        "mdash" => '—',
        "ndash" => '–',
        "rsquo" => '’',
        "lsquo" => '‘',
        "ldquo" => '“',
        "rdquo" => '”',
        "laquo" => '«',
        "raquo" => '»',
        "bull" => '•',
        "middot" => '·',
        "copy" => '©',
        "reg" => '®',
        "trade" => '™',
        "times" => '×',
        _ => return None,
    })
}

/// Pending separator before the next visible character.
#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Gap {
    None,
    Space,
    Line,
    Paragraph,
}

struct TextBuilder {
    out: String,
    gap: Gap,
}

impl TextBuilder {
    fn gap(&mut self, gap: Gap) {
        if gap > self.gap {
            self.gap = gap;
        }
    }

    fn text(&mut self, text: &str) {
        for c in text.chars() {
            if c.is_whitespace() {
                self.gap(Gap::Space);
                continue;
            }
            if !self.out.is_empty() {
                self.out.push_str(match self.gap {
                    Gap::None => "",
                    Gap::Space => " ",
                    Gap::Line => "\n",
                    Gap::Paragraph => "\n\n",
                });
            }
            self.gap = Gap::None;
            self.out.push(c);
        }
    }
}

/// Convert an HTML fragment to plain text (see [`NewsItem::summary`]).
pub fn html_to_text(html: &str) -> String {
    let mut b = TextBuilder {
        out: String::new(),
        gap: Gap::None,
    };
    // Open `<a>`: its href and the output length when it started.
    let mut link: Option<(String, usize)> = None;
    let mut rest = html;
    while !rest.is_empty() {
        let Some(lt) = rest.find('<') else {
            b.text(&decode_entities(rest));
            break;
        };
        b.text(&decode_entities(&rest[..lt]));
        let Some(gt) = rest[lt..].find('>') else {
            b.text(&decode_entities(&rest[lt..]));
            break;
        };
        let tag = &rest[lt + 1..lt + gt];
        rest = &rest[lt + gt + 1..];
        let closing = tag.starts_with('/');
        let name: String = tag
            .trim_start_matches('/')
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase())
            .collect();
        match name.as_str() {
            "p" | "div" | "ul" | "ol" | "pre" | "blockquote" | "table" | "h1" | "h2" | "h3"
            | "h4" | "h5" | "h6" | "hr" => b.gap(Gap::Paragraph),
            "br" | "tr" => b.gap(Gap::Line),
            "li" if !closing => {
                b.gap(Gap::Line);
                b.text("•");
                b.gap(Gap::Space);
            }
            "li" => b.gap(Gap::Line),
            "a" if !closing => {
                link = attr(tag, "href").map(|href| (decode_entities(&href), b.out.len()));
            }
            "a" => {
                if let Some((href, start)) = link.take() {
                    let text = b.out[start..].trim();
                    if !href.is_empty() && text != href {
                        if text.is_empty() {
                            b.text(&href);
                        } else {
                            b.gap(Gap::Space);
                            b.text(&format!("({href})"));
                        }
                    }
                }
            }
            "td" | "th" => b.gap(Gap::Space),
            _ => {}
        }
    }
    b.out
}

/// Value of attribute `name` in a tag's inner text (`a href="…"`).
fn attr(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    loop {
        let i = rest.find(name)?;
        let preceded = rest[..i].ends_with(|c: char| c.is_whitespace());
        rest = &rest[i + name.len()..];
        let after = rest.trim_start();
        if !preceded || !after.starts_with('=') {
            continue;
        }
        let value = after[1..].trim_start();
        return Some(match value.chars().next()? {
            q @ ('"' | '\'') => value[1..].split(q).next()?.to_owned(),
            _ => value
                .split(|c: char| c.is_whitespace() || c == '>')
                .next()?
                .to_owned(),
        });
    }
}

/// RFC 2822 date (`Tue, 22 Sep 2026 09:09:27 +0000`; weekday optional,
/// numeric or named zones) → Unix seconds.
pub fn parse_rfc2822(s: &str) -> Option<i64> {
    let mut parts = s.split_whitespace().peekable();
    if parts.peek()?.ends_with(',') {
        parts.next();
    }
    let day: i64 = parts.next()?.parse().ok()?;
    let month = match parts.next()?.to_ascii_lowercase().as_str() {
        "jan" => 1,
        "feb" => 2,
        "mar" => 3,
        "apr" => 4,
        "may" => 5,
        "jun" => 6,
        "jul" => 7,
        "aug" => 8,
        "sep" => 9,
        "oct" => 10,
        "nov" => 11,
        "dec" => 12,
        _ => return None,
    };
    let year: i64 = match parts.next()? {
        y if y.len() == 2 => {
            let y: i64 = y.parse().ok()?;
            if y < 50 { 2000 + y } else { 1900 + y }
        }
        y => y.parse().ok()?,
    };
    let mut time = parts.next()?.split(':');
    let hh: i64 = time.next()?.parse().ok()?;
    let mm: i64 = time.next()?.parse().ok()?;
    let ss: i64 = match time.next() {
        Some(s) => s.parse().ok()?,
        None => 0,
    };
    if time.next().is_some()
        || !(1..=31).contains(&day)
        || !(0..24).contains(&hh)
        || !(0..60).contains(&mm)
        || !(0..=60).contains(&ss)
    {
        return None;
    }
    let offset = match parts.next() {
        None => 0,
        Some(zone) => zone_offset(zone)?,
    };
    Some(days_from_civil(year, month, day) * 86_400 + hh * 3600 + mm * 60 + ss - offset)
}

/// Seconds east of UTC for an RFC 2822 zone.
fn zone_offset(zone: &str) -> Option<i64> {
    if let Some(sign) = zone.chars().next().filter(|c| matches!(c, '+' | '-')) {
        let digits = &zone[1..];
        if digits.len() != 4 || !digits.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        let n: i64 = digits.parse().ok()?;
        let secs = (n / 100) * 3600 + (n % 100) * 60;
        return Some(if sign == '-' { -secs } else { secs });
    }
    let hours = match zone.to_ascii_uppercase().as_str() {
        "GMT" | "UT" | "UTC" | "Z" => 0,
        "EST" | "CDT" => -5,
        "EDT" => -4,
        "CST" | "MDT" => -6,
        "MST" => -7,
        "PST" => -8,
        "PDT" => -7,
        _ => return None,
    };
    Some(hours * 3600)
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

/// `Some(label)` when Arch news applies to this system (Arch or a derivative).
pub fn applies() -> Option<String> {
    let arch = std::path::Path::new("/etc/arch-release").exists() || cmd::which("pacman").is_some();
    if !arch {
        return None;
    }
    let os_release = std::fs::read_to_string("/etc/os-release")
        .or_else(|_| std::fs::read_to_string("/usr/lib/os-release"))
        .unwrap_or_default();
    Some(source_label(&os_release))
}

/// Label for the news source given the contents of os-release.
fn source_label(os_release: &str) -> String {
    let value = |key: &str| {
        os_release.lines().find_map(|line| {
            let v = line.trim().strip_prefix(key)?.strip_prefix('=')?;
            let v = v.trim().trim_matches(|c| c == '"' || c == '\'');
            (!v.is_empty()).then(|| v.to_owned())
        })
    };
    let id = value("ID");
    if id.as_deref().is_none_or(|id| id == "arch") {
        return "Arch Linux news".to_owned();
    }
    let name = value("PRETTY_NAME")
        .or_else(|| value("NAME"))
        .or(id)
        .unwrap_or_default();
    format!("Arch Linux news ({name} is based on Arch)")
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../fixtures/arch-news.xml");

    #[test]
    fn parses_fixture_feed() {
        let items = parse_rss(FIXTURE);
        assert_eq!(items.len(), 3);
        assert_eq!(
            items[0].title,
            "Mkinitcpio >=42 requires manual intervention for TPM2-based unlocking of LUKS devices"
        );
        assert_eq!(
            items[1].title,
            "virtualbox-ext-vnc >= 7.2.12-2 requires manual intervention"
        );
        assert_eq!(items[2].title, "Active AUR malicious packages incident");
        assert_eq!(
            items[1].link,
            "https://archlinux.org/news/virtualbox-ext-vnc-7212-2-requires-manual-intervention/"
        );
        assert_eq!(
            items[0].published,
            parse_rfc2822("Tue, 22 Sep 2026 09:09:27 +0000").unwrap()
        );
        assert!(items[0].published > items[1].published);
        assert!(items[1].published > items[2].published);
        for item in &items {
            assert!(!item.summary.is_empty());
            for bad in ["<", "&lt;", "&gt;", "&amp;", "&quot;", "&#"] {
                assert!(!item.summary.contains(bad), "{bad} in {}", item.summary);
            }
        }
        assert!(
            items[0]
                .summary
                .contains("TPM2 (https://wiki.archlinux.org/title/Trusted_Platform_Module)")
        );
        assert!(items[0].summary.contains("\n\n"));
    }

    #[test]
    fn handles_cdata_and_missing_dates() {
        let xml = "<rss><channel><item><title><![CDATA[A & B]]></title><link>x</link>\
            <description><![CDATA[<p>hi</p>]]></description>\
            <pubDate>1 Jan 2026 00:00 GMT</pubDate></item>\
            <item><title>no date</title></item></channel></rss>";
        let items = parse_rss(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "A & B");
        assert_eq!(items[0].summary, "hi");
        assert_eq!(items[0].published, 1_767_225_600);
    }

    #[test]
    fn parses_rfc2822() {
        assert_eq!(
            parse_rfc2822("Tue, 22 Sep 2026 09:09:27 +0000"),
            Some(1_790_068_167)
        );
        assert_eq!(
            parse_rfc2822("22 Sep 2026 09:09:27 GMT"),
            Some(1_790_068_167)
        );
        assert_eq!(
            parse_rfc2822("Tue, 22 Sep 2026 04:09:27 -0500"),
            Some(1_790_068_167)
        );
        assert_eq!(
            parse_rfc2822("Tue, 22 Sep 2026 11:39:27 +0230"),
            Some(1_790_068_167)
        );
        assert_eq!(parse_rfc2822("Thu, 01 Jan 1970 00:00:00 EST"), Some(18_000));
        assert_eq!(parse_rfc2822("Tue, 22 Foo 2026 09:09:27 +0000"), None);
        assert_eq!(parse_rfc2822("Tue, 22 Sep 2026 25:09:27 +0000"), None);
        assert_eq!(parse_rfc2822("garbage"), None);
    }

    #[test]
    fn converts_html_to_text() {
        assert_eq!(
            html_to_text("<p>Run <code>pacman -Syu</code>  now.</p>\n<p>Second\n line</p>"),
            "Run pacman -Syu now.\n\nSecond line"
        );
        assert_eq!(
            html_to_text(r#"See <a href="https://wiki.archlinux.org/">the wiki</a>."#),
            "See the wiki (https://wiki.archlinux.org/)."
        );
        assert_eq!(
            html_to_text(r#"<a href="https://x.org/">https://x.org/</a>"#),
            "https://x.org/"
        );
        assert_eq!(
            html_to_text("<ul><li>one</li><li>two</li></ul><p>end</p>"),
            "• one\n• two\n\nend"
        );
        // XML layer decoded first, then HTML entities inside the markup.
        let xml = "<item><description>&lt;p&gt;a &amp;gt;= b&amp;nbsp;&amp;mdash; it&amp;rsquo;s &amp;#x41;&lt;/p&gt;</description></item>";
        let html = field(xml, "description").unwrap();
        assert_eq!(html, "<p>a &gt;= b&nbsp;&mdash; it&rsquo;s &#x41;</p>");
        assert_eq!(html_to_text(&html), "a >= b — it’s A");
        assert_eq!(html_to_text("a &lt;b&gt; &unknown; c"), "a <b> &unknown; c");
    }

    fn item(published: i64) -> NewsItem {
        NewsItem {
            title: published.to_string(),
            link: String::new(),
            published,
            summary: String::new(),
        }
    }

    #[test]
    fn unread_filters_strictly_after_since() {
        let items = [item(100), item(300), item(200)];
        let got: Vec<i64> = unread(&items, 100).iter().map(|i| i.published).collect();
        assert_eq!(got, [300, 200]);
        assert!(unread(&items, 300).is_empty());
    }

    #[test]
    fn labels_source() {
        let arch = "NAME=\"Arch Linux\"\nPRETTY_NAME=\"Arch Linux\"\nID=arch\n";
        assert_eq!(source_label(arch), "Arch Linux news");
        let cachy = "NAME=\"CachyOS Linux\"\nPRETTY_NAME=\"CachyOS\"\nID=cachyos\nID_LIKE=arch\n";
        assert_eq!(
            source_label(cachy),
            "Arch Linux news (CachyOS is based on Arch)"
        );
        assert_eq!(source_label(""), "Arch Linux news");
    }
}
