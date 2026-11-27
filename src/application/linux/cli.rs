//! Linux controller commands. Only a viewing command opens the device center.
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Args, Subcommand};

use crate::account::api::{DeviceInfo, ServerApi, parse_device_id};
use crate::account::client::AuthenticatedClient;
use crate::account::login::{LoginOutcome, interactive_login};
use crate::account::server::{ServerConfig, probe_certificate};
use crate::i18n::t;
use crate::media::{CodecPreference, ConnectionMediaOptions, FrameRateChoice, TransportChoice};
use crate::transport::direct::{self, lan};

use super::joins;

#[path = "cli/account.rs"]
mod account;

#[derive(Subcommand)]
pub(super) enum Command {
    /// Configure or inspect this profile's Farwing Server.
    Server {
        #[command(subcommand)]
        action: ServerAction,
    },
    /// Sign in to a Farwing Server account.
    Login {
        /// Server address; omit to use the configured server and certificate trust.
        #[arg(long)]
        server: Option<String>,
        /// Account username; omit for a prompt.
        #[arg(long)]
        username: Option<String>,
        /// Read the password (one line) from redirected standard input.
        #[arg(long)]
        password_stdin: bool,
    },
    /// Sign out and remove this device from the account.
    Logout,
    /// List this device, other devices in the account, and friends' shared devices.
    Devices,
    /// Show this profile's device ID and public key.
    Id,
    /// Discover Farwing hosts on the local network.
    Lan {
        /// How long to wait for replies (1–30 seconds).
        #[arg(long, default_value_t = 2)]
        seconds: u64,
    },
    /// Manage friends and friend requests.
    Friend {
        #[command(subcommand)]
        action: account::FriendAction,
    },
    /// Inspect or change what this account grants a friend.
    Grant {
        #[command(subcommand)]
        action: account::GrantAction,
    },
    /// List the account's logins or revoke another device's login.
    Sessions {
        /// Revoke the login on this device ID.
        #[arg(long)]
        revoke: Option<String>,
    },
    /// Open a viewing window using a short number or device ID through the configured server.
    Assist {
        target: String,
        /// The host's access code; omit if trusted or the host only requires approval.
        #[arg(long)]
        code: Option<String>,
        /// Use a guest token, even if a login is saved.
        #[arg(long)]
        guest: bool,
        #[command(flatten)]
        media: MediaArgs,
    },
    /// Open a viewing window for an account device or connect directly to an address.
    Connect {
        /// Hostname or IP address, optionally with port. Bracket IPv6 with a port.
        #[arg(long, required_unless_present = "device_id")]
        direct: Option<String>,
        /// Select an account device; with --direct, verify that the host owns this ID.
        #[arg(long)]
        device_id: Option<String>,
        /// The host's access code (direct connections only).
        #[arg(long, requires = "direct", conflicts_with = "ask")]
        code: Option<String>,
        /// Ask the host user to confirm instead of using an access code (direct only).
        #[arg(long, requires = "direct")]
        ask: bool,
        #[command(flatten)]
        media: MediaArgs,
    },
}

#[derive(Subcommand)]
pub(super) enum ServerAction {
    /// Save the address and optional certificate trust settings for this profile.
    Set {
        address: String,
        /// Pin the certificate's SHA-256 fingerprint after verifying it independently.
        #[arg(long)]
        pin: Option<String>,
        /// PEM file containing a custom CA certificate.
        #[arg(long)]
        ca: Option<PathBuf>,
    },
    /// Show the effective address and certificate trust settings.
    Show,
    /// Inspect the certificate fingerprint and server identity without saving.
    Probe { address: String },
}

#[derive(Args)]
pub(super) struct MediaArgs {
    /// Start playback muted on this machine.
    #[arg(long)]
    mute: bool,
    /// Listen without showing or controlling the remote picture.
    #[arg(long)]
    audio_only: bool,
    /// Start by viewing the remote camera instead of the desktop.
    #[arg(long, conflicts_with = "audio_only")]
    camera: bool,
    /// Stream frame rate: auto, 144, 90, 60, or 30.
    #[arg(long)]
    fps: Option<FrameRateChoice>,
    /// Video codec: auto, prefer-av1, prefer-h264, av1, or h264.
    #[arg(long)]
    codec: Option<CodecPreference>,
    /// Transport preference: auto, p2p, or relay (when available).
    #[arg(long)]
    transport: Option<TransportChoice>,
}

