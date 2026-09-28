//! The `check` command: compare files on disk with present locations, per account and folder.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;
use sqlx::{PgPool, Row};

use crate::config::{AccountConfig, Config};
use crate::walk;

/// At most this many file names are printed per folder and kind of difference.
const MAX_LISTED: usize = 20;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct FolderDiff {
    /// On disk, no present location.
    pub not_indexed: Vec<String>,
    /// Present location, no file on disk.
    pub missing_on_disk: Vec<String>,
    /// Both, but subdir or flags differ.
    pub changed: Vec<String>,
}

impl FolderDiff {
    pub fn count(&self) -> usize {
        self.not_indexed.len() + self.missing_on_disk.len() + self.changed.len()
    }
}

/// Check one account; prints differences to stdout and returns their number.
pub async fn check_account(
    pool: &PgPool,
    config: &Config,
    account: &AccountConfig,
) -> Result<usize> {
    let name = &account.name;
    let account_id: Option<i16> =
        sqlx::query_scalar("SELECT account_id FROM accounts WHERE name = $1")
            .bind(name)
            .fetch_optional(pool)
            .await?;
    let mut db: BTreeMap<String, BTreeMap<String, (String, String)>> = BTreeMap::new();
    if let Some(id) = account_id {
        let rows = sqlx::query(
            "SELECT f.path, l.base_name, l.subdir, l.flags
             FROM folders f JOIN locations l USING (folder_id)
             WHERE f.account_id = $1 AND l.gone_at IS NULL",
        )
        .bind(id)
        .fetch_all(pool)
        .await?;
        for r in rows {
            db.entry(r.try_get("path")?).or_default().insert(
                r.try_get("base_name")?,
                (r.try_get("subdir")?, r.try_get("flags")?),
            );
        }
    } else {
        println!("maildir-index check account={name} problem=account_not_in_database");
    }

    let mut problems = usize::from(account_id.is_none());
    let mut disk: BTreeMap<String, Vec<walk::Entry>> = BTreeMap::new();
    match walk::find_folders(&config.account_dir(account)) {
        Ok(d) => {
            if d.folders.is_empty() {
                println!("maildir-index check account={name} problem=no_maildir_folders");
                problems += 1;
            }
            for p in d.problems {
                println!("maildir-index check account={name} problem=discovery detail={p:?}");
                problems += 1;
            }
            for f in d.folders {
                match walk::list_messages(&f.dir) {
                    Ok(l) => {
                        for n in &l.non_utf8 {
                            println!(
                                "maildir-index check account={name} folder={} problem=non_utf8_name file={n:?}",
                                f.path
                            );
                            problems += 1;
                        }
                        disk.insert(f.path, l.entries);
                    }
                    Err(e) => {
                        println!(
                            "maildir-index check account={name} folder={} problem=unreadable kind={:?}",
                            f.path,
                            e.kind()
                        );
                        problems += 1;
                    }
                }
            }
        }
        Err(e) => {
            println!(
                "maildir-index check account={name} problem=account_directory kind={:?}",
                e.kind()
            );
            problems += 1;
        }
    }

    let folders: BTreeSet<&String> = db.keys().chain(disk.keys()).collect();
    let empty_db = BTreeMap::new();
    let mut differences = problems;
    for folder in folders {
        let on_disk = disk.get(folder).map(Vec::as_slice).unwrap_or_default();
        let diff = diff_folder(on_disk, db.get(folder).unwrap_or(&empty_db));
        if diff.count() == 0 {
            continue;
        }
        differences += diff.count();
        println!(
            "maildir-index check account={name} folder={folder} not_indexed={} missing_on_disk={} changed={}",
            diff.not_indexed.len(),
            diff.missing_on_disk.len(),
            diff.changed.len()
        );
        for (kind, list) in [
            ("not_indexed", &diff.not_indexed),
            ("missing_on_disk", &diff.missing_on_disk),
            ("changed", &diff.changed),
        ] {
            for f in list.iter().take(MAX_LISTED) {
                println!("  {kind} {folder}/{f}");
            }
            if list.len() > MAX_LISTED {
                println!("  {kind} ... and {} more", list.len() - MAX_LISTED);
            }
        }
    }
    println!("maildir-index check account={name} differences={differences}");
    Ok(differences)
}

/// Compare one folder's disk entries with its present locations (base_name → (subdir, flags)).
pub fn diff_folder(disk: &[walk::Entry], db: &BTreeMap<String, (String, String)>) -> FolderDiff {
    let mut d = FolderDiff::default();
    for e in disk {
        match db.get(&e.base_name) {
            None => d.not_indexed.push(format!("{}/{}", e.subdir, e.file_name)),
            Some((subdir, flags)) if subdir != e.subdir || flags != &e.flags => {
                d.changed.push(format!("{}/{}", e.subdir, e.file_name))
            }
            Some(_) => {}
        }
    }
    let on_disk: BTreeSet<&str> = disk.iter().map(|e| e.base_name.as_str()).collect();
    for (base, (subdir, flags)) in db {
        if !on_disk.contains(base.as_str()) {
            let name = if subdir == "new" && flags.is_empty() {
                base.clone()
            } else {
                format!("{base}:2,{flags}")
            };
            d.missing_on_disk.push(format!("{subdir}/{name}"));
        }
    }
    d
}
