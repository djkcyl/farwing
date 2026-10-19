//! `farwing-server` command line.

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use farwing_server::config::{self, Config};
use farwing_server::{admin, crypto};
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(
    name = "farwing-server",
    version,
    about = "Farwing Server: self-hosted backend for Farwing"
)]
struct Cli {
    /// Configuration file.
    #[arg(
        long,
        short = 'c',
        global = true,
        env = "FARWING_CONFIG",
        default_value = "farwing-server.toml"
    )]
    config: PathBuf,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Write a commented configuration file with a fresh TURN shared secret.
    Init {
        /// Public https:// URL of this server.
        #[arg(long)]
        public_url: Option<String>,
        /// Data directory (database, wallpapers, certificates).
        #[arg(long)]
        data_dir: Option<String>,
        /// Overwrite an existing file.
        #[arg(long)]
        force: bool,
    },
    /// Run the server.
    Serve,
    /// Manage accounts.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
    /// Manage devices.
    Device {
        #[command(subcommand)]
        command: DeviceCommand,
    },
    /// Inspect sessions.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Inspect friendships and grants.
    Friend {
        #[command(subcommand)]
        command: FriendCommand,
    },
}

#[derive(Subcommand)]
enum UserCommand {
    /// Create an account.
    Create {
        username: String,
        #[arg(long)]
        nickname: Option<String>,
        /// Read the password from the first line of stdin instead of prompting.
        #[arg(long)]
        password_stdin: bool,
        /// Make the account an administrator of the web console.
        #[arg(long)]
        admin: bool,
    },
    /// List accounts.
    List,
    /// Make an account an administrator of the web console.
    Promote { username: String },
    /// Make an administrator an ordinary account.
    Demote { username: String },
    /// Turn off an account's two-step verification (lost authenticator).
    ResetTotp { username: String },
    /// Change a password (revokes the account's logins).
    Passwd {
        username: String,
        #[arg(long)]
        password_stdin: bool,
    },
    /// Disable an account (revokes its logins).
    Disable { username: String },
    /// Re-enable an account.
    Enable { username: String },
    /// Delete an account (unbinds its devices, revokes its logins).
    Delete { username: String },
}

#[derive(Subcommand)]
enum DeviceCommand {
    /// List devices.
    List {
        /// Only devices bound to this account.
        #[arg(long)]
        user: Option<String>,
    },
    /// Unbind a device from its account and revoke its logins.
    Remove { device_id: String },
}

#[derive(Subcommand)]
enum SessionCommand {
    /// List active sessions.
    List,
}

#[derive(Subcommand)]
enum FriendCommand {
    /// List an account's friends and the grants it gives them.
    List { username: String },
}

fn init_logging(directive: Option<&str>) {
    use tracing_subscriber::EnvFilter;
    let filter = std::env::var("RUST_LOG")
        .ok()
        .and_then(|v| EnvFilter::try_new(v).ok())
        .or_else(|| directive.and_then(|d| EnvFilter::try_new(d).ok()))
        .unwrap_or_else(|| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();
}

fn load(path: &Path) -> Result<Config> {
    Config::load(path).with_context(|| {
        format!(
            "loading {} (create one with `farwing-server init --config {}`)",
            path.display(),
            path.display()
        )
    })
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Init {
            public_url,
            data_dir,
            force,
        } => {
            if cli.config.exists() && !force {
                bail!(
                    "{} already exists (use --force to overwrite)",
                    cli.config.display()
                );
            }
            let public_url = public_url.unwrap_or_else(|| "https://localhost:8443".to_owned());
            let data_dir = data_dir.unwrap_or_else(|| "./data".to_owned());
            let text =
                config::render_template(&public_url, &data_dir, &crypto::new_shared_secret());
            let base = cli
                .config
                .parent()
                .map(Path::to_path_buf)
                .unwrap_or_else(|| PathBuf::from("."));
            Config::from_toml_str(&text, &base).context("generated configuration is invalid")?;
            if let Some(dir) = cli.config.parent().filter(|d| !d.as_os_str().is_empty()) {
                std::fs::create_dir_all(dir)?;
            }
            std::fs::write(&cli.config, text)
                .with_context(|| format!("writing {}", cli.config.display()))?;
            println!("wrote {}", cli.config.display());
            println!(
                "next: edit [tls] and [turn], then `farwing-server user create <name> --config {}`",
                cli.config.display()
            );
            Ok(())
        }
        Command::Serve => {
            let cfg = load(&cli.config)?;
            init_logging(cfg.log.as_deref());
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(farwing_server::serve(cfg))
        }
        Command::User { command } => {
            let cfg = load(&cli.config)?;
            match command {
                UserCommand::Create {
                    username,
                    nickname,
                    password_stdin,
                    admin: make_admin,
                } => {
                    let pw = admin::read_password(password_stdin)?;
                    let id = admin::user_create(&cfg, &username, nickname.as_deref(), &pw)?;
                    if make_admin {
                        admin::user_set_role(&cfg, &username, true)?;
                    }
                    println!(
                        "created {} {username} ({id})",
                        if make_admin { "administrator" } else { "user" }
                    );
                    Ok(())
                }
                UserCommand::List => admin::user_list(&cfg),
                UserCommand::Promote { username } => {
                    admin::user_set_role(&cfg, &username, true)?;
                    println!("{username} is now an administrator");
                    Ok(())
                }
                UserCommand::Demote { username } => {
                    admin::user_set_role(&cfg, &username, false)?;
                    println!("{username} is now an ordinary account");
                    Ok(())
                }
                UserCommand::ResetTotp { username } => admin::user_reset_totp(&cfg, &username),
                UserCommand::Passwd {
                    username,
                    password_stdin,
                } => {
                    let pw = admin::read_password(password_stdin)?;
                    admin::user_passwd(&cfg, &username, &pw)
                }
                UserCommand::Disable { username } => {
                    admin::user_set_disabled(&cfg, &username, true)
                }
                UserCommand::Enable { username } => {
                    admin::user_set_disabled(&cfg, &username, false)
                }
                UserCommand::Delete { username } => admin::user_delete(&cfg, &username),
            }
        }
        Command::Device { command } => {
            let cfg = load(&cli.config)?;
            match command {
                DeviceCommand::List { user } => admin::device_list(&cfg, user.as_deref()),
                DeviceCommand::Remove { device_id } => admin::device_remove(&cfg, &device_id),
            }
        }
        Command::Session { command } => {
            let cfg = load(&cli.config)?;
            match command {
                SessionCommand::List => admin::session_list(&cfg),
            }
        }
        Command::Friend { command } => {
            let cfg = load(&cli.config)?;
            match command {
                FriendCommand::List { username } => admin::friend_list(&cfg, &username),
            }
        }
    }
}