impl MediaArgs {
    fn options(self) -> Result<ConnectionMediaOptions> {
        let mut options = crate::media::preferences::load()?;
        options.muted = self.mute;
        options.audio_only = self.audio_only;
        options.camera_only = self.camera;
        if let Some(fps) = self.fps {
            options.frame_rate = fps;
        }
        if let Some(codec) = self.codec {
            options.codec = codec;
        }
        if let Some(transport) = self.transport {
            options.transport = transport;
        }
        anyhow::ensure!(
            !matches!(options.codec, CodecPreference::H265 | CodecPreference::PreferH265),
            "H.265 decoding is not supported on Linux; choose AV1, H.264 or auto"
        );
        options.validate()?;
        Ok(options)
    }
}

pub(super) enum Launch {
    Join {
        request: joins::JoinRequest,
        options: ConnectionMediaOptions,
    },
    Account {
        client: Arc<AuthenticatedClient>,
        device: DeviceInfo,
        options: ConnectionMediaOptions,
    },
}

pub(super) async fn run(command: Command) -> Result<Option<Launch>> {
    match command {
        Command::Server { action } => server(action).await?,
        Command::Login {
            server,
            username,
            password_stdin,
        } => {
            let outcome = interactive_login(server, username, password_stdin).await?;
            println!(
                "{}",
                match outcome {
                    LoginOutcome::New => "Signed in",
                    LoginOutcome::Restored => "Restored saved login",
                }
            );
        }
        Command::Logout => account::logout().await?,
        Command::Devices => account::with_client(account::devices).await?,
        Command::Id => {
            let device = direct::local_device()?;
            println!("Device name: {}", device.name);
            println!("Device ID: {}", device.identity.device_id);
            println!("Device public key: {}", device.identity.public_key());
        }
        Command::Lan { seconds } => discover(seconds).await?,
        Command::Friend { action } => {
            account::with_client(async move |client| account::friend(client, action).await).await?;
        }
        Command::Grant { action } => {
            account::with_client(async move |client| account::grant(client, action).await).await?;
        }
        Command::Sessions { revoke } => {
            account::with_client(async move |client| account::sessions(client, revoke).await).await?;
        }
        Command::Assist {
            target,
            code,
            guest,
            media,
        } => {
            let request = joins::Command::Assist {
                target,
                code,
                guest,
            }
            .into_request()?;
            return Ok(Some(Launch::Join {
                request,
                options: media.options()?,
            }));
        }
        Command::Connect {
            direct,
            device_id,
            code,
            ask,
            media,
        } => {
            let options = media.options()?;
            if let Some(direct) = direct {
                let request = joins::Command::Connect {
                    direct,
                    code,
                    ask,
                    device_id,
                }
                .into_request()?;
                return Ok(Some(Launch::Join { request, options }));
            }
            let device_id = device_id
                .as_deref()
                .and_then(parse_device_id)
                .context(t().center.unrecognized_target)?;
            let client = AuthenticatedClient::from_saved_session()?;
            let device = match account_device(&client, &device_id).await {
                Ok(device) => device,
                Err(error) => {
                    client.close().await;
                    return Err(error);
                }
            };
            return Ok(Some(Launch::Account {
                client: Arc::new(client),
                device,
                options,
            }));
        }
    }
    Ok(None)
}

async fn account_device(client: &AuthenticatedClient, device_id: &str) -> Result<DeviceInfo> {
    let list = client.list_devices().await?;
    anyhow::ensure!(
        list.current_device.device_id != device_id,
        "{}",
        t().session.error_connect_to_self
    );
    list.devices
        .into_iter()
        .chain(list.shared)
        .find(|device| device.device_id == device_id)
        .with_context(|| (t().center.device_not_found)(device_id))
}

