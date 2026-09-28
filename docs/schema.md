# Schema v1: design notes

These choices were made from a survey of a real ~250,000-message corpus from two providers,
not from assumptions. The numbers below are proportions from that survey.

## Identity is the raw bytes

`messages.sha256` is the SHA-256 of the file exactly as stored. In the survey **no two files were
byte-identical**, including across accounts: the same email delivered to two mailboxes carries
different delivery headers. So:

- one row per file, never "one row per email";
- "the same email" is a `message_id` question, answered by a join (about 1% of Message-IDs
  appeared in both accounts), not by a unique constraint;
- the loader inserts with `ON CONFLICT (sha256) DO NOTHING`, which makes every pass idempotent.

## Location is separate from identity

mbsync names files `<time>.<pid>_<n>.<host>,U=<uid>:2,<flags>`. The part before `:2,` is stable
while the message sits in one folder; the flags after it change when the message is read, flagged
or replied to. The same bytes reappear under a **new** name when a message is moved on the server
or a folder's UIDVALIDITY is reset. Hence `locations (folder_id, base_name) → sha256`, with the
latest `subdir`, `flags`, `first_seen` / `last_seen`, and `gone_at` instead of deletion, so a
viewer link never dangles silently.

## Headers

Header blocks are large (median ~8 KB: `Received`, `DKIM-Signature`, `ARC-*`), and not always valid
UTF-8. The whole block is kept as `bytea` for sender and authentication analysis; the fields most
queries need are extracted into columns. `List-Id` / `List-Unsubscribe` get their own columns
because about three quarters of the corpus was list mail.

Edge cases found and handled by the column types:

| Found | Handling |
|---|---|
| Messages with no `Message-ID` (rare) | `message_id` nullable |
| `Message-ID` without angle brackets, or over 255 characters | `text`; brackets and surrounding whitespace removed so copies join |
| Missing or unparseable `Date`; dates without a zone | `date_raw` kept; `sent_at` nullable |

## Body text

About **one message in five was HTML-only**. Storing only `text/plain` would leave those with no
text, so HTML is rendered to text (`html2text`: no line wrapping, links as footnotes, images
dropped), then whitespace-normalised: invisible padding characters removed, runs of spaces and of
blank lines collapsed. Newsletters built from layout tables otherwise render to hundreds of kilobytes
of spacer lines. `body_source` records `plain`, `html` or `none`, and `loader_version` records which
loader wrote the row, so rows can be re-rendered selectively if the renderer improves. NUL bytes
are stripped because PostgreSQL `text` cannot hold them. The rendering is for search and
classification only; display always goes back to the original file.

## Configuration is data, not schema

Migrations create structure only. Account names and addresses are inserted by the loader from its
config file, so nothing about a particular installation is ever committed.
