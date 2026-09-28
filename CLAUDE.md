# CLAUDE.md: build brief for the maildir-index loader

You are building the **loader**: a Rust command-line program that indexes Maildir trees (as written
by mbsync/isync) into PostgreSQL. The schema already exists (`migrations/0001_init.sql`) and its
reasoning is in `docs/schema.md`. **Read both before writing code.** This file is the spec; where it
and your instincts disagree, follow this file and say so in your summary.

## Ground rules

- **This repository is public.** Never commit real mail, real names, real email addresses, host
  names, IP addresses or site paths. Test fixtures are hand-written and use `example.org` /
  `example.com`. Config examples use placeholders.
- **Never log message content.** No subject, address, display name, Message-ID or body text in any
  log line or error message, at any level. Log counts, durations, SHA-256 prefixes, folder names and
  file names only. (A test enforces this; see Acceptance.)
- The Maildir is **read-only** to this program. It never writes, renames or deletes anything there.
- Work on a branch named `loader`, in small commits with clear messages. Don't push; the owner pushes.
- **`migrations/0001_init.sql` is frozen once it has been applied anywhere.** Schema changes after
  that go in `0002_…`. (As of this brief it has not been applied, so fixing a real defect in it is
  allowed: say so explicitly in your summary if you do.)

## What exists

```
migrations/0001_init.sql   schema v1: accounts, folders, messages, locations, load_runs
docs/schema.md             why the schema is shaped this way (read it)
README.md, LICENSE-*       dual MIT OR Apache-2.0
```

## Crate

One crate, `maildir-index`, library + one binary of the same name. Suggested dependencies (check
current versions and APIs; adjust if a crate's API differs from what's assumed here):

- `mail-parser` (parsing), `html2text` (HTML to text), `sha2` + `hex`
- `sqlx` with `postgres`, `runtime-tokio`, `tls-rustls`, `migrate`, `chrono` (or `time`)
- `tokio`, `clap` (derive), `serde` + `toml`, `anyhow`, `tracing` + `tracing-subscriber`

Use **runtime queries** (`sqlx::query`, `query_as`), not the compile-time `query!` macros, so the
package builds without a database and without committed `.sqlx` data.

## Configuration

**Database: standard libpq environment only.** `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`,
`PGSSLMODE`, and the password **only** via `PGPASSFILE` (pgpass format). Never accept a password in
the config file, on the command line, or in a URL. Confirm sqlx's `PgConnectOptions` honours
`PGPASSFILE` and `PGSSLMODE=require`; if it doesn't, read the pgpass file yourself (format
`host:port:db:user:password`, `*` wildcards, `\:` escapes) and set the option explicitly. With
`PGSSLMODE=require` a non-TLS connection must fail, not silently downgrade.

**Everything else: one TOML file**, default `/etc/maildir-index/config.toml`, overridable with
`--config`:

```toml
maildir_root = "/srv/mail/maildir"     # one subdirectory per account

[[account]]
name = "personal"                       # directory name under maildir_root
address = "me@example.org"

[[account]]
name = "work"
address = "me@example.com"
```

On each run, upsert `accounts` by `name` (update `address` if it changed). An account directory that
is missing or empty is an **error for that account** (see "gone guard"), not "zero messages".

## Walking the Maildir

- Folders: every directory under `<root>/<account>/` that contains `cur/` or `new/`. The folder
  `path` is its path relative to the account directory (`INBOX`, `Archive/2019`, …); mbsync is
  configured with `SubFolders Verbatim`, so nested folders appear as nested directories.
- Messages: regular files directly inside `cur/` and `new/`. Skip `tmp/`, dotfiles
  (`.uidvalidity`, `.mbsyncstate*`, `.isyncuidmap.db`), symlinks and anything that isn't a file.
- Filename: `<base>:2,<flags>`, or just `<base>` (common in `new/`). `base_name` is everything before
  `:2,`; `flags` is what follows (possibly empty). mbsync bases look like
  `1700000000.12345_1.host,U=42`, but don't depend on that shape.
- A file can vanish between listing and reading (mbsync renames on flag changes). Treat
  `NotFound` as "skip, it will be seen next pass", not as an error.

## Per-file algorithm

Maildir message files are immutable: a flag change renames the file but never alters its bytes.
So a known `(folder, base_name)` doesn't need to be read again.

1. If `(folder_id, base_name)` is already in `locations`: update `subdir`, `flags`, `last_seen`,
   clear `gone_at`. **Don't read the file.** (With `--verify`, read and hash it anyway; if the hash
   differs from the stored `sha256`, log an error with both prefixes and count it.)
