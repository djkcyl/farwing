//! `farwing-server` command line: run the server, or print a one-time setup
//! code. Everything else is done in the web console or through the
//! administration API (`docs/server-admin-api.md`).

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::net::SocketAddr;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "farwing-server",
    version,
    about = "Farwing Server: self-hosted backend for Farwing"
)]
struct Cli {
    /// Data directory (database, certificates, wallpapers). Default: `data`
    /// next to this program.
    #[arg(long, global = true, env = "FARWING_DATA_DIR")]
    data_dir: Option<PathBuf>,
    /// Listen address of the web and API port, e.g. `0.0.0.0:8443`. When
    /// given it cannot be changed online.
    #[arg(long, env = "FARWING_LISTEN", value_parser = farwing_server::config::parse_listen)]
    listen: Option<SocketAddr>,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Print a new one-time setup code: it opens the setup page while no
    /// administrator exists, and the recovery of an administrator account
    /// afterwards. Valid for 24 hours; replaces any earlier code.
    SetupCode,
}

fn data_dir(given: Option<PathBuf>) -> Result<PathBuf> {
    match given {
        Some(dir) => Ok(dir),
        None => Ok(std::env::current_exe()
            .context("locating this program")?
            .parent()
            .context("locating this program's directory")?
            .join("data")),
    }
}

/// Log to standard output with a filter the settings can change while the
/// server runs (`RUST_LOG` wins over the setting).
fn init_logging() {
    use tracing_subscriber::prelude::*;
    use tracing_subscriber::{EnvFilter, reload};
    let from_env = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok());
    let fixed = from_env.is_some();
    let (filter, handle) =
        reload::Layer::new(from_env.unwrap_or_else(|| EnvFilter::new("info")));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .init();
    if !fixed {
        farwing_server::server::set_log_reloader(move |directive| {
            if let Ok(filter) = EnvFilter::try_new(directive) {
                let _ = handle.reload(filter);
            }
        });
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let data_dir = data_dir(cli.data_dir)?;
    match cli.command {
        Some(Command::SetupCode) => {
            std::fs::create_dir_all(&data_dir)
                .with_context(|| format!("creating {}", data_dir.display()))?;
            let conn = farwing_server::db::open_connection(&data_dir.join("farwing.db"))?;
            let code = farwing_server::setup::new_code(&conn)?;
            let state = if farwing_server::setup::needs_setup(&conn)? {
                "set up the server"
            } else {
                "recover an administrator account"
            };
            println!("Setup code: {code}");
            println!(
                "Open https://<this server>/setup to {state}. The code is valid for 24 hours."
            );
            Ok(())
        }
        None => {
            init_logging();
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(farwing_server::serve(data_dir, cli.listen))
        }
    }
}
