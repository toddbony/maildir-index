//! Index Maildir trees (as written by mbsync/isync) into PostgreSQL.
//!
//! The Maildir is read-only to this crate. Nothing here logs message content: log lines carry
//! counts, durations, SHA-256 prefixes, folder names and file names only.

pub mod check;
pub mod config;
pub mod db;
pub mod extract;
pub mod links;
pub mod load;
pub mod par;
pub mod sample;
pub mod walk;

/// Written to `messages.loader_version` and `load_runs.loader_version`.
pub const LOADER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// The schema migrations, embedded at build time.
pub static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("./migrations");

/// Short hex prefix of a SHA-256, the only message identifier that may appear in logs.
pub fn sha_prefix(sha256: &[u8]) -> String {
    hex::encode(&sha256[..sha256.len().min(6)])
}