2. Otherwise read the bytes and compute SHA-256.
3. If that sha256 is not in `messages`, parse and insert (`ON CONFLICT (sha256) DO NOTHING`).
4. Insert the location `(folder_id, base_name, sha256, subdir, flags)`.

Batch writes (a transaction per few hundred files is fine). A crash mid-run must leave the database
consistent, and a re-run must finish the job: idempotency is the recovery mechanism.

**End of an account pass: gone marking.** Locations in that account's folders that were not seen in
this pass get `gone_at = now()` (only where `gone_at IS NULL`). **Gone guard:** if this pass saw fewer
than half of the account's currently present locations (`gone_at IS NULL`), don't mark anything gone:
log an error, record it in `load_runs.errors`, and exit non-zero. That protects the history from an
unmounted disk or a wrong path.

## Field extraction (`messages`)

| Column | Rule |
|---|---|
| `raw_headers` | Bytes from the start of the file up to (not including) the first blank line (`\r\n\r\n` or `\n\n`). Whole file if there is none. **Byte-exact, never decoded** |
| `message_id` | Parsed Message-ID, surrounding whitespace and `<>` removed. NULL if absent or empty |
| `in_reply_to` | First ID of In-Reply-To, same normalisation |
| `date_raw` | The Date header's text as written (unfolded, trimmed) |
| `sent_at` | Parsed Date. A date with no zone is taken as UTC (`date_raw` keeps the truth). NULL if missing, unparseable, or outside 1971–2100 |
| `from_addr`, `from_name` | First From mailbox: address as written, display name decoded (RFC 2047) |
| `to_addrs`, `cc_addrs` | Addresses only, in order, groups flattened |
| `subject` | Decoded (RFC 2047) |
| `list_id`, `list_unsubscribe` | Header text, unfolded and trimmed |
| `body_text`, `body_source` | See below |
| `size_bytes` | File length |
| `attachment_count` | Number of attachments the parser reports |
| `loader_version` | `env!("CARGO_PKG_VERSION")` |

Strip NUL (`\0`) from **every** text column (PostgreSQL `text` rejects it). Invalid UTF-8 in decoded
fields becomes U+FFFD; that's acceptable because `raw_headers` keeps the exact bytes.

**Body.** Decide the source by the **MIME type of the part actually chosen**, not by which parser
accessor returned it: `mail-parser` can hand back an HTML part (or its own HTML-to-text conversion)
from its "text body" accessors when no `text/plain` exists. **Don't use mail-parser's built-in
HTML-to-text.**

- A `text/plain` body part exists → `body_text` = its decoded text, `body_source = 'plain'`.
- Else a `text/html` body part exists → render with **`html2text`**: no line wrapping (a very large
  width), links as numbered footnotes with the URL list at the end, images contributing nothing
  (or only alt text), no decorations/markup. `body_source = 'html'`.
- Else → `body_text` NULL, `body_source = 'none'`.
- **Then normalise** (v0.1.1, both sources): drop NUL and invisible padding characters (soft hyphen,
  U+034F, U+180E, U+200B–U+200D, U+2060, U+FEFF); collapse runs of whitespace within a line (including
  no-break spaces) to one space and trim each line; collapse runs of blank lines to one; trim blank
  lines at both ends. If nothing is left, `body_text` NULL and `body_source = 'none'`. Found on real
  mail: layout-table newsletters rendered to hundreds of KB of spacer lines and padding.

If a message can't be parsed at all, still insert the row: `raw_headers` from the bytes,
`body_source = 'none'`, other fields NULL; count it as an error and log its sha256 prefix and path.
**One bad message never stops a run.**

## Commands

```
maildir-index migrate                     apply embedded migrations (sqlx::migrate!) and exit
maildir-index load [--account NAME]... [--verify]
                                          migrate, then load all (or the named) accounts
maildir-index check [--account NAME]...   compare files on disk vs present locations per account/folder;
                                          print differences; exit non-zero if any
maildir-index sample --n 20 [--account NAME] [--seed N] [--show]
                                          NO database: pick N random messages, print per message the
                                          sha256 prefix, body_source, text length, and with --show
                                          ONLY, the rendered body_text for human review
```

