//! Test harness: a throwaway database per test on the local server named by `DATABASE_URL`,
//! and a Maildir tree built from the fixtures in a temporary directory.
#![allow(dead_code)]

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use maildir_index::config::Config;
use maildir_index::load::{self, Stats};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::{AssertSqlSafe, Connection, Executor, PgConnection, PgPool};

pub const ACCOUNT: &str = "personal";

/// `None` (after printing why) when `DATABASE_URL` is unset: the test should return early.
pub async fn test_db() -> Option<TestDb> {
    let Ok(url) = std::env::var("DATABASE_URL") else {
        eprintln!(
            "skipping: DATABASE_URL is not set (point it at a local, disposable PostgreSQL 17)"
        );
        return None;
    };
    static N: AtomicU32 = AtomicU32::new(0);
    let name = format!(
        "mdi_test_{}_{}_{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    );
    let admin: PgConnectOptions = url.parse().expect("DATABASE_URL");
    let mut conn = PgConnection::connect_with(&admin)
        .await
        .expect("connect DATABASE_URL");
    // The name is generated above from digits and underscores only.
    conn.execute(AssertSqlSafe(format!("CREATE DATABASE {name}")))
        .await
        .unwrap();
    conn.close().await.unwrap();
    let options = admin.clone().database(&name);
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(options.clone())
        .await
        .unwrap();
    maildir_index::MIGRATOR.run(&pool).await.unwrap();
    Some(TestDb {
        name,
        admin,
        options,
        pool,
        password: password_from_url(&url),
    })
}

fn password_from_url(url: &str) -> Option<String> {
    let rest = url.split_once("://")?.1;
    let userinfo = rest.split_once('@')?.0;
    Some(userinfo.split_once(':')?.1.to_string())
}

pub struct TestDb {
    pub name: String,
    admin: PgConnectOptions,
    pub options: PgConnectOptions,
    pub pool: PgPool,
    password: Option<String>,
}

impl TestDb {
    pub async fn drop_db(self) {
        self.pool.close().await;
        let mut conn = PgConnection::connect_with(&self.admin).await.unwrap();
        conn.execute(AssertSqlSafe(format!(
            "DROP DATABASE IF EXISTS {} WITH (FORCE)",
            self.name
        )))
        .await
        .unwrap();
    }

    pub async fn load(&self, tree: &Tree) -> Stats {
        self.load_opts(tree, load::Options::default()).await
    }

    pub async fn load_opts(&self, tree: &Tree, opts: load::Options) -> Stats {
        let config = tree.config();
        let ids = load::upsert_accounts(&self.pool, &config).await.unwrap();
        load::load_account(
            &self.pool,
            &self.options,
            &config,
            &config.accounts[0],
            ids[ACCOUNT],
            opts,
        )
        .await
    }

    pub async fn scalar_i64(&self, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(&self.pool).await.unwrap()
    }

    /// Run the real binary against this database, configured the way a deployment is: libpq
    /// environment only, password (if any) through a 0600 PGPASSFILE.
    pub fn run_binary(&self, tree: &Tree, args: &[&str], rust_log: &str) -> Output {
        let config_path = tree.write_config();
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_maildir-index"));
        cmd.arg("--config").arg(&config_path).args(args);
        for k in [
            "PGPASSWORD",
            "PGPASSFILE",
            "PGSERVICE",
            "DATABASE_URL",
            "PGSSLMODE",
        ] {
            cmd.env_remove(k);
        }
        cmd.env("PGHOST", self.options.get_host())
            .env("PGPORT", self.options.get_port().to_string())
            .env("PGUSER", self.options.get_username())
            .env("PGDATABASE", &self.name)
            .env("PGSSLMODE", "prefer")
            .env("RUST_LOG", rust_log);
        if let Some(pw) = &self.password {
            let pgpass = tree.dir.path().join("pgpass");
            std::fs::write(
                &pgpass,
                format!("*:*:*:*:{}\n", pw.replace('\\', "\\\\").replace(':', "\\:")),
            )
            .unwrap();
            std::fs::set_permissions(&pgpass, std::fs::Permissions::from_mode(0o600)).unwrap();
            cmd.env("PGPASSFILE", pgpass);
        }
        cmd.output().expect("run maildir-index")
    }
}

/// A Maildir root with one account, built in a temporary directory.
pub struct Tree {
    pub dir: tempfile::TempDir,
}

impl Tree {
    pub fn new() -> Tree {
        let t = Tree {
            dir: tempfile::tempdir().unwrap(),
        };
        std::fs::create_dir_all(t.account_dir()).unwrap();
        t
    }

    pub fn root(&self) -> PathBuf {
        self.dir.path().join("mail")
    }

    pub fn account_dir(&self) -> PathBuf {
        self.root().join(ACCOUNT)
    }

    /// Create a Maildir folder (cur/new/tmp) and return its path.
    pub fn folder(&self, path: &str) -> PathBuf {
        let d = self.account_dir().join(path);
        for s in ["cur", "new", "tmp"] {
            std::fs::create_dir_all(d.join(s)).unwrap();
        }
        d
    }

    /// Put a fixture into `<folder>/<subdir>/<file_name>`.
    pub fn put(&self, folder: &str, subdir: &str, file_name: &str, fixture_name: &str) -> PathBuf {
        let p = self.folder(folder).join(subdir).join(file_name);
        std::fs::write(&p, fixture(fixture_name)).unwrap();
        p
    }

    pub fn path(&self, folder: &str, subdir: &str, file_name: &str) -> PathBuf {
        self.account_dir().join(folder).join(subdir).join(file_name)
    }

    pub fn rename(&self, from: (&str, &str, &str), to: (&str, &str, &str)) {
        let to_path = self.path(to.0, to.1, to.2);
        std::fs::create_dir_all(to_path.parent().unwrap()).unwrap();
        std::fs::rename(self.path(from.0, from.1, from.2), to_path).unwrap();
    }

    pub fn config_text(&self) -> String {
        format!(
            "maildir_root = {:?}\n[[account]]\nname = {ACCOUNT:?}\naddress = \"me@example.org\"\n",
            self.root().to_str().unwrap()
        )
    }

    pub fn config(&self) -> Config {
        Config::parse(&self.config_text()).unwrap()
    }

    pub fn write_config(&self) -> PathBuf {
        let p = self.dir.path().join("config.toml");
        std::fs::write(&p, self.config_text()).unwrap();
        p
    }
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(fixture_path(name)).unwrap()
}

pub fn fixture_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// Every fixture file name.
pub fn all_fixtures() -> Vec<String> {
    let mut v: Vec<String> =
        std::fs::read_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures"))
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
    v.sort();
    v
}

/// An mbsync-style file name for the i-th fixture.
pub fn mbsync_name(i: usize, flags: &str) -> String {
    format!("1700000000.{}_1.testhost,U={}:2,{flags}", 1000 + i, i + 1)
}
