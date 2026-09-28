//! Field extraction: raw message bytes to a `messages` row.

use chrono::{DateTime, Datelike, Duration, NaiveDate, TimeZone, Utc};
use mail_parser::{HeaderValue, Message, MessageParser, MessagePart, MimeHeaders, PartType};
use sha2::{Digest, Sha256};

/// Wrap width for HTML rendering: effectively "never wrap".
const HTML_WIDTH: usize = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BodySource {
    Plain,
    Html,
    None,
}

impl BodySource {
    pub fn as_str(self) -> &'static str {
        match self {
            BodySource::Plain => "plain",
            BodySource::Html => "html",
            BodySource::None => "none",
        }
    }
}

/// One `messages` row, minus the columns the database fills in.
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub sha256: [u8; 32],
    pub message_id: Option<String>,
    pub in_reply_to: Option<String>,
    pub date_raw: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub from_addr: Option<String>,
    pub from_name: Option<String>,
    pub to_addrs: Option<Vec<String>>,
    pub cc_addrs: Option<Vec<String>>,
    pub subject: Option<String>,
    pub list_id: Option<String>,
    pub list_unsubscribe: Option<String>,
    pub raw_headers: Vec<u8>,
    pub body_text: Option<String>,
    pub body_source: BodySource,
    pub size_bytes: i32,
    pub attachment_count: i16,
}

/// Why a message was stored with less than full extraction. Each counts as an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Problem {
    /// Not recognisable as a message: only `raw_headers` is filled in.
    Unparseable,
    /// The parser or renderer panicked: only `raw_headers` is filled in.
    Panicked,
    /// Headers extracted, but the HTML body could not be rendered (`body_source = 'none'`).
    HtmlRenderFailed,
}

impl Problem {
    pub fn as_str(self) -> &'static str {
        match self {
            Problem::Unparseable => "unparseable message",
            Problem::Panicked => "parser panicked",
            Problem::HtmlRenderFailed => "html body could not be rendered",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Extracted {
    pub row: MessageRow,
    pub problem: Option<Problem>,
}

pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}

/// The header block: from the start up to (not including) the first blank line, byte-exact.
/// The whole input if there is no blank line.
pub fn raw_headers(bytes: &[u8]) -> &[u8] {
    let mut line_start = 0;
    while line_start < bytes.len() {
        let rest = &bytes[line_start..];
        if rest.starts_with(b"\n") || rest.starts_with(b"\r\n") {
            return &bytes[..line_start];
        }
        match rest.iter().position(|&b| b == b'\n') {
            Some(i) => line_start += i + 1,
            None => break,
        }
    }
    bytes
}

/// Extract a row from raw bytes. Never panics: a panic in the parser or renderer yields a
/// raw-headers-only row with [`Problem::Panicked`].
pub fn extract(bytes: &[u8]) -> Extracted {
    let size_bytes = i32::try_from(bytes.len()).unwrap_or(i32::MAX);
    let sha = sha256(bytes);
    match std::panic::catch_unwind(|| extract_inner(bytes, sha, size_bytes)) {
        Ok(e) => e,
        Err(_) => Extracted {
            row: bare_row(bytes, sha, size_bytes),
            problem: Some(Problem::Panicked),
        },
    }
}

fn bare_row(bytes: &[u8], sha256: [u8; 32], size_bytes: i32) -> MessageRow {
    MessageRow {
        sha256,
        message_id: None,
        in_reply_to: None,
        date_raw: None,
        sent_at: None,
        from_addr: None,
        from_name: None,
        to_addrs: None,
        cc_addrs: None,
        subject: None,
        list_id: None,
        list_unsubscribe: None,
        raw_headers: raw_headers(bytes).to_vec(),
        body_text: None,
        body_source: BodySource::None,
        size_bytes,
        attachment_count: 0,
    }
}

fn extract_inner(bytes: &[u8], sha: [u8; 32], size_bytes: i32) -> Extracted {
    let mut row = bare_row(bytes, sha, size_bytes);
    let Some(msg) = MessageParser::default()
        .parse(bytes)
        .filter(has_real_header)
    else {
        return Extracted {
            row,
            problem: Some(Problem::Unparseable),
        };
    };

    row.message_id = msg.message_id().and_then(normalize_id);
    row.in_reply_to = match msg.in_reply_to() {
        HeaderValue::Text(t) => normalize_id(t),
        HeaderValue::TextList(l) => l.first().and_then(|t| normalize_id(t)),
        _ => None,
    };
    row.date_raw = header_text(&msg, "Date");
    row.sent_at = sent_at(&msg, row.date_raw.as_deref());
    if let Some(addr) = msg.from().and_then(|a| a.first()) {
        row.from_addr = addr.address.as_deref().and_then(clean);
        row.from_name = addr.name.as_deref().and_then(clean);
    }
    row.to_addrs = msg.to().map(|a| {
        a.iter()
            .filter_map(|x| x.address.as_deref().and_then(clean))
            .collect()
    });
    row.cc_addrs = msg.cc().map(|a| {
        a.iter()
            .filter_map(|x| x.address.as_deref().and_then(clean))
            .collect()
    });
    row.subject = msg.subject().map(strip_nul);
    row.list_id = header_text(&msg, "List-Id");
    row.list_unsubscribe = header_text(&msg, "List-Unsubscribe");
    row.attachment_count = i16::try_from(msg.attachment_count()).unwrap_or(i16::MAX);

    let mut problem = None;
    let body_parts = |ids: &[u32]| -> Vec<&MessagePart<'_>> {
        ids.iter()
            .filter_map(|&i| msg.parts.get(i as usize))
            .collect()
    };
    if let Some(text) = body_parts(&msg.text_body).into_iter().find_map(plain_text) {
        row.body_text = Some(normalize_text(text));
        row.body_source = BodySource::Plain;
    } else if let Some(html) = body_parts(&msg.html_body).into_iter().find_map(html_text) {
        match html_to_text(html) {
            Some(text) => {
                row.body_text = Some(normalize_text(&text));
                row.body_source = BodySource::Html;
            }
            None => problem = Some(Problem::HtmlRenderFailed),
        }
    }
    // Nothing left after normalisation (e.g. an HTML body of spacers and images): no text.
    if row.body_text.as_deref() == Some("") {
        row.body_text = None;
        row.body_source = BodySource::None;
    }
    Extracted { row, problem }
}