`load` writes one `load_runs` row per account (insert at start, update `finished_at`, `files_seen`,
`inserted` = new messages, `errors` at the end) and prints one summary line per account to stdout,
`key=value` style so it's easy to query in a log store, e.g.
`maildir-index account=personal files_seen=65802 new_messages=12 new_locations=12 gone=0 errors=0 duration_s=41`.
Exit non-zero if any account had errors or tripped the gone guard.

Performance target: a 2-core, 8 GB x86 box, ~250,000 messages / ~25 GB on first load. Stream files
(never hold all bodies in memory); modest parallelism for read + parse is welcome but optional. A
catch-up run with nothing new should only list directories and touch the database, not read files.

## Tests

Unit tests with hand-written fixtures in `tests/fixtures/` (fake `example.org` mail). Integration
tests against a **local** PostgreSQL 17, using `DATABASE_URL` (skip, with a clear message, if unset);
`#[sqlx::test]` with embedded migrations is fine. Never point tests at a shared or production
database. Required cases:

1. Plain-text message: all columns extracted; `body_source = 'plain'`.
2. HTML-only message: `body_source = 'html'`, text non-empty, no tags, link URLs appear as footnotes.
3. Multipart/alternative: the plain part wins.
4. No Message-ID → `message_id` NULL. Unbracketed Message-ID → stored without brackets.
5. Unparseable Date → `sent_at` NULL, `date_raw` kept. Date without zone → parsed as UTC.
6. NUL bytes in body and subject → stripped, row inserted.
7. Non-UTF-8 (8-bit) header bytes → `raw_headers` byte-identical to the file's header block.
8. Unparseable garbage file → row inserted with `body_source = 'none'`, counted as an error, run continues.
9. **Idempotency:** `load` twice on the same tree → second run: 0 new messages, 0 new locations.
10. **Flag rename:** `cur/X:2,S` → `cur/X:2,FS` → same location row, `flags` updated, no new message.
11. **Same bytes, new name** (server-side move / UIDVALIDITY reset) → one `messages` row, two
    locations, the old one `gone_at` set.
12. `new/` file without `:2,` → `flags = ''`, `subdir = 'new'`; later moved to `cur/X:2,S` → same location updated.
13. **Gone guard:** empty or missing account directory → nothing marked gone, error, non-zero exit.
14. Dotfiles, `tmp/`, symlinks ignored.
15. **Log hygiene:** run `load` over fixtures with distinctive subjects/addresses/Message-IDs,
    capture all log output, assert none of those strings appear.
16. `check` reports zero differences after a load, and non-zero after deleting a fixture file.
17. Layout-table HTML with spacer rows and `&zwnj;&nbsp;` preheader padding → compact text, no runs of
    blank lines or spaces, no invisible characters; an HTML body of only spacers → `body_source = 'none'`.

## Packaging (`cargo deb`)

Follow the pattern: the package installs things and registers units; it **enables nothing** and
**installs no config into `/etc`**.

- `/usr/bin/maildir-index`
- `lib/systemd/system/maildir-index-load.service`: `Type=oneshot`,
  `ExecStart=/usr/bin/maildir-index load`, `User=maildir-index`,
  `EnvironmentFile=-/etc/default/maildir-index`, hardening (`NoNewPrivileges=yes`,
  `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp=yes`). Output goes to the journal.
- `lib/systemd/system/maildir-index-load.timer`: e.g. `OnCalendar=*:0/15`, `Persistent=true`.
  Sites adjust schedule and user with a drop-in.
- `/usr/share/doc/maildir-index/`: `config.example.toml`, `default.example` (the `PG*` variables,
  `PGSSLMODE=require`, `PGPASSFILE=…`), and a short `README.deploy.md` (install with `dpkg -i`, copy
  the examples into place, run `migrate` then `load` by hand once, check with `check`, **then**
  enable the timer; how to override `User=` with a drop-in).
- `postinst`: create the system user/group `maildir-index` **only if absent**
  (`adduser --system --group --no-create-home …`); never modify an existing user.
  `prerm`: stop the timer and service.
- `depends = "$auto, adduser, systemd"`.

## Acceptance (what "done" means for this session)

- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo test` (with a local `DATABASE_URL`) all green.
- All 16 test cases above exist and pass.
- `cargo deb` builds; `dpkg-deb -c target/debian/*.deb` lists exactly the files above.
- `README.md` updated: status, the four commands, configuration via libpq environment + TOML.
- A short summary for the owner: what was built, any deviation from this brief and why, any schema
  concern, and anything you could not verify.

**Out of scope:** the viewer (`/m/<sha256>`), classification, any change to the Maildir, and any
deployment to a real host.
