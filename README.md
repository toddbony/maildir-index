# maildir-index

Index Maildir trees (as written by [mbsync/isync](https://isync.sourceforge.io/)) into PostgreSQL
for analysis: parsed headers plus a searchable text rendering of each message body.

The Maildir stays the source of truth. The database holds what you query (who sent what, when,
from which mailing list, with what text) and a pointer back to every file, so a viewer can always
open the original message.

**Status:** schema v1 (`migrations/0001_init.sql`). Loader and viewer not yet written.

## Design in one paragraph

A message is identified by the SHA-256 of its raw bytes. Where it lives (account, folder, Maildir
filename) is a separate table, because Maildir names change when flags change and the same bytes
can reappear under a new name. The same email delivered to two accounts is two rows sharing a
`Message-ID`. Bodies come from `text/plain` when present, otherwise from `text/html` rendered to
text. The raw header block is kept byte-exact. See [`docs/schema.md`](docs/schema.md).

## Planned pieces

| Piece | What |
|---|---|
| `migrations/` | SQL migrations, applied by the loader at start (sqlx) |
| loader | Walks the Maildir, parses with `mail-parser`, renders HTML with `html2text`, upserts idempotently |
| viewer | `/m/<sha256>`: opens the original file and renders it sanitized (`ammonia`, strict CSP) |

## Configuration

Nothing site-specific lives in this repository. Connection settings come from the standard libpq
environment (`PGHOST`, `PGDATABASE`, `PGUSER`, `PGPASSFILE`, `PGSSLMODE`); the Maildir root and
the account list come from a config file kept outside the repo (an `example.toml` will document it).

## License

Licensed under either of [Apache License, Version 2.0](LICENSE-APACHE) or [MIT license](LICENSE-MIT)
at your option.
