-- maildir-index schema v1
--
-- Indexes Maildir trees (as written by mbsync/isync) into Postgres for analysis:
-- headers and a text rendering of the body. The Maildir stays the source of truth;
-- every row points back to its file through `locations`.
--
-- Identity: a message is its raw bytes (sha256). The same email delivered to two
-- accounts arrives with different delivery headers and so is two rows, joined by
-- message_id. Location is separate from identity because Maildir filenames change
-- (flag renames) and the same bytes can reappear under a new name (a server-side
-- move, or a UIDVALIDITY reset).
--
-- This file creates structure only. Account rows are configuration and are inserted
-- by the loader from its config file, never by a migration.

CREATE TABLE accounts (
    account_id  smallint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    name        text NOT NULL UNIQUE,
    address     text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
COMMENT ON TABLE  accounts      IS 'One per Maildir tree; name is the directory under the Maildir root.';
COMMENT ON COLUMN accounts.name IS 'Directory name under the Maildir root, e.g. "work".';

CREATE TABLE folders (
    folder_id   integer  GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id  smallint NOT NULL REFERENCES accounts,
    path        text NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now(),
    UNIQUE (account_id, path)
);
COMMENT ON COLUMN folders.path IS 'Folder path relative to the account directory, e.g. "INBOX" or "Archive/2019".';

CREATE TABLE messages (
    sha256            bytea PRIMARY KEY CHECK (octet_length(sha256) = 32),
    message_id        text,
    in_reply_to       text,
    date_raw          text,
    sent_at           timestamptz,
    from_addr         text,
    from_name         text,
    to_addrs          text[],
    cc_addrs          text[],
    subject           text,
    list_id           text,
    list_unsubscribe  text,
    raw_headers       bytea NOT NULL,
    body_text         text,
    body_source       text NOT NULL CHECK (body_source IN ('plain', 'html', 'none')),
    size_bytes        integer NOT NULL CHECK (size_bytes >= 0),
    attachment_count  smallint NOT NULL DEFAULT 0 CHECK (attachment_count >= 0),
    loaded_at         timestamptz NOT NULL DEFAULT now(),
    loader_version    text NOT NULL
);
COMMENT ON TABLE  messages                 IS 'One row per unique raw message file (sha256 of its exact bytes).';
COMMENT ON COLUMN messages.sha256          IS 'SHA-256 of the raw file bytes. Also the viewer key: /m/<hex>.';
COMMENT ON COLUMN messages.message_id      IS 'Message-ID with surrounding whitespace and angle brackets removed, so copies join across accounts. NULL when absent. Not unique: the same email in two accounts shares it.';
COMMENT ON COLUMN messages.date_raw        IS 'Date header as written; kept because some are unparseable or lack a zone.';
COMMENT ON COLUMN messages.sent_at         IS 'Parsed Date header. NULL when missing or unparseable.';
COMMENT ON COLUMN messages.raw_headers     IS 'The whole header block, byte-exact (not always valid UTF-8).';
COMMENT ON COLUMN messages.body_text       IS 'Text for search and classification, NUL bytes removed. From text/plain if present, else rendered from text/html.';
COMMENT ON COLUMN messages.body_source     IS 'Which part body_text came from: plain, html (rendered to text), or none.';
COMMENT ON COLUMN messages.loader_version  IS 'Loader version that wrote the row, so rows can be re-derived selectively.';

CREATE INDEX messages_message_id_idx ON messages (message_id);
CREATE INDEX messages_sent_at_idx    ON messages (sent_at);
CREATE INDEX messages_from_addr_idx  ON messages (lower(from_addr));
CREATE INDEX messages_list_id_idx    ON messages (list_id) WHERE list_id IS NOT NULL;

CREATE TABLE locations (
    folder_id   integer NOT NULL REFERENCES folders,
    base_name   text NOT NULL,
    sha256      bytea NOT NULL REFERENCES messages,
    subdir      text NOT NULL CHECK (subdir IN ('cur', 'new')),
    flags       text NOT NULL DEFAULT '',
    first_seen  timestamptz NOT NULL DEFAULT now(),
    last_seen   timestamptz NOT NULL DEFAULT now(),
    gone_at     timestamptz,
    PRIMARY KEY (folder_id, base_name)
);
COMMENT ON TABLE  locations           IS 'Where each message file has been seen. Rows are kept after the file disappears (gone_at).';
COMMENT ON COLUMN locations.base_name IS 'Maildir filename up to (not including) ":2,". Stable across flag changes.';
COMMENT ON COLUMN locations.subdir    IS 'cur or new, as last seen; with base_name and flags it rebuilds the path.';
COMMENT ON COLUMN locations.flags     IS 'Maildir info flags as last seen, e.g. "FS" (flagged, seen).';
COMMENT ON COLUMN locations.gone_at   IS 'Set when a full pass no longer finds the file; cleared if it reappears.';

CREATE INDEX locations_sha256_idx ON locations (sha256);

CREATE TABLE load_runs (
    run_id          integer GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    account_id      smallint REFERENCES accounts,
    started_at      timestamptz NOT NULL DEFAULT now(),
    finished_at     timestamptz,
    files_seen      integer,
    inserted        integer,
    errors          integer,
    loader_version  text NOT NULL
);
COMMENT ON TABLE load_runs IS 'One row per loader pass per account. finished_at NULL = the run did not complete.';
