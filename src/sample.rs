//! The `sample` command: extract a few random messages without a database, for eyeballing the
//! body rendering. Message text is printed only with `--show`, and only to stdout.

use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::config::Config;
use crate::extract;
use crate::sha_prefix;
use crate::walk;

/// splitmix64: small, seedable, good enough for picking files.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

pub fn run(
    config: &Config,
    n: usize,
    account: Option<&str>,
    seed: Option<u64>,
    show: bool,
) -> Result<()> {
    let names: Vec<String> = account.map(|a| vec![a.to_string()]).unwrap_or_default();
    let seed = seed.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0)
    });
    let mut rng = Rng(seed);
    // Reservoir sampling over all message files, in a deterministic (sorted) order.
    let mut picked: Vec<PathBuf> = Vec::with_capacity(n);
    let mut total = 0u64;
    for acct in config.select(&names)? {
        let dir = config.account_dir(acct);
        let d = walk::find_folders(&dir)
            .with_context(|| format!("account {}: cannot read account directory", acct.name))?;
        for f in &d.folders {
            let listing = walk::list_messages(&f.dir)
                .with_context(|| format!("account {}: cannot list folder {}", acct.name, f.path))?;
            for e in listing.entries {
                total += 1;
                if picked.len() < n {
                    picked.push(e.path(&f.dir));
                } else {
                    let j = rng.below(total) as usize;
                    if j < n {
                        picked[j] = e.path(&f.dir);
                    }
                }
            }
        }
    }
    if total == 0 {
        bail!("no message files found");
    }
    println!(
        "maildir-index sample seed={seed} files={total} picked={}",
        picked.len()
    );
    for path in picked {
        let bytes = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                println!("read_error={:?}", e.kind());
                continue;
            }
        };
        let e = extract::extract(&bytes);
        let text_len = e.row.body_text.as_deref().map_or(0, |t| t.chars().count());
        println!(
            "sha256={} body_source={} text_len={text_len}{}",
            sha_prefix(&e.row.sha256),
            e.row.body_source.as_str(),
            e.problem
                .map(|p| format!(" problem={:?}", p.as_str()))
                .unwrap_or_default()
        );
        if show {
            println!("----- body_text begin -----");
            println!("{}", e.row.body_text.as_deref().unwrap_or(""));
            println!("----- body_text end -----");
        }
    }
    Ok(())
}
