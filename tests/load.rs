//! Load, check and log-hygiene tests against a local PostgreSQL (`DATABASE_URL`).
//! Each test creates and drops its own database.

mod common;

use chrono::{DateTime, Utc};
use common::*;
use maildir_index::load;
use sqlx::Row;

/// A tree holding every fixture in INBOX/cur, mbsync-style names.
fn full_tree() -> Tree {
    let t = Tree::new();
    for (i, f) in all_fixtures().iter().enumerate() {
        t.put("INBOX", "cur", &mbsync_name(i, "S"), f);
    }
    t
}

/// Several ordinary messages, so a single move or deletion does not trip the gone guard.
fn tree_with(n: usize) -> Tree {
    let t = Tree::new();
    let plain = [
        "plain.eml",
        "html-only.eml",
        "alternative.eml",
        "no-message-id.eml",
        "unbracketed-id.eml",
        "bad-date.eml",
        "no-zone-date.eml",
        "eightbit.eml",
    ];
    for i in 0..n {
        t.put("INBOX", "cur", &mbsync_name(i, "S"), plain[i % plain.len()]);
    }
    t
}

#[tokio::test]
async fn case01_plain_row_round_trip() {
    let Some(db) = test_db().await else { return };
    let t = Tree::new();
    t.put("INBOX", "cur", &mbsync_name(0, "S"), "plain.eml");
    let s = db.load(&t).await;
    assert_eq!((s.new_messages, s.new_locations, s.errors), (1, 1, 0));
    let r = sqlx::query("SELECT * FROM messages")
        .fetch_one(&db.pool)
        .await
        .unwrap();
    let bytes = fixture("plain.eml");
    assert_eq!(
        r.get::<Vec<u8>, _>("sha256"),
        maildir_index::extract::sha256(&bytes).to_vec()
    );
    assert_eq!(
        r.get::<Option<String>, _>("message_id").as_deref(),
        Some("plain-quokka-0001@example.org")
    );
    assert_eq!(
        r.get::<Option<String>, _>("in_reply_to").as_deref(),
        Some("parent-quokka-0000@example.org")
    );
    assert_eq!(
        r.get::<Option<DateTime<Utc>>, _>("sent_at")
            .unwrap()
            .to_rfc3339(),
        "2023-01-02T09:00:00+00:00"
    );
    assert_eq!(
        r.get::<Option<Vec<String>>, _>("to_addrs").unwrap(),
        [
            "bob.quokka@example.org",
            "carol.quokka@example.org",
            "dave.quokka@example.com"
        ]
    );
    assert_eq!(
        r.get::<Option<Vec<String>>, _>("cc_addrs").unwrap(),
        ["erin.quokka@example.com"]
    );
    assert_eq!(
        r.get::<Option<String>, _>("from_name").as_deref(),
        Some("Alice Quökka")
    );
    assert_eq!(r.get::<String, _>("body_source"), "plain");
    assert_eq!(r.get::<i32, _>("size_bytes") as usize, bytes.len());
    assert_eq!(
        r.get::<Vec<u8>, _>("raw_headers"),
        maildir_index::extract::raw_headers(&bytes)
    );
    assert_eq!(
        r.get::<String, _>("loader_version"),
        env!("CARGO_PKG_VERSION")
    );
    let run = sqlx::query(
        "SELECT finished_at IS NOT NULL AS done, files_seen, inserted, errors FROM load_runs",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert!(run.get::<bool, _>("done"));
    assert_eq!(
        (
            run.get::<i32, _>("files_seen"),
            run.get::<i32, _>("inserted"),
            run.get::<i32, _>("errors")
        ),
        (1, 1, 0)
    );
    db.drop_db().await;
}

#[tokio::test]
async fn case06_and_case08_nul_and_garbage_rows_inserted_run_continues() {
    let Some(db) = test_db().await else { return };
    let t = full_tree();
    let s = db.load(&t).await;
    let n = all_fixtures().len() as u64;
    assert_eq!(s.files_seen, n);
    assert_eq!(
        s.new_messages, n,
        "every file becomes a row, garbage included"
    );
    assert_eq!(s.new_locations, n);
    assert_eq!(s.errors, 1, "only the garbage file is an error");
    assert!(s.failed());
    let nul: (Option<String>, Option<String>) = sqlx::query_as(
        "SELECT subject, body_text FROM messages WHERE message_id = 'nul-quokka-0006@example.org'",
    )
    .fetch_one(&db.pool)
    .await
    .unwrap();
    assert_eq!(nul.0.as_deref(), Some("Quokka nul subject"));
    assert_eq!(nul.1.as_deref().map(str::trim_end), Some("Quokka nul body"));
    let garbage = maildir_index::extract::sha256(&fixture("garbage.eml"));
    let (source, headers): (String, Vec<u8>) =
        sqlx::query_as("SELECT body_source, raw_headers FROM messages WHERE sha256 = $1")
            .bind(&garbage[..])
            .fetch_one(&db.pool)
            .await
            .unwrap();
    assert_eq!(source, "none");
    assert_eq!(
        headers,
        maildir_index::extract::raw_headers(&fixture("garbage.eml"))
    );
    assert_eq!(db.scalar_i64("SELECT errors::int8 FROM load_runs").await, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn case09_idempotency() {
    let Some(db) = test_db().await else { return };
    let t = full_tree();
    let first = db.load(&t).await;
    assert!(first.new_messages > 0);
    let second = db.load(&t).await;
    assert_eq!(
        (second.new_messages, second.new_locations, second.gone),
        (0, 0, 0)
    );
    assert_eq!(
        second.errors, 0,
        "a known location is not re-read, so garbage is not re-reported"
    );
    assert_eq!(second.files_seen, first.files_seen);
    assert_eq!(
        db.scalar_i64("SELECT count(*) FROM locations WHERE gone_at IS NULL")
            .await,
        first.files_seen as i64
    );
    db.drop_db().await;
}

#[tokio::test]
async fn case10_flag_rename() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(4);
    t.put("INBOX", "cur", "X:2,S", "plain.eml");
    db.load(&t).await;
    t.rename(("INBOX", "cur", "X:2,S"), ("INBOX", "cur", "X:2,FS"));
    let s = db.load(&t).await;
    assert_eq!(
        (s.new_messages, s.new_locations, s.gone, s.errors),
        (0, 0, 0, 0)
    );
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT subdir, flags, gone_at IS NULL FROM locations WHERE base_name = 'X'",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(rows, [("cur".to_string(), "FS".to_string(), true)]);
    db.drop_db().await;
}

#[tokio::test]
async fn case11_same_bytes_new_name() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(4);
    t.put("INBOX", "cur", "OLD,U=99:2,S", "nul.eml");
    db.load(&t).await;
    // Server-side move to another folder (new UID, new base name), same bytes.
    t.rename(
        ("INBOX", "cur", "OLD,U=99:2,S"),
        ("Archive/2023", "cur", "NEW,U=1:2,S"),
    );
    let s = db.load(&t).await;
    assert_eq!(
        (s.new_messages, s.new_locations, s.gone, s.errors),
        (0, 1, 1, 0)
    );
    let sha = maildir_index::extract::sha256(&fixture("nul.eml"));
    let rows: Vec<(String, String, bool)> = sqlx::query_as(
        "SELECT f.path, l.base_name, l.gone_at IS NOT NULL FROM locations l JOIN folders f USING (folder_id)
         WHERE l.sha256 = $1 ORDER BY f.path",
    )
    .bind(&sha[..])
    .fetch_all(&db.pool)
    .await
    .unwrap();
    assert_eq!(
        rows,
        [
            ("Archive/2023".into(), "NEW,U=1".into(), false),
            ("INBOX".into(), "OLD,U=99".into(), true)
        ]
    );
    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages WHERE sha256 = $1")
        .bind(&sha[..])
        .fetch_one(&db.pool)
        .await
        .unwrap();
    assert_eq!(messages, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn case12_new_without_info_then_cur() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(4);
    t.put(
        "INBOX",
        "new",
        "1700000001.555_1.testhost,U=77",
        "alternative.eml",
    );
    let s = db.load(&t).await;
    assert_eq!(s.errors, 0);
    let q =
        "SELECT subdir, flags FROM locations WHERE base_name = '1700000001.555_1.testhost,U=77'";
    let row: (String, String) = sqlx::query_as(q).fetch_one(&db.pool).await.unwrap();
    assert_eq!(row, ("new".into(), "".into()));
    t.rename(
        ("INBOX", "new", "1700000001.555_1.testhost,U=77"),
        ("INBOX", "cur", "1700000001.555_1.testhost,U=77:2,S"),
    );
    let s = db.load(&t).await;
    assert_eq!((s.new_messages, s.new_locations, s.gone), (0, 0, 0));
    let rows: Vec<(String, String)> = sqlx::query_as(q).fetch_all(&db.pool).await.unwrap();
    assert_eq!(rows, [("cur".into(), "S".into())]);
    db.drop_db().await;
}

async fn gone_count(db: &TestDb) -> i64 {
    db.scalar_i64("SELECT count(*) FROM locations WHERE gone_at IS NOT NULL")
        .await
}

#[tokio::test]
async fn case13_gone_guard_missing_empty_and_mostly_missing() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(6);
    db.load(&t).await;

    // Missing account directory (unmounted disk, wrong path).
    let aside = t.dir.path().join("aside");
    std::fs::rename(t.account_dir(), &aside).unwrap();
    let s = db.load(&t).await;
    assert!(s.guard_tripped && s.errors > 0 && s.failed());
    assert_eq!(gone_count(&db).await, 0);

    // Empty account directory.
    std::fs::create_dir(t.account_dir()).unwrap();
    let s = db.load(&t).await;
    assert!(s.guard_tripped && s.failed());
    assert_eq!(gone_count(&db).await, 0);

    // Fewer than half of the present locations seen.
    std::fs::remove_dir(t.account_dir()).unwrap();
    std::fs::rename(&aside, t.account_dir()).unwrap();
    for i in 0..4 {
        std::fs::remove_file(t.path("INBOX", "cur", &mbsync_name(i, "S"))).unwrap();
    }
    let s = db.load(&t).await;
    assert!(s.guard_tripped && s.failed());
    assert_eq!(s.gone, 0);
    assert_eq!(gone_count(&db).await, 0);

    // Every trip is recorded in load_runs.errors.
    let errs: Vec<i32> = sqlx::query_scalar("SELECT errors FROM load_runs ORDER BY run_id")
        .fetch_all(&db.pool)
        .await
        .unwrap();
    assert_eq!(errs[0], 0);
    assert!(errs[1..].iter().all(|&e| e >= 1), "{errs:?}");

    // The binary exits non-zero.
    std::fs::remove_dir_all(t.account_dir()).unwrap();
    let out = db.run_binary(&t, &["load"], "info");
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stdout).contains("account=personal"));
    assert_eq!(gone_count(&db).await, 0);
    db.drop_db().await;
}

