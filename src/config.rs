//! The TOML configuration file: Maildir root and accounts. Never holds database settings.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

pub const DEFAULT_PATH: &str = "/etc/maildir-index/config.toml";

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// One subdirectory per account.
    pub maildir_root: PathBuf,
    #[serde(rename = "account", default)]
    pub accounts: Vec<AccountConfig>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AccountConfig {
    /// Directory name under `maildir_root`.
    pub name: String,
    pub address: String,
}

impl Config {
    pub fn load(path: &Path) -> Result<Config> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config file {}", path.display()))?;
        Self::parse(&text).with_context(|| format!("config file {}", path.display()))
    }

    pub fn parse(text: &str) -> Result<Config> {
        // toml's Display quotes the offending line, which may hold an address; report the
        // position and the bare message instead.
        let config: Config = toml::from_str(text).map_err(|e| {
            let at = e
                .span()
                .map(|s| {
                    let line = text[..s.start].matches('\n').count() + 1;
                    format!(" at line {line}")
                })
                .unwrap_or_default();
            anyhow::anyhow!("invalid configuration{at}: {}", e.message())
        })?;
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<()> {
        if self.accounts.is_empty() {
            bail!("no [[account]] entries");
        }
        let mut names = HashSet::new();
        for a in &self.accounts {
            let valid = !a.name.is_empty()
                && a.name != "."
                && a.name != ".."
                && !a.name.contains('/')
                && !a.name.contains('\0');
            if !valid {
                bail!("account name {:?} is not a single directory name", a.name);
            }
            if !names.insert(a.name.as_str()) {
                bail!("account {:?} is listed twice", a.name);
            }
        }
        Ok(())
    }

    /// The accounts named on the command line, or all of them when none are named.
    pub fn select(&self, names: &[String]) -> Result<Vec<&AccountConfig>> {
        if names.is_empty() {
            return Ok(self.accounts.iter().collect());
        }
        names
            .iter()
            .map(|n| {
                self.accounts
                    .iter()
                    .find(|a| &a.name == n)
                    .with_context(|| format!("account {n:?} is not in the config file"))
            })
            .collect()
    }

    pub fn account_dir(&self, account: &AccountConfig) -> PathBuf {
        self.maildir_root.join(&account.name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_example() {
        let c = Config::parse(include_str!("../example.toml")).unwrap();
        assert_eq!(c.accounts.len(), 2);
        assert_eq!(c.select(&["work".into()]).unwrap()[0].name, "work");
        assert!(c.select(&["nope".into()]).is_err());
    }

    #[test]
    fn rejects_password_and_hides_values() {
        let err = Config::parse(
            "maildir_root = \"/x\"\npassword = \"s3cret\"\n[[account]]\nname = \"a\"\naddress = \"a@example.org\"\n",
        )
        .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("line 2"), "{msg}");
        assert!(!msg.contains("s3cret"), "{msg}");
    }

    #[test]
    fn rejects_bad_names() {
        for name in ["", "..", "a/b"] {
            let t = format!(
                "maildir_root = \"/x\"\n[[account]]\nname = {name:?}\naddress = \"a@example.org\"\n"
            );
            assert!(Config::parse(&t).is_err(), "{name}");
        }
    }
}