/// mail-parser accepts almost anything; binary junk comes back with a "header" named after its
/// first line. A message needs at least one header with a valid RFC 5322 field name.
fn has_real_header(msg: &Message<'_>) -> bool {
    msg.headers().iter().any(|h| {
        let name = h.name.as_str();
        !name.is_empty() && name.bytes().all(|b| (33..=126).contains(&b) && b != b':')
    })
}

/// The decoded text of a body part whose MIME type is text/plain (or unspecified).
fn plain_text<'a>(part: &'a MessagePart<'_>) -> Option<&'a str> {
    let PartType::Text(text) = &part.body else {
        return None;
    };
    match part.content_type() {
        None => Some(text),
        Some(ct) => (ct.ctype().eq_ignore_ascii_case("text")
            && ct.subtype().is_none_or(|s| s.eq_ignore_ascii_case("plain")))
        .then_some(text),
    }
}

/// The decoded HTML of a body part whose MIME type is text/html.
fn html_text<'a>(part: &'a MessagePart<'_>) -> Option<&'a str> {
    let PartType::Html(html) = &part.body else {
        return None;
    };
    part.content_type()
        .is_some_and(|ct| {
            ct.ctype().eq_ignore_ascii_case("text")
                && ct.subtype().is_some_and(|s| s.eq_ignore_ascii_case("html"))
        })
        .then_some(html)
}

/// Render HTML to plain text: no wrapping, links as numbered footnotes, images as alt text
/// only, no decoration. `None` if html2text fails.
pub fn html_to_text(html: &str) -> Option<String> {
    html2text::config::with_decorator(html2text::render::TrivialDecorator::new())
        .link_footnotes(true)
        .no_table_borders()
        .allow_width_overflow()
        .string_from_read(html.as_bytes(), HTML_WIDTH)
        .ok()
}

/// Invisible format characters that marketing mail uses as padding (preheader filler, spacer
/// cells). They carry no text and are dropped.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\0' | '\u{00AD}' // soft hyphen
            | '\u{034F}' // combining grapheme joiner
            | '\u{180E}' // Mongolian vowel separator
            | '\u{200B}'
            ..='\u{200D}' // zero-width space, non-joiner, joiner
            | '\u{2060}' // word joiner
            | '\u{FEFF}' // byte-order mark / zero-width no-break space
    )
}

