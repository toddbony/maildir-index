//! The `load` pass: walk an account, record locations, insert new messages, mark gone ones.

use std::collections::{HashMap, HashSet};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use sqlx::postgres::PgConnectOptions;
use sqlx::{Connection, PgConnection, PgPool, Postgres, QueryBuilder, Row};

use crate::config::{AccountConfig, Config};
use crate::extract::{self, MessageRow};
use crate::walk::{self, Entry, Folder};
use crate::{LOADER_VERSION, par, sha_prefix};

/// New files are read, hashed and parsed in batches bounded by count and total size.
const BATCH_FILES: usize = 256;
const BATCH_BYTES: u64 = 64 << 20;
/// Known locations are updated in chunks of this many rows.
const UPDATE_CHUNK: usize = 5000;
/// First key of the advisory lock that stops two passes over one account overlapping.
const LOCK_CLASS: i32 = 0x6d69_6478; // "midx"

#[derive(Debug, Clone, Copy, Default)]
pub struct Options {
    /// Re-read and hash files of known locations and compare with the stored sha256.
    pub verify: bool,
}

#[derive(Debug, Default, Clone)]
pub struct Stats {
    pub files_seen: u64,
    pub new_messages: u64,
    pub new_locations: u64,
    pub gone: u64,
    pub errors: u64,
    /// The account directory was missing/empty or the pass saw too few files; nothing was
    /// marked gone.
    pub guard_tripped: bool,
    pub duration_s: u64,
}

impl Stats {
    pub fn failed(&self) -> bool {
        self.errors > 0 || self.guard_tripped
    }

    pub fn summary_line(&self, account: &str) -> String {
        format!(
            "maildir-index account={account} files_seen={} new_messages={} new_locations={} gone={} errors={} duration_s={}",
            self.files_seen,
            self.new_messages,
            self.new_locations,
            self.gone,
            self.errors,
            self.duration_s
        )
    }
}

/// Insert or update every configured account by name; returns name → account_id.
pub async fn upsert_accounts(pool: &PgPool, config: &Config) -> Result<HashMap<String, i16>> {
    let mut ids = HashMap::new();
    for a in &config.accounts {
        let id: i16 = sqlx::query_scalar(
            "INSERT INTO accounts (name, address) VALUES ($1, $2)
             ON CONFLICT (name) DO UPDATE SET address = EXCLUDED.address
             RETURNING account_id",
        )
        .bind(&a.name)
        .bind(&a.address)
        .fetch_one(pool)
        .await
        .context("upserting account")?;
        ids.insert(a.name.clone(), id);
    }
    Ok(ids)
}

/// One pass over one account. Writes a `load_runs` row. Errors that stop the pass are logged
/// and counted; the returned stats are always usable for the summary line.
pub async fn load_account(
    pool: &PgPool,
    lock_options: &PgConnectOptions,
    config: &Config,
    account: &AccountConfig,
    account_id: i16,
    options: Options,
) -> Stats {
    let started = Instant::now();
    let mut stats = Stats::default();
    let run: Result<(i32, DateTime<Utc>)> = sqlx::query_as(
        "INSERT INTO load_runs (account_id, loader_version) VALUES ($1, $2)
         RETURNING run_id, started_at",
    )
    .bind(account_id)
    .bind(LOADER_VERSION)
    .fetch_one(pool)
    .await
    .context("recording load run");
    let result = match run {
        Ok((run_id, seen_at)) => {
            let r = Pass {
                pool,
                account_id,
                seen_at,
                options,
                stats: &mut stats,
            }
            .run(lock_options, &config.account_dir(account))
            .await;
            if let Err(e) = &r {
                tracing::error!(account = %account.name, "load failed: {e:#}");
                stats.errors += 1;
            }
            finish_run(pool, run_id, &stats).await
        }
        Err(e) => Err(e),
    };
    if let Err(e) = result {
        tracing::error!(account = %account.name, "{e:#}");
        stats.errors += 1;
    }
    stats.duration_s = started.elapsed().as_secs();
    stats
}

async fn finish_run(pool: &PgPool, run_id: i32, stats: &Stats) -> Result<()> {
    sqlx::query(
        "UPDATE load_runs SET finished_at = now(), files_seen = $2, inserted = $3, errors = $4
         WHERE run_id = $1",
    )
    .bind(run_id)
    .bind(clamp_i32(stats.files_seen))
    .bind(clamp_i32(stats.new_messages))
    .bind(clamp_i32(stats.errors))
    .execute(pool)
    .await
    .context("finishing load run")?;
    Ok(())
}

