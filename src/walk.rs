//! Walking an account's Maildir tree: folder discovery and message listing. Read-only.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// A Maildir folder: a directory with `cur/` or `new/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Folder {
    /// Relative to the account directory, `/`-separated (`INBOX`, `Archive/2019`).
    pub path: String,
    pub dir: PathBuf,
}

/// Result of folder discovery. `problems` lists directories that could not be read or named;
/// when it is non-empty the folder list may be incomplete.
#[derive(Debug, Default)]
pub struct Discovery {
    pub folders: Vec<Folder>,
    pub problems: Vec<String>,
}

/// Find every Maildir folder below `account_dir` (not the account directory itself).
///
/// Fails if `account_dir` itself cannot be read (e.g. missing). Symlinked directories are not
/// followed.
pub fn find_folders(account_dir: &Path) -> io::Result<Discovery> {
    let mut d = Discovery::default();
    let top = sorted_dir(account_dir)?;
    if is_maildir(account_dir) {
        tracing::warn!(
            "account directory itself has cur/ or new/; only subdirectories are indexed as folders"
        );
    }
    let mut stack: Vec<(String, PathBuf)> = Vec::new();
    push_children(&mut stack, &mut d, "", top, false);
    while let Some((path, dir)) = stack.pop() {
        let maildir = is_maildir(&dir);
        if maildir {
            d.folders.push(Folder {
                path: path.clone(),
                dir: dir.clone(),
            });
        }
        match sorted_dir(&dir) {
            Ok(children) => push_children(&mut stack, &mut d, &path, children, maildir),
            Err(e) => d
                .problems
                .push(format!("cannot read directory {path}: {}", e.kind())),
        }
    }
    d.folders.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(d)
}

fn push_children(
    stack: &mut Vec<(String, PathBuf)>,
    d: &mut Discovery,
    parent: &str,
    children: Vec<(OsString, fs::FileType, PathBuf)>,
    parent_is_maildir: bool,
) {
    for (name, ft, full) in children.into_iter().rev() {
        if !ft.is_dir() {
            continue; // files, and symlinks (never followed)
        }
        if parent_is_maildir && (name == "cur" || name == "new" || name == "tmp") {
            continue;
        }
        let Some(name) = name.to_str() else {
            d.problems.push(format!(
                "directory name is not UTF-8 under {:?}: {}",
                parent,
                name.to_string_lossy()
            ));
            continue;
        };
        let path = if parent.is_empty() {
            name.to_string()
        } else {
            format!("{parent}/{name}")
        };
        stack.push((path, full));
    }
}

fn sorted_dir(dir: &Path) -> io::Result<Vec<(OsString, fs::FileType, PathBuf)>> {
    let mut v = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        v.push((entry.file_name(), entry.file_type()?, entry.path()));
    }
    v.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(v)
}

fn is_real_dir(p: &Path) -> bool {
    fs::symlink_metadata(p).is_ok_and(|m| m.is_dir())
}

fn is_maildir(dir: &Path) -> bool {
    is_real_dir(&dir.join("cur")) || is_real_dir(&dir.join("new"))
}

/// A message file seen on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub base_name: String,
    pub flags: String,
    /// `"cur"` or `"new"`.
    pub subdir: &'static str,
    pub file_name: String,
}

impl Entry {
    pub fn path(&self, folder_dir: &Path) -> PathBuf {
        folder_dir.join(self.subdir).join(&self.file_name)
    }

    /// For log lines: `<folder>/<subdir>/<file name>`.
    pub fn display(&self, folder: &str) -> String {
        format!("{folder}/{}/{}", self.subdir, self.file_name)
    }
}

/// Split a Maildir file name into `(base_name, flags)` at the first `:2,`.
pub fn split_name(name: &str) -> (&str, &str) {
    match name.find(":2,") {
        Some(i) => (&name[..i], &name[i + 3..]),
        None => (name, ""),
    }
}

/// Messages in a folder, one per base name (sorted), plus names that were skipped because they
/// are not UTF-8.
#[derive(Debug, Default)]
pub struct Listing {
    pub entries: Vec<Entry>,
    pub non_utf8: Vec<String>,
}

/// List regular files directly inside `new/` and `cur/`, skipping dotfiles, symlinks and
/// anything that is not a file. `new/` is listed first so a message that mbsync moves to `cur/`
/// meanwhile is still seen; if a base name is in both, the `cur/` entry wins.
pub fn list_messages(folder_dir: &Path) -> io::Result<Listing> {
    let mut by_base: BTreeMap<String, Entry> = BTreeMap::new();
    let mut listing = Listing::default();
    for subdir in ["new", "cur"] {
        let dir = folder_dir.join(subdir);
        if !is_real_dir(&dir) {
            continue;
        }
        let read = match fs::read_dir(&dir) {
            Ok(r) => r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e),
        };
        for entry in read {
            let entry = entry?;
            let name = entry.file_name();
            if name.as_encoded_bytes().first() == Some(&b'.') {
                continue;
            }
            let ft = match entry.file_type() {
                Ok(ft) => ft,
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            if !ft.is_file() {
                continue;
            }
            let Some(name) = name.to_str() else {
                listing
                    .non_utf8
                    .push(format!("{subdir}/{}", name.to_string_lossy()));
                continue;
            };
            let (base, flags) = split_name(name);
            by_base.insert(
                base.to_string(),
                Entry {
                    base_name: base.to_string(),
                    flags: flags.to_string(),
                    subdir,
                    file_name: name.to_string(),
                },
            );
        }
    }
    listing.entries = by_base.into_values().collect();
    Ok(listing)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_names() {
        assert_eq!(
            split_name("1700000000.1_1.h,U=42:2,FS"),
            ("1700000000.1_1.h,U=42", "FS")
        );
        assert_eq!(split_name("1700000000.1_1.h:2,"), ("1700000000.1_1.h", ""));
        assert_eq!(split_name("plain-name"), ("plain-name", ""));
    }
}