/// Normalise body text for search and classification (display always reopens the file):
/// drop NULs and invisible padding characters, collapse runs of horizontal whitespace
/// (including no-break spaces) to one space, trim each line, collapse runs of blank lines to a
/// single blank line, and trim blank lines at both ends.
pub fn normalize_text(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(1 << 20));
    let mut line = String::new();
    let mut blank_pending = false;
    for raw in s.split('\n') {
        line.clear();
        let mut space = false;
        for c in raw.chars() {
            if is_invisible(c) {
                continue;
            }
            if c.is_whitespace() {
                space = true;
                continue;
            }
            if space && !line.is_empty() {
                line.push(' ');
            }
            space = false;
            line.push(c);
        }
        if line.is_empty() {
            blank_pending = !out.is_empty();
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
            if blank_pending {
                out.push('\n');
            }
        }
        blank_pending = false;
        out.push_str(&line);
    }
    out
}

/// The first header with this name: raw bytes, lossily decoded, unfolded, trimmed.
fn header_text(msg: &Message<'_>, name: &str) -> Option<String> {
    let h = msg
        .headers()
        .iter()
        .find(|h| h.name.as_str().eq_ignore_ascii_case(name))?;
    let raw = msg
        .raw_message()
        .get(h.offset_start as usize..h.offset_end as usize)?;
    let text: String = String::from_utf8_lossy(raw)
        .chars()
        .filter(|&c| c != '\r' && c != '\n' && c != '\0')
        .collect();
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_string())
}

/// Parsed Date, as UTC. A date without a zone is taken as UTC. `None` if missing, unparseable
/// or outside 1971–2100.
fn sent_at(msg: &Message<'_>, date_raw: Option<&str>) -> Option<DateTime<Utc>> {
    let parsed = msg.date().cloned().or_else(|| {
        // mail-parser rejects a date with no zone; retry with an explicit UTC zone.
        mail_parser::DateTime::parse_rfc822(&format!("{} +0000", date_raw?))
    })?;
    let utc = to_utc(&parsed)?;
    (1971..=2100).contains(&utc.year()).then_some(utc)
}

fn to_utc(d: &mail_parser::DateTime) -> Option<DateTime<Utc>> {
    let local = NaiveDate::from_ymd_opt(d.year.into(), d.month.into(), d.day.into())?.and_hms_opt(
        d.hour.into(),
        d.minute.into(),
        d.second.into(),
    )?;
    let offset = i64::from(d.tz_hour) * 3600 + i64::from(d.tz_minute) * 60;
    let offset = if d.tz_before_gmt { -offset } else { offset };
    let utc = local.checked_sub_signed(Duration::seconds(offset))?;
    Some(Utc.from_utc_datetime(&utc))
}

/// Message-ID normalisation: trim, drop surrounding `<>`, NUL-strip; empty becomes `None`.
fn normalize_id(s: &str) -> Option<String> {
    let s = s.trim();
    let s = s.strip_prefix('<').unwrap_or(s);
    let s = s.strip_suffix('>').unwrap_or(s);
    clean(s.trim())
}

fn clean(s: &str) -> Option<String> {
    let s = strip_nul(s);
    (!s.trim().is_empty()).then_some(s)
}

