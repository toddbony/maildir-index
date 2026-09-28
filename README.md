# maildir-index

Index Maildir trees (as written by [mbsync/isync](https://isync.sourceforge.io/)) into PostgreSQL
for analysis: parsed headers plus a searchable text rendering of each message body.

The Maildir stays the source of truth. The database holds what you query (who sent what, when,
from which mailing list, with what text) and a pointer back to every file, so a viewer can always
open the original message.

**Status:** schema v1 (`migrations/0001_init.sql`) and the loader (this crate, 0.1.0). The viewer
is not yet written.

## Design in one paragraph

A message is identified by the SHA-256 of its raw bytes. Where it lives (account, folder, Maildir
filename) is a separate table, because Maildir names change when flags change and the same bytes
can reappear under a new name. The same email delivered to two accounts is two rows sharing a
`Message-ID`. Bodies come from `text/plain` when present, otherwise from `text/html` rendered to
text. The raw header block is kept byte-exact. See [`docs/schema.md`](docs/schema.md).

## Pieces

| Piece | What |
|---|---|
| `migrations/` | SQL migrations, applied by the loader at start (sqlx) |
| loader | **Done.** Walks the Maildir, parses with `mail-parser`, renders HTML with `html2text`, upserts idempotently |
| viewer | `/m/<sha256>`: opens the original file and renders it sanitized (`ammonia`, strict CSP) |

## Commands

```
maildir-index migrate                     apply the embedded migrations and exit
maildir-index load [--account NAME]... [--verify]
                                          migrate, then load all (or the named) accounts
maildir-index check [--account NAME]...   compare files on disk with indexed locations;
                                          exit non-zero on any difference
maildir-index sample --n 20 [--account NAME] [--seed N] [--show]
                                          no database: extract N random messages and print
                                          sha256 prefix, body source and text length
                                          (--show also prints the rendered text)
```

All commands take `--config PATH` (default `/etc/maildir-index/config.toml`).

`load` prints one line per account, e.g.
`maildir-index account=personal files_seen=65802 new_messages=12 new_locations=12 gone=0 errors=0 duration_s=41`,
and exits non-zero if any account had errors. A run with nothing new only lists directories
and updates the database; it does not read message files. Files that disappear are marked
`gone_at`, never deleted, unless the pass saw fewer than half of the known files (e.g. an
unmounted disk): then nothing is marked and the run fails.

Logs (stderr, `RUST_LOG`) never contain message content: only counts, durations, SHA-256
prefixes, folder names and file names.

## Configuration

Nothing site-specific lives in this repository.

**Database:** the standard libpq environment only: `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`,
`PGSSLMODE`, and the password **only** through `PGPASSFILE` (pgpass format, mode 0600).
`PGPASSWORD`, URLs and passwords in the config file are refused, and so is an invalid
`PGSSLMODE`. With `PGSSLMODE=require` a server without TLS is an error, never a silent downgrade.

**Everything else:** one TOML file (see [`example.toml`](example.toml)):

```toml
maildir_root = "/srv/mail/maildir"     # one subdirectory per account

[[account]]
name = "personal"                       # directory name under maildir_root
address = "me@example.org"
```

Folders are the directories under each account that contain `cur/` or `new/` (mbsync
`SubFolders Verbatim`). The Maildir is only ever read.

## Building, testing, packaging

```
cargo build --release
DATABASE_URL=postgres:///maildir_index_test?host=/var/run/postgresql cargo test
cargo deb            # target/debian/maildir-index_*.deb
```

Database tests create and drop their own throwaway database on the server named by
`DATABASE_URL` (the role needs `CREATEDB`); use a local, disposable server. Without
`DATABASE_URL` they are skipped with a message. The Debian package installs the binary, a
oneshot service and a 15-minute timer, and enables nothing; see
[`debian/README.deploy.md`](debian/README.deploy.md).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
