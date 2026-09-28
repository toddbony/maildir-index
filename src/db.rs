//! Database connection from the standard libpq environment only.
//!
//! `PGHOST`, `PGPORT`, `PGDATABASE`, `PGUSER`, `PGSSLMODE` and, for the password, `PGPASSFILE`
//! (pgpass format). A password is never accepted from anywhere else: not the config file, not
//! the command line, not a URL, and not `PGPASSWORD`.

use anyhow::{Context, Result, bail};
use sqlx::PgPool;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions, PgSslMode};

/// Connection options from the libpq environment variables.
pub fn connect_options() -> Result<PgConnectOptions> {
    connect_options_from(|k| std::env::var_os(k))
}

fn connect_options_from(
    var: impl Fn(&str) -> Option<std::ffi::OsString>,
) -> Result<PgConnectOptions> {
    if var("PGPASSWORD").is_some() {
        bail!("PGPASSWORD is set; the database password is accepted only via PGPASSFILE");
    }
    // sqlx silently falls back to `prefer` on an unrecognised PGSSLMODE; refuse instead, so a
    // typo cannot turn `require` into a possible plaintext connection.
    if let Some(mode) = var("PGSSLMODE") {
        let mode = mode.to_str().context("PGSSLMODE is not valid UTF-8")?;
        mode.parse::<PgSslMode>()
            .map_err(|_| anyhow::anyhow!("PGSSLMODE={mode:?} is not a valid sslmode"))?;
    }
    // `new()` reads PGPASSFILE, but also falls back to ~/.pgpass; only PGPASSFILE is allowed.
    let options = if var("PGPASSFILE").is_some() {
        PgConnectOptions::new()
    } else {
        PgConnectOptions::new_without_pgpass()
    };
    Ok(options.application_name("maildir-index"))
}

pub async fn connect() -> Result<PgPool> {
    let options = connect_options()?;
    PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .context("connecting to PostgreSQL (libpq environment: PGHOST, PGDATABASE, ...)")
}

pub async fn migrate(pool: &PgPool) -> Result<()> {
    crate::MIGRATOR
        .run(pool)
        .await
        .context("applying migrations")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<OsString> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        move |k| pairs.iter().find(|(n, _)| n == k).map(|(_, v)| v.into())
    }

    #[test]
    fn rejects_pgpassword() {
        let e = connect_options_from(env(&[("PGPASSWORD", "x")])).unwrap_err();
        assert!(e.to_string().contains("PGPASSFILE"));
    }

    #[test]
    fn rejects_bad_sslmode() {
        assert!(connect_options_from(env(&[("PGSSLMODE", "requre")])).is_err());
        assert!(connect_options_from(env(&[("PGSSLMODE", "require")])).is_ok());
    }
}
