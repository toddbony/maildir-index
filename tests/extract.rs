//! Field extraction on hand-written fixtures (no database). Cases 1–8 of the brief.

use chrono::{TimeZone, Utc};
use maildir_index::extract::{BodySource, Problem, extract, raw_headers};

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

#[test]
fn case01_plain_text_all_columns() {
    let bytes = fixture("plain.eml");
    let e = extract(&bytes);
    assert_eq!(e.problem, None);
    let r = e.row;
    assert_eq!(r.sha256, maildir_index::extract::sha256(&bytes));
    assert_eq!(
        r.message_id.as_deref(),
        Some("plain-quokka-0001@example.org")
    );
    assert_eq!(
        r.in_reply_to.as_deref(),
        Some("parent-quokka-0000@example.org")
    );
    assert_eq!(
        r.date_raw.as_deref(),
        Some("Mon, 2 Jan 2023 10:00:00 +0100")
    );
    assert_eq!(
        r.sent_at,
        Some(Utc.with_ymd_and_hms(2023, 1, 2, 9, 0, 0).unwrap())
    );
    assert_eq!(r.from_addr.as_deref(), Some("alice.quokka@example.org"));
    assert_eq!(r.from_name.as_deref(), Some("Alice Quökka"));
    assert_eq!(
        r.to_addrs.as_deref(),
        Some(
            &[
                "bob.quokka@example.org",
                "carol.quokka@example.org",
                "dave.quokka@example.com"
            ]
            .map(String::from)[..]
        )
    );
    assert_eq!(
        r.cc_addrs.as_deref(),
        Some(&["erin.quokka@example.com".to_string()][..])
    );
    assert_eq!(r.subject.as_deref(), Some("Quokka plain subject é"));
    assert_eq!(
        r.list_id.as_deref(),
        Some("Quokka discussion <quokka.lists.example.org>")
    );
    assert_eq!(
        r.list_unsubscribe.as_deref(),
        Some("<mailto:leave-quokka@example.org>, <https://lists.example.org/quokka/leave>")
    );
    assert_eq!(r.body_source, BodySource::Plain);
    assert_eq!(
        r.body_text.as_deref(),
        Some("Hello Bob,\n\nThe quokka plain body says café.")
    );
    assert_eq!(r.size_bytes as usize, bytes.len());
    assert_eq!(r.attachment_count, 0);
    assert_eq!(r.raw_headers, raw_headers(&bytes));
    assert!(
        r.raw_headers
            .ends_with(b"Content-Transfer-Encoding: quoted-printable\r\n")
    );
}

#[test]
fn case02_html_only() {
    let r = extract(&fixture("html-only.eml")).row;
    assert_eq!(r.body_source, BodySource::Html);
    let text = r.body_text.unwrap();
    assert!(text.contains("Quokka weekly"), "{text}");
    assert!(text.contains("full article[1]"), "{text}");
    assert!(
        text.contains("[1]: https://example.org/quokka/article"),
        "{text}"
    );
    assert!(
        text.contains("[2]: https://example.com/quokka/archive"),
        "{text}"
    );
    assert!(
        text.contains("cell one") && text.contains("cell two"),
        "{text}"
    );
    for bad in ['<', '>', '*', '#', '─', '│'] {
        assert!(!text.contains(bad), "{bad:?} in {text}");
    }
    assert!(
        !text.contains("color: red") && !text.contains("pixel.gif"),
        "{text}"
    );
    // No wrapping: the paragraph with both links is one line.
    assert!(
        text.lines()
            .any(|l| l.contains("Read the full article[1]") && l.contains("archive[2]")),
        "{text}"
    );
}

#[test]
fn case03_alternative_plain_wins() {
    let r = extract(&fixture("alternative.eml")).row;
    assert_eq!(r.body_source, BodySource::Plain);
    assert_eq!(
        r.body_text.as_deref().map(str::trim_end),
        Some("Quokka plain version")
    );
    assert_eq!(r.attachment_count, 1);
    assert_eq!(
        r.sent_at,
        Some(Utc.with_ymd_and_hms(2023, 1, 4, 17, 0, 0).unwrap())
    );
}

#[test]
fn case04_message_id_absent_and_unbracketed() {
    assert_eq!(extract(&fixture("no-message-id.eml")).row.message_id, None);
    assert_eq!(
        extract(&fixture("unbracketed-id.eml"))
            .row
            .message_id
            .as_deref(),
        Some("unbracketed-quokka-0004@example.org")
    );
}

#[test]
fn case05_dates() {
    let bad = extract(&fixture("bad-date.eml")).row;
    assert_eq!(bad.sent_at, None);
    assert_eq!(bad.date_raw.as_deref(), Some("sometime around teatime"));
    let nozone = extract(&fixture("no-zone-date.eml")).row;
    assert_eq!(
        nozone.sent_at,
        Some(Utc.with_ymd_and_hms(2023, 1, 6, 14, 15, 16).unwrap())
    );
    assert_eq!(nozone.date_raw.as_deref(), Some("Fri, 6 Jan 2023 14:15:16"));
}

#[test]
fn case06_nul_stripped() {
    let r = extract(&fixture("nul.eml")).row;
    assert_eq!(r.subject.as_deref(), Some("Quokka nul subject"));
    assert_eq!(
        r.body_text.as_deref().map(str::trim_end),
        Some("Quokka nul body")
    );
    // raw_headers is bytea and keeps the NUL.
    assert!(r.raw_headers.contains(&0));
}

#[test]
fn case07_eight_bit_headers_byte_identical() {
    let bytes = fixture("eightbit.eml");
    let r = extract(&bytes).row;
    let end = bytes.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 2;
    assert_eq!(r.raw_headers, &bytes[..end]);
    assert!(r.raw_headers.contains(&0xe9) && r.raw_headers.contains(&0xff));
    assert_eq!(r.subject.as_deref(), Some("Quokka caf\u{fffd} 8-bit"));
    assert_eq!(r.from_addr.as_deref(), Some("andre.quokka@example.org"));
}

#[test]
fn case08_garbage_unparseable() {
    let bytes = fixture("garbage.eml");
    let e = extract(&bytes);
    assert_eq!(e.problem, Some(Problem::Unparseable));
    assert_eq!(e.row.body_source, BodySource::None);
    assert_eq!(e.row.raw_headers, raw_headers(&bytes));
    assert!(e.row.message_id.is_none() && e.row.subject.is_none() && e.row.body_text.is_none());
    assert_eq!(e.row.size_bytes as usize, bytes.len());
}