pub fn strip_nul(s: &str) -> String {
    s.replace('\0', "")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_block_boundaries() {
        assert_eq!(
            raw_headers(b"A: b\r\nC: d\r\n\r\nbody"),
            b"A: b\r\nC: d\r\n"
        );
        assert_eq!(raw_headers(b"A: b\n\nbody\n\nmore"), b"A: b\n");
        assert_eq!(raw_headers(b"A: b\nno blank line"), b"A: b\nno blank line");
        assert_eq!(raw_headers(b"\r\nbody"), b"");
        assert_eq!(raw_headers(b""), b"");
    }

    #[test]
    fn ids() {
        assert_eq!(
            normalize_id("  <a@example.org> ").as_deref(),
            Some("a@example.org")
        );
        assert_eq!(
            normalize_id("a@example.org").as_deref(),
            Some("a@example.org")
        );
        assert_eq!(normalize_id("<>"), None);
        assert_eq!(normalize_id("  "), None);
    }

    #[test]
    fn html_rendering() {
        let t = html_to_text(
            "<html><head><style>p{color:red}</style></head><body><h1>Hi</h1><p><b>x</b> <a href=\"https://example.org/a\">link</a></p><img src=\"t.gif\"><img src=\"l.png\" alt=\"Logo\"></body></html>",
        )
        .unwrap();
        assert!(t.contains("link[1]"), "{t}");
        assert!(t.contains("[1]: https://example.org/a"), "{t}");
        assert!(t.contains("Logo"), "{t}");
        assert!(
            !t.contains('<') && !t.contains("**") && !t.contains("# "),
            "{t}"
        );
        assert!(!t.contains("color:red"), "{t}");
        let long = format!("<p>{}</p>", "word ".repeat(1000));
        assert_eq!(html_to_text(&long).unwrap().trim().lines().count(), 1);
    }

    #[test]
    fn normalization() {
        assert_eq!(
            normalize_text("\n\n  a   b\u{00A0}\u{00A0}c  \r\n\n\n\n \u{200C}\u{00A0} \n\td\n\n"),
            "a b c\n\nd"
        );
        assert_eq!(normalize_text("x\u{200B}y\u{FEFF}\u{034F}"), "xy");
        assert_eq!(normalize_text("one\ntwo\n\nthree"), "one\ntwo\n\nthree");
        assert_eq!(normalize_text("\u{200C}\u{00A0}\n \n\u{00AD}"), "");
        assert_eq!(normalize_text("nul\0byte"), "nulbyte");
    }

    #[test]
    fn marketing_layout_html_is_compact() {
        let spacer = "<tr><td height=\"20\" style=\"font-size:0\">&nbsp;</td></tr>".repeat(30);
        let pre = "&zwnj;&nbsp;".repeat(90);
        let html = format!(
            "<html><body><div style=\"display:none\">Sale ends Sunday{pre}</div>\
             <table width=\"600\"><tr><td><table>{spacer}<tr><td>Hello there, big sale.</td>\
             <td width=\"40\">&nbsp;</td><td>Shop <a href=\"https://example.org/shop\">now</a></td></tr>\
             {spacer}<tr><td>Second sentence here.</td></tr>{spacer}</table></td></tr></table></body></html>"
        );
        let raw = format!(
            "From: a@example.org\r\nSubject: s\r\nContent-Type: text/html; charset=utf-8\r\n\r\n{html}"
        );
        let e = extract(raw.as_bytes());
        assert_eq!(e.row.body_source, BodySource::Html);
        let t = e.row.body_text.unwrap();
        assert!(t.contains("Hello there, big sale."), "{t}");
        assert!(t.contains("Second sentence here."), "{t}");
        assert!(t.contains("https://example.org/shop"), "{t}");
        assert!(!t.contains("\n\n\n"), "{t:?}");
        assert!(!t.contains("  "), "{t:?}");
        assert!(!t.contains('\u{200C}') && !t.contains('\u{00A0}'), "{t:?}");
        assert!(t.len() < 300, "{} bytes: {t:?}", t.len());
    }

    #[test]
    fn html_of_only_spacers_has_no_text() {
        let raw = "From: a@example.org\r\nContent-Type: text/html\r\n\r\n<table><tr><td>&nbsp;</td></tr><tr><td>&zwnj;</td></tr></table>";
        let e = extract(raw.as_bytes());
        assert_eq!(e.row.body_source, BodySource::None);
        assert_eq!(e.row.body_text, None);
    }

    #[test]
    fn date_ranges() {
        let m = |d: &str| {
            let raw = format!("Date: {d}\r\nFrom: a@example.org\r\n\r\nx");
            extract(raw.as_bytes()).row.sent_at.map(|t| t.to_rfc3339())
        };
        assert_eq!(
            m("Mon, 2 Jan 2023 10:00:00 +0100").as_deref(),
            Some("2023-01-02T09:00:00+00:00")
        );
        assert_eq!(
            m("Mon, 2 Jan 2023 10:00:00 -0230").as_deref(),
            Some("2023-01-02T12:30:00+00:00")
        );
        assert_eq!(
            m("Mon, 2 Jan 2023 10:00:00").as_deref(),
            Some("2023-01-02T10:00:00+00:00")
        );
        assert_eq!(m("Mon, 2 Jan 1950 10:00:00 +0000"), None);
        assert_eq!(m("Mon, 2 Jan 2150 10:00:00 +0000"), None);
        assert_eq!(m("Tue, 31 Feb 2023 10:00:00 +0000"), None);
        assert_eq!(m("soon"), None);
    }

    #[test]
    fn garbage_is_unparseable() {
        for g in [
            &b""[..],
            b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR",
            b"%PDF-1.4 binary",
            b"just words\n",
        ] {
            let e = extract(g);
            assert_eq!(e.problem, Some(Problem::Unparseable), "{g:?}");
            assert_eq!(e.row.body_source, BodySource::None);
            assert_eq!(e.row.raw_headers, raw_headers(g));
        }
    }
}
