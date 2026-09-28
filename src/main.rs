use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use maildir_index::config::{self, Config};
use maildir_index::{check, db, load, sample};

/// Index Maildir trees (mbsync/isync) into PostgreSQL.
///
/// Database settings come from the libpq environment only (PGHOST, PGPORT, PGDATABASE,
/// PGUSER, PGSSLMODE, and PGPASSFILE for the password).
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Configuration file (Maildir root and accounts).
    #[arg(long, global = true, default_value = config::DEFAULT_PATH)]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply the embedded schema migrations and exit.
    Migrate,
    /// Migrate, then load all (or the named) accounts.
    Load {
        /// Account to load (repeatable); default all.
        #[arg(long = "account", value_name = "NAME")]
        accounts: Vec<String>,
        /// Also re-read files of known locations and compare their sha256.
        #[arg(long)]
        verify: bool,
    },
    /// Compare files on disk with present locations; exit non-zero on any difference.
    Check {
        /// Account to check (repeatable); default all.
        #[arg(long = "account", value_name = "NAME")]
        accounts: Vec<String>,
    },
    /// Without a database: extract N random messages and print a summary of each.
    Sample {
        #[arg(long, default_value_t = 20)]
        n: usize,
        #[arg(long, value_name = "NAME")]
        account: Option<String>,
        #[arg(long)]
        seed: Option<u64>,
        /// Print the rendered body_text of each sampled message (for human review).
        #[arg(long)]
        show: bool,
    },
}

/// Appended after RUST_LOG, so they win whatever the operator asks for.
const FORCED_DIRECTIVES: &[&str] = &[
    // Parsers that see message content log it (html5ever traces every token).
    "html5ever=off",
    "markup5ever=off",
    "html2text=off",
    "mail_parser=off",
    // sqlx logs malformed pgpass lines verbatim, which could expose a password.
    "sqlx_postgres::options::pgpass=error",
];

fn init_logging() {
    let mut filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    for d in FORCED_DIRECTIVES {
        filter = filter.add_directive(d.parse().expect("static directive"));
    }
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .init();
    // A panic payload can quote message text (e.g. a string-slicing panic); log the location only.
    std::panic::set_hook(Box::new(|info| {
        let at = info
            .location()
            .map(|l| format!("{}:{}", l.file(), l.line()))
            .unwrap_or_default();
        tracing::error!("panic at {at} (message suppressed)");
    }));
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_logging();
    let result = match cli.command {
        Command::Sample {
            n,
            account,
            seed,
            show,
        } => Config::load(&cli.config)
            .and_then(|c| sample::run(&c, n, account.as_deref(), seed, show))
            .map(|()| true),
        command => tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .map_err(anyhow::Error::from)
            .and_then(|rt| rt.block_on(run(command, &cli.config))),
    };
    match result {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::FAILURE,
        Err(e) => {
            // `{:#}` prints the context chain via Display; Debug could include server DETAIL.
            tracing::error!("{e:#}");
            ExitCode::FAILURE
        }
    }
}

/// Returns Ok(false) when the command ran but found errors or differences.
async fn run(command: Command, config_path: &std::path::Path) -> Result<bool> {
    match command {
        Command::Migrate => {
            let pool = db::connect().await?;
            db::migrate(&pool).await?;
            tracing::info!("migrations applied");
            Ok(true)
        }
        Command::Load { accounts, verify } => {
            let config = Config::load(config_path)?;
            let selected = config.select(&accounts)?;
            let lock_options = db::connect_options()?;
            let pool = db::connect().await?;
            db::migrate(&pool).await?;
            let ids = load::upsert_accounts(&pool, &config).await?;
            let mut ok = true;
            for account in selected {
                let stats = load::load_account(
                    &pool,
                    &lock_options,
                    &config,
                    account,
                    ids[&account.name],
                    load::Options { verify },
                )
                .await;
                println!("{}", stats.summary_line(&account.name));
                ok &= !stats.failed();
            }
            Ok(ok)
        }
        Command::Check { accounts } => {
            let config = Config::load(config_path)?;
            let selected = config.select(&accounts)?;
            let pool = db::connect().await?;
            let mut total = 0;
            for account in selected {
                total += check::check_account(&pool, &config, account).await?;
            }
            Ok(total == 0)
        }
        Command::Sample { .. } => unreachable!("handled without a runtime"),
    }
}