#[tokio::test]
async fn case14_dotfiles_tmp_symlinks_ignored() {
    let Some(db) = test_db().await else { return };
    let t = Tree::new();
    let inbox = t.folder("INBOX");
    t.put("INBOX", "cur", &mbsync_name(0, "S"), "plain.eml");
    std::fs::write(inbox.join(".uidvalidity"), "1\n2\n").unwrap();
    std::fs::write(inbox.join(".mbsyncstate"), "x").unwrap();
    std::fs::write(inbox.join(".mbsyncstate.journal"), "x").unwrap();
    std::fs::write(inbox.join(".isyncuidmap.db"), "x").unwrap();
    std::fs::write(inbox.join("cur/.hidden:2,S"), fixture("html-only.eml")).unwrap();
    std::fs::write(
        inbox.join("tmp/1700000009.1_1.testhost"),
        fixture("alternative.eml"),
    )
    .unwrap();
    std::fs::create_dir(inbox.join("cur/a-directory")).unwrap();
    std::os::unix::fs::symlink(fixture_path("nul.eml"), inbox.join("cur/link:2,S")).unwrap();
    std::os::unix::fs::symlink(fixture_path("nul.eml"), inbox.join("new/link2")).unwrap();
    // A symlinked folder is not followed.
    let elsewhere = t.dir.path().join("elsewhere");
    for s in ["cur", "new", "tmp"] {
        std::fs::create_dir_all(elsewhere.join(s)).unwrap();
    }
    std::fs::write(elsewhere.join("cur/x:2,S"), fixture("eightbit.eml")).unwrap();
    std::os::unix::fs::symlink(&elsewhere, t.account_dir().join("Linked")).unwrap();

    let s = db.load(&t).await;
    assert_eq!(
        (s.files_seen, s.new_messages, s.new_locations, s.errors),
        (1, 1, 1, 0)
    );
    assert_eq!(db.scalar_i64("SELECT count(*) FROM folders").await, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn case15_log_hygiene() {
    let Some(db) = test_db().await else { return };
    let t = full_tree();
    t.put(
        "Nested/Deeper",
        "new",
        "1700000002.1_1.testhost",
        "plain.eml",
    );
    let mut logs = String::new();
    let mut run = |args: &[&str]| {
        let out = db.run_binary(&t, args, "trace");
        logs.push_str(&String::from_utf8_lossy(&out.stderr));
        logs.push_str(&String::from_utf8_lossy(&out.stdout));
        out
    };
    let first = run(&["load"]);
    assert!(!first.status.success(), "the garbage fixture is an error");
    assert!(run(&["load", "--verify"]).status.success());
    // A changed file under a known name, found by --verify.
    std::fs::write(
        t.path("INBOX", "cur", &mbsync_name(0, "S")),
        fixture("nul.eml"),
    )
    .unwrap();
    assert!(!run(&["load", "--verify"]).status.success());
    run(&["check"]);
    run(&["sample", "--n", "50", "--seed", "7"]);

    assert!(logs.contains("unparseable message"), "the error is logged");
    assert!(logs.contains("differs from stored sha256"));
    assert!(logs.len() > 1000, "trace logging produced output");
    for needle in [
        "quokka", // every subject, address, display name and Message-ID in the fixtures
        "Quokka",
        "example.org",
        "example.com",
        "Qu\u{f6}kka",
        "teatime",
        "café",
        "cell one",
        "Hello Bob",
    ] {
        if let Some(line) = logs.lines().find(|l| l.contains(needle)) {
            let at = line.find(needle).unwrap();
            let lo = line.floor_char_boundary(at.saturating_sub(200));
            panic!(
                "log output contains {needle:?}: ...{}",
                &line[lo..(at + needle.len()).min(line.len())]
            );
        }
    }
    db.drop_db().await;
}

#[tokio::test]
async fn case16_check() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(5);
    t.put("INBOX", "new", "1700000003.1_1.testhost", "eightbit.eml");
    let s = db.load(&t).await;
    assert!(!s.failed());
    let out = db.run_binary(&t, &["check"], "warn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "{stdout}");
    assert!(stdout.contains("differences=0"), "{stdout}");

    std::fs::remove_file(t.path("INBOX", "cur", &mbsync_name(2, "S"))).unwrap();
    let out = db.run_binary(&t, &["check"], "warn");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(!out.status.success(), "{stdout}");
    assert!(stdout.contains("missing_on_disk=1"), "{stdout}");
    assert!(
        stdout.contains(&format!(
            "missing_on_disk INBOX/cur/{}",
            mbsync_name(2, "S")
        )),
        "{stdout}"
    );
    assert!(stdout.contains("differences=1"), "{stdout}");
    db.drop_db().await;
}

#[tokio::test]
async fn verify_detects_changed_bytes() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(3);
    db.load(&t).await;
    std::fs::write(
        t.path("INBOX", "cur", &mbsync_name(1, "S")),
        b"Subject: changed\n\nx",
    )
    .unwrap();
    let plain = db.load(&t).await;
    assert_eq!(plain.errors, 0, "without --verify known files are not read");
    let verified = db.load_opts(&t, load::Options { verify: true }).await;
    assert_eq!(verified.errors, 1);
    db.drop_db().await;
}

#[tokio::test]
async fn gone_location_reappears() {
    let Some(db) = test_db().await else { return };
    let t = tree_with(6);
    db.load(&t).await;
    let name = mbsync_name(0, "S");
    t.rename(("INBOX", "cur", &name), ("INBOX", "tmp", &name));
    assert_eq!(db.load(&t).await.gone, 1);
    t.rename(("INBOX", "tmp", &name), ("INBOX", "cur", &name));
    let s = db.load(&t).await;
    assert_eq!((s.gone, s.new_locations), (0, 0));
    assert_eq!(gone_count(&db).await, 0);
    db.drop_db().await;
}