async fn server(action: ServerAction) -> Result<()> {
    match action {
        ServerAction::Set { address, pin, ca } => {
            let ca = ca
                .map(|path| {
                    std::fs::read_to_string(&path)
                        .with_context(|| format!("Read CA file {}", path.display()))
                })
                .transpose()?;
            let config = ServerConfig::new(&address, pin.as_deref(), ca)?;
            config.save()?;
            println!("Saved server {}", config.origin());
            if let Some(pin) = config.pinned_fingerprint() {
                println!("Pinned certificate SHA-256: {pin}");
            }
        }
        ServerAction::Show => {
            if let Some(config) = ServerConfig::load()? {
                println!("Server: {}", config.origin());
                match config.pinned_fingerprint() {
                    Some(pin) => println!("Pinned certificate SHA-256: {pin}"),
                    None => println!("Certificate trust: system roots"),
                }
                if config.has_custom_ca() {
                    println!("Custom CA configured");
                }
            } else {
                println!("No server configured");
            }
        }
        ServerAction::Probe { address } => {
            let probe = probe_certificate(&address).await?;
            println!("Certificate SHA-256: {}", probe.sha256);
            println!(
                "Trusted by system: {}",
                if probe.trusted_by_system { "yes" } else { "no" }
            );
            let config = ServerConfig::new(&address, Some(&probe.sha256), None)?;
            let info = ServerApi::new(&config)?.server_info().await?;
            println!(
                "Server: {} {} (protocol {} v{}..=v{})",
                info.server.name,
                info.server.version,
                info.protocol.name,
                info.protocol.min,
                info.protocol.max
            );
            if !info.server.instance_name.is_empty() {
                println!("Instance: {}", info.server.instance_name);
            }
        }
    }
    Ok(())
}

async fn discover(seconds: u64) -> Result<()> {
    let found = lan::discover(Duration::from_secs(seconds.clamp(1, 30))).await?;
    if found.is_empty() {
        println!("No Farwing hosts found on the local network");
        return Ok(());
    }
    for host in found {
        let announce = &host.announce;
        println!(
            "{} [{}] {} — methods: {}",
            announce.name,
            announce.device_id,
            host.target(),
            announce.methods.join(",")
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::{Parser, error::ErrorKind};

    use super::*;
    use crate::application::Arguments;

    #[test]
    fn connect_parses_an_account_id_without_requiring_a_direct_address() {
        let args = Arguments::try_parse_from([
            "farwing",
            "connect",
            "--device-id",
            "abcdefghijklmnop",
            "--fps",
            "60",
            "--codec",
            "av1",
        ])
        .unwrap();
        let Some(Command::Connect {
            direct,
            device_id,
            media,
            ..
        }) = args.command
        else {
            panic!("expected a connection command");
        };
        assert!(direct.is_none());
        assert_eq!(device_id.as_deref(), Some("abcdefghijklmnop"));
        assert_eq!(media.fps, Some(FrameRateChoice::Fps60));
        assert_eq!(media.codec, Some(CodecPreference::Av1));
    }

    #[test]
    fn connect_preserves_a_direct_address_with_an_expected_id_and_approval() {
        let args = Arguments::try_parse_from([
            "farwing",
            "connect",
            "--direct",
            "[::1]:9000",
            "--device-id",
            "ABCD-EFGH-IJKL-MNOP",
            "--ask",
        ])
        .unwrap();
        let Some(Command::Connect {
            direct,
            device_id,
            ask,
            ..
        }) = args.command
        else {
            panic!("expected a connection command");
        };
        assert_eq!(direct.as_deref(), Some("[::1]:9000"));
        assert_eq!(device_id.as_deref(), Some("ABCD-EFGH-IJKL-MNOP"));
        assert!(ask);
    }

    #[test]
    fn connect_does_not_silently_ignore_a_direct_access_code_for_an_account_device() {
        let result = Arguments::try_parse_from([
            "farwing",
            "connect",
            "--device-id",
            "abcdefghijklmnop",
            "--code",
            "example",
        ]);
        let Err(error) = result else {
            panic!("expected a direct-address requirement");
        };
        assert_eq!(error.kind(), ErrorKind::MissingRequiredArgument);
        assert!(error.to_string().contains("--direct"));
    }
}