fn clamp_i32(n: u64) -> i32 {
    i32::try_from(n).unwrap_or(i32::MAX)
}

struct Pass<'a> {
    pool: &'a PgPool,
    account_id: i16,
    /// `load_runs.started_at`; every location seen in this pass gets exactly this `last_seen`.
    seen_at: DateTime<Utc>,
    options: Options,
    stats: &'a mut Stats,
}

impl Pass<'_> {
    async fn run(mut self, lock_options: &PgConnectOptions, account_dir: &Path) -> Result<()> {
        // Two overlapping passes would each mark the other's locations gone. The lock lives on
        // its own connection so that it is released however this function exits.
        let mut lock_conn = PgConnection::connect_with(lock_options)
            .await
            .context("connecting for the account lock")?;
        let locked: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1, $2)")
            .bind(LOCK_CLASS)
            .bind(i32::from(self.account_id))
            .fetch_one(&mut lock_conn)
            .await?;
        if !locked {
            anyhow::bail!("another load of this account is running");
        }

        let discovery = match walk::find_folders(account_dir) {
            Ok(d) if !d.folders.is_empty() => d,
            Ok(_) => {
                return self.guard("account directory contains no Maildir folders");
            }
            Err(e) => {
                return self.guard(&format!("account directory cannot be read: {}", e.kind()));
            }
        };
        for p in &discovery.problems {
            tracing::error!("{p}");
            self.stats.errors += 1;
        }

        let mut unlisted: Vec<i32> = Vec::new();
        for folder in &discovery.folders {
            let folder_id = self.folder_id(&folder.path).await?;
            match walk::list_messages(&folder.dir) {
                Ok(listing) => {
                    for name in &listing.non_utf8 {
                        tracing::error!(folder = %folder.path, "skipping file with non-UTF-8 name: {name}");
                        self.stats.errors += 1;
                    }
                    self.folder_pass(folder, folder_id, listing.entries).await?;
                }
                Err(e) => {
                    tracing::error!(folder = %folder.path, "cannot list folder: {}", e.kind());
                    self.stats.errors += 1;
                    unlisted.push(folder_id);
                }
            }
        }

        if !discovery.problems.is_empty() {
            tracing::error!("folder discovery was incomplete; not marking anything gone");
            return Ok(());
        }
        self.mark_gone(&unlisted).await?;
        drop(lock_conn);
        Ok(())
    }

    /// The gone guard tripped before any marking: record it and stop.
    fn guard(&mut self, why: &str) -> Result<()> {
        tracing::error!("{why}; not marking anything gone");
        self.stats.errors += 1;
        self.stats.guard_tripped = true;
        Ok(())
    }

    async fn folder_id(&self, path: &str) -> Result<i32> {
        let existing: Option<i32> =
            sqlx::query_scalar("SELECT folder_id FROM folders WHERE account_id = $1 AND path = $2")
                .bind(self.account_id)
                .bind(path)
                .fetch_optional(self.pool)
                .await?;
        if let Some(id) = existing {
            return Ok(id);
        }
        Ok(sqlx::query_scalar(
            "INSERT INTO folders (account_id, path) VALUES ($1, $2)
             ON CONFLICT (account_id, path) DO UPDATE SET path = EXCLUDED.path
             RETURNING folder_id",
        )
        .bind(self.account_id)
        .bind(path)
        .fetch_one(self.pool)
        .await?)
    }

    async fn folder_pass(
        &mut self,
        folder: &Folder,
        folder_id: i32,
        entries: Vec<Entry>,
    ) -> Result<()> {
        self.stats.files_seen += entries.len() as u64;
        let rows = sqlx::query("SELECT base_name, sha256 FROM locations WHERE folder_id = $1")
            .bind(folder_id)
            .fetch_all(self.pool)
            .await?;
        let mut known: HashMap<String, Vec<u8>> = HashMap::with_capacity(rows.len());
        for r in rows {
            known.insert(r.try_get("base_name")?, r.try_get("sha256")?);
        }

        let mut seen: Vec<&Entry> = Vec::new();
        let mut verify: Vec<(PathBuf, String, Vec<u8>)> = Vec::new();
        let mut batch: Vec<NewFile> = Vec::new();
        let mut batch_bytes = 0u64;
        for entry in &entries {
            if let Some(sha) = known.get(&entry.base_name) {
                seen.push(entry);
                if self.options.verify {
                    verify.push((
                        entry.path(&folder.dir),
                        entry.display(&folder.path),
                        sha.clone(),
                    ));
                }
                continue;
            }
            let path = entry.path(&folder.dir);
            let size = match std::fs::symlink_metadata(&path) {
                Ok(m) => m.len(),
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => {
                    tracing::error!(file = %entry.display(&folder.path), "cannot stat: {}", e.kind());
                    self.stats.errors += 1;
                    continue;
                }
            };
            batch_bytes += size;
            batch.push(NewFile {
                entry: entry.clone(),
                path,
                name: entry.display(&folder.path),
            });
            if batch.len() >= BATCH_FILES || batch_bytes >= BATCH_BYTES {
                self.insert_new(folder_id, std::mem::take(&mut batch))
                    .await?;
                batch_bytes = 0;
            }
        }
        if !batch.is_empty() {
            self.insert_new(folder_id, batch).await?;
        }
        for chunk in seen.chunks(UPDATE_CHUNK) {
            self.touch_known(folder_id, chunk).await?;
        }
        for chunk in verify.chunks(BATCH_FILES) {
            self.verify(chunk.to_vec()).await;
        }
        Ok(())
    }

    /// Step 1: a known location. Update subdir/flags/last_seen and clear gone_at; no file read.
    async fn touch_known(&self, folder_id: i32, entries: &[&Entry]) -> Result<()> {
        let bases: Vec<&str> = entries.iter().map(|e| e.base_name.as_str()).collect();
        let subdirs: Vec<&str> = entries.iter().map(|e| e.subdir).collect();
        let flags: Vec<&str> = entries.iter().map(|e| e.flags.as_str()).collect();
        sqlx::query(
            "UPDATE locations AS l
             SET subdir = u.subdir, flags = u.flags, last_seen = $2, gone_at = NULL
             FROM unnest($3::text[], $4::text[], $5::text[]) AS u(base_name, subdir, flags)
             WHERE l.folder_id = $1 AND l.base_name = u.base_name",
        )
        .bind(folder_id)
        .bind(self.seen_at)
        .bind(&bases)
        .bind(&subdirs)
        .bind(&flags)
        .execute(self.pool)
        .await?;
        Ok(())
    }

    /// `--verify`: hash files of known locations and compare with the stored sha256.
    async fn verify(&mut self, files: Vec<(PathBuf, String, Vec<u8>)>) {
        let results = par::map(files, |(path, name, stored)| {
            let r = std::fs::read(&path).map(|b| extract::sha256(&b));
            (name, stored, r)
        });
        for (name, stored, r) in results {
            match r {
                Ok(actual) if actual[..] == stored[..] => {}
                Ok(actual) => {
                    tracing::error!(
                        file = %name,
                        stored = %sha_prefix(&stored),
                        actual = %sha_prefix(&actual),
                        "file content differs from stored sha256"
                    );
                    self.stats.errors += 1;
                }
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => {
                    tracing::error!(file = %name, "cannot read: {}", e.kind());
                    self.stats.errors += 1;
                }
            }
        }
    }

    /// Steps 2–4 for a batch of files with unknown base names, in one transaction.
    async fn insert_new(&mut self, folder_id: i32, batch: Vec<NewFile>) -> Result<()> {
        // 2. Read and hash.
        let read = par::map(batch, |f| {
            let r = std::fs::read(&f.path).map(|bytes| {
                let sha = extract::sha256(&bytes);
                (bytes, sha)
            });
            (f, r)
        });
        let mut files: Vec<(NewFile, Vec<u8>, [u8; 32])> = Vec::with_capacity(read.len());
        for (f, r) in read {
            match r {
                Ok((bytes, sha)) => files.push((f, bytes, sha)),
                // Renamed or deleted since the listing: it will be seen next pass.
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    tracing::debug!(file = %f.name, "vanished before reading");
                }
                Err(e) => {
                    tracing::error!(file = %f.name, "cannot read: {}", e.kind());
                    self.stats.errors += 1;
                }
            }
        }
        if files.is_empty() {
            return Ok(());
        }

        // 3. Parse only what is not in `messages` yet (once per sha256 within the batch).
        let shas: Vec<&[u8]> = files.iter().map(|(_, _, s)| &s[..]).collect();
        let existing: HashSet<Vec<u8>> =
            sqlx::query_scalar("SELECT sha256 FROM messages WHERE sha256 = ANY($1::bytea[])")
                .bind(&shas)
                .fetch_all(self.pool)
                .await?
                .into_iter()
                .collect();
        let mut to_parse: Vec<(&[u8], &str)> = Vec::new();
        let mut queued: HashSet<[u8; 32]> = HashSet::new();
        for (f, bytes, sha) in &files {
            if !existing.contains(&sha[..]) && queued.insert(*sha) {
                to_parse.push((bytes, &f.name));
            }
        }
        let mut rows: Vec<MessageRow> = Vec::with_capacity(to_parse.len());
        for (e, name) in par::map(to_parse, |(bytes, name)| (extract::extract(bytes), name)) {
            if let Some(p) = e.problem {
                tracing::error!(file = %name, sha256 = %sha_prefix(&e.row.sha256), "{}", p.as_str());
                self.stats.errors += 1;
            }
            rows.push(e.row);
        }

        // 4. Insert messages and locations together.
        let mut tx = self.pool.begin().await?;
        if !rows.is_empty() {
            let mut q = QueryBuilder::<Postgres>::new(
                "INSERT INTO messages (sha256, message_id, in_reply_to, date_raw, sent_at, from_addr,
                 from_name, to_addrs, cc_addrs, subject, list_id, list_unsubscribe, raw_headers,
                 body_text, body_source, size_bytes, attachment_count, loader_version) ",
            );
            q.push_values(&rows, |mut b, r| {
                b.push_bind(&r.sha256[..])
                    .push_bind(&r.message_id)
                    .push_bind(&r.in_reply_to)
                    .push_bind(&r.date_raw)
                    .push_bind(r.sent_at)
                    .push_bind(&r.from_addr)
                    .push_bind(&r.from_name)
                    .push_bind(&r.to_addrs)
                    .push_bind(&r.cc_addrs)
                    .push_bind(&r.subject)
                    .push_bind(&r.list_id)
                    .push_bind(&r.list_unsubscribe)
                    .push_bind(&r.raw_headers)
                    .push_bind(&r.body_text)
                    .push_bind(r.body_source.as_str())
                    .push_bind(r.size_bytes)
                    .push_bind(r.attachment_count)
                    .push_bind(LOADER_VERSION);
            });
            q.push(" ON CONFLICT (sha256) DO NOTHING");
            self.stats.new_messages += q.build().execute(&mut *tx).await?.rows_affected();
        }

        let mut q = QueryBuilder::<Postgres>::new(
            "INSERT INTO locations (folder_id, base_name, sha256, subdir, flags, last_seen) ",
        );
        q.push_values(&files, |mut b, (f, _, sha)| {
            b.push_bind(folder_id)
                .push_bind(&f.entry.base_name)
                .push_bind(&sha[..])
                .push_bind(f.entry.subdir)
                .push_bind(&f.entry.flags)
                .push_bind(self.seen_at);
        });
        q.push(
            " ON CONFLICT (folder_id, base_name) DO UPDATE
              SET subdir = EXCLUDED.subdir, flags = EXCLUDED.flags,
                  last_seen = EXCLUDED.last_seen, gone_at = NULL
              RETURNING (xmax = 0) AS inserted",
        );
        let inserted: Vec<bool> = q.build_query_scalar().fetch_all(&mut *tx).await?;
        tx.commit().await?;
        self.stats.new_locations += inserted.into_iter().filter(|&i| i).count() as u64;
        Ok(())
    }

    /// End of pass: locations not seen get `gone_at`, unless the gone guard trips.
    async fn mark_gone(&mut self, unlisted: &[i32]) -> Result<()> {
        let (present, seen): (i64, i64) = sqlx::query_as(
            "SELECT count(*), count(*) FILTER (WHERE l.last_seen = $2)
             FROM locations l JOIN folders f USING (folder_id)
             WHERE f.account_id = $1 AND l.gone_at IS NULL",
        )
        .bind(self.account_id)
        .bind(self.seen_at)
        .fetch_one(self.pool)
        .await?;
        if seen * 2 < present {
            tracing::error!(
                seen,
                present,
                "gone guard: this pass saw fewer than half of the present locations; not marking anything gone"
            );
            self.stats.errors += 1;
            self.stats.guard_tripped = true;
            return Ok(());
        }
        let gone = sqlx::query(
            "UPDATE locations l SET gone_at = now()
             FROM folders f
             WHERE l.folder_id = f.folder_id AND f.account_id = $1
               AND l.gone_at IS NULL AND l.last_seen <> $2
               AND NOT (l.folder_id = ANY($3))",
        )
        .bind(self.account_id)
        .bind(self.seen_at)
        .bind(unlisted)
        .execute(self.pool)
        .await?
        .rows_affected();
        self.stats.gone = gone;
        Ok(())
    }
}

struct NewFile {
    entry: Entry,
    path: PathBuf,
    name: String,
}
