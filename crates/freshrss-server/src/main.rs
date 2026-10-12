//! `freshrss-rs` binary: serve the reader, or run one of the admin subcommands.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use freshrss_server::config::{AppConfig, LogFormat};
use freshrss_server::db::Db;
use freshrss_server::router::{router, AppState};
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::EnvFilter;

#[derive(Parser)]
#[command(
    name = "freshrss-rs",
    version,
    about = "A Rust port of the FreshRSS backend, serving a Feeddi-style reader UI"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Configuration file. Falls back to $FRESHRSS_CONFIG, then ./config.toml.
    #[arg(long, global = true, env = "FRESHRSS_CONFIG")]
    config: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// Run the web server and the background refresh scheduler.
    Serve,
    /// Apply pending database migrations and exit.
    Migrate,
    /// Fetch or read a feed and print what the parser, GUID policy and sanitizer made of it.
    Parse(freshrss_server::cli::ParseArgs),
}

#[tokio::main(flavor = "multi_thread")]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    // `parse` is a feed-inspection tool: it needs no database and no configuration, and requiring
    // both would make it useless in the one situation it exists for — checking a parser change
    // against a feed from a clean checkout.
    if let Command::Parse(args) = cli.command {
        return freshrss_server::cli::run(args).await;
    }

    let config = AppConfig::load_from(cli.config.as_deref())
        .context("could not load configuration (set FRESHRSS_CONFIG or edit config.toml)")?;

    init_tracing(&config);

    match cli.command {
        Command::Migrate => {
            // Db::open already migrated; report the resulting state explicitly for the operator.
            let db = Db::open(&config.database).await?;
            println!("schema at version {}", db.schema_version().await?);
        }
        Command::Serve => serve(config).await?,
        Command::Parse(_) => unreachable!("handled above, before configuration is loaded"),
    }
    Ok(())
}

async fn serve(config: AppConfig) -> Result<()> {
    let bind = config.server.bind;
    let db = Db::open(&config.database).await?;
    tracing::info!(
        database = %config.database.path.display(),
        pool_size = config.database.pool_size,
        "database ready"
    );

    let app = router(AppState::new(db.clone(), config));
    let listener = tokio::net::TcpListener::bind(bind)
        .await
        .with_context(|| format!("could not bind to {bind}"))?;
    tracing::info!(%bind, "listening");

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;

    db.optimize().await?;
    tracing::info!("shut down cleanly");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        if let Err(err) = tokio::signal::ctrl_c().await {
            tracing::warn!(%err, "could not install the ctrl-c handler");
            std::future::pending::<()>().await;
        }
    };

    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(err) => {
                tracing::warn!(%err, "could not install the SIGTERM handler");
                std::future::pending::<()>().await;
            }
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => tracing::info!("received ctrl-c"),
        _ = terminate => tracing::info!("received SIGTERM"),
    }
    tracing::info!("draining in-flight requests");
}

fn init_tracing(config: &AppConfig) {
    let filter = EnvFilter::try_new(&config.log.level)
        .unwrap_or_else(|err| {
            eprintln!(
                "invalid log level {:?}: {err}; falling back to info",
                config.log.level
            );
            EnvFilter::new("info")
        })
        // The SQL emitted by the search compiler is useful but far too loud by default.
        .add_directive("sqlx=warn".parse().expect("static directive"));

    let builder = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(true);

    match config.log.format {
        LogFormat::Pretty => builder.pretty().with_span_events(FmtSpan::NONE).init(),
        LogFormat::Compact => builder.compact().with_span_events(FmtSpan::NONE).init(),
        LogFormat::Json => builder.json().with_span_events(FmtSpan::NONE).init(),
    }
}
