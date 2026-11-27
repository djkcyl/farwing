//! Entry points for server-assisted and direct connections from the Linux client.
use crate::account::{
    api::{DeviceInfo, device_id_text, parse_device_id},
    assist::{AssistRequest, is_short_number, normalize_connect_id},
    client::{AuthenticatedClient, NoSavedSession},
    server::ServerConfig,
};
use crate::i18n::t;
use crate::media::ConnectionMediaOptions;
use crate::session::controller::{ConnectionProgressReporter, ControllerConnection, direct};
use crate::transport::direct::Target;
use anyhow::{Result, ensure};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[path = "joins/ui.rs"]
mod ui;
pub(super) use ui::JoinUi;

pub(super) enum JoinAction {
    Account(DeviceInfo),
    Join(JoinRequest),
}

enum ConnectTarget {
    ShortNumber(String),
    DeviceId(String),
    Address(Target),
}

impl ConnectTarget {
    fn parse(text: &str) -> Result<Self> {
        ensure!(!text.trim().is_empty(), "{}", t().center.enter_target);
        if let Ok(number) = normalize_connect_id(text) {
            return Ok(Self::ShortNumber(number));
        }
        if let Some(id) = parse_device_id(text) {
            return Ok(Self::DeviceId(id));
        }
        Target::parse(text)
            .map(Self::Address)
            .map_err(|_| anyhow::anyhow!("{}", t().center.unrecognized_target))
    }
}

#[derive(Clone)]
pub(super) struct JoinRequest(Destination);

#[derive(Clone)]
enum Destination {
    Assist(AssistRequest, AssistAccount),
    Direct(direct::DirectRequest),
}

#[derive(Clone)]
enum AssistAccount {
    /// The device center already owns a signed-in account.
    SignedIn,
    /// For the CLI: use the saved account if present, otherwise a guest token.
    SavedOrGuest,
    /// Never touch the saved login. Configure the first server only if provided.
    Guest(Option<ServerConfig>),
}

impl JoinRequest {
    fn assist(
        target: &str,
        code: String,
        account: AssistAccount,
        publisher_id: Option<&str>,
    ) -> Result<Self> {
        let mut request = AssistRequest::new(target, code)?;
        request.expected_publisher_id = publisher_id.map(str::to_owned);
        Ok(Self(Destination::Assist(request, account)))
    }

    fn direct(
        address: &str,
        code: Option<String>,
        ask: bool,
        expected_id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Self> {
        let mut request = direct::DirectRequest::parse(address, code, ask)?;
        if let Some(expected_id) = expected_id {
            request = request.expecting(&device_id(expected_id)?);
        }
        if let Some(name) = name {
            request = request.named(name);
        }
        Ok(Self(Destination::Direct(request)))
    }

    pub(super) fn title(&self) -> String {
        match &self.0 {
            Destination::Assist(request, _) if is_short_number(&request.target) => {
                (t().session.target_short_number)(&request.target)
            }
            Destination::Assist(request, _) => {
                (t().session.target_device)(&device_id_text(&request.target))
            }
            Destination::Direct(request) => request.title(),
        }
    }

    pub(super) async fn connect(
        self,
        client: Option<Arc<AuthenticatedClient>>,
        options: ConnectionMediaOptions,
        visit: &str,
        record: &crate::session::controller::VisitRecord,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
    ) -> Result<(ControllerConnection, String)> {
        let (request, account) = match self.0 {
            Destination::Assist(request, account) => (request, account),
            Destination::Direct(request) => {
                return direct::connect_viewer(&request, options, visit, record, reporter, cancel)
                    .await;
            }
        };
        ensure!(!cancel.is_cancelled(), "connection cancelled");
        let owned = !matches!(account, AssistAccount::SignedIn);
        let client = match account {
            AssistAccount::SignedIn => {
                client.ok_or_else(|| anyhow::anyhow!("{}", t().session.account_session_ended))?
            }
            AssistAccount::SavedOrGuest => {
                Arc::new(match AuthenticatedClient::from_saved_session() {
                    Ok(client) => client,
                    Err(error) if error.is::<NoSavedSession>() => {
                        AuthenticatedClient::guest_with_runtime()?
                    }
                    Err(error) => return Err(error),
                })
            }
            AssistAccount::Guest(server) => {
                if let Some(server) = server {
                    // Registration and the guest client must use the same profile
                    // config. Do not replace the trust settings of an existing server.
                    match ServerConfig::load()? {
                        None => server.save()?,
                        Some(configured) if configured == server => {}
                        Some(_) => anyhow::bail!("{}", t().center.change_server_title),
                    }
                }
                Arc::new(AuthenticatedClient::guest_with_runtime()?)
            }
        };
        let result = ControllerConnection::connect_assistance(
            Arc::clone(&client),
            request,
            options,
            visit,
            record,
            reporter,
            cancel,
        )
        .await;
        match result {
            Ok((mut connection, alias)) => {
                if owned {
                    connection.retain_linux_account(client);
                }
                Ok((connection, alias))
            }
            Err(error) => {
                if owned {
                    client.close().await;
                }
                Err(error)
            }
        }
    }
}

fn device_id(text: &str) -> Result<String> {
    parse_device_id(text).ok_or_else(|| anyhow::anyhow!("{}", t().center.unrecognized_target))
}

#[derive(clap::Subcommand)]
pub(super) enum Command {
    /// Open a viewing window using a short number or device ID through the configured server.
    Assist {
        target: String,
        /// The host's access code; omit if trusted or the host only requires approval.
        #[arg(long)]
        code: Option<String>,
        /// Use a guest token, even if a login is saved.
        #[arg(long)]
        guest: bool,
    },
    /// Open a viewing window directly, without using a Farwing server.
    Connect {
        /// Hostname or IP address, optionally with port. Bracket IPv6 with a port.
        #[arg(long)]
        direct: String,
        /// The host's access code.
        #[arg(long, conflicts_with = "ask")]
        code: Option<String>,
        /// Ask the host user to confirm instead of using an access code.
        #[arg(long)]
        ask: bool,
        /// Connect only if the host proves it owns this device ID.
        #[arg(long)]
        device_id: Option<String>,
    },
}

impl Command {
    pub(super) fn into_request(self) -> Result<JoinRequest> {
        match self {
            Self::Assist {
                target,
                code,
                guest,
            } => {
                let account = if guest {
                    AssistAccount::Guest(None)
                } else {
                    AssistAccount::SavedOrGuest
                };
                JoinRequest::assist(&target, code.unwrap_or_default(), account, None)
            }
            Self::Connect {
                direct,
                code,
                ask,
                device_id,
            } => JoinRequest::direct(&direct, code, ask, device_id.as_deref(), None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(subcommand)]
        command: Command,
    }

    #[test]
    fn unified_target_prefers_server_identifiers_over_hostnames() {
        // Both unformatted identifiers would also parse as direct hostnames.
        assert!(matches!(
            ConnectTarget::parse("123456789").unwrap(),
            ConnectTarget::ShortNumber(number) if number == "123456789"
        ));
        assert!(matches!(
            ConnectTarget::parse("ABCD-EFGH-IJKL-MNOP").unwrap(),
            ConnectTarget::DeviceId(id) if id == "abcdefghijklmnop"
        ));
        assert!(matches!(
            ConnectTarget::parse("[::1]:9000").unwrap(),
            ConnectTarget::Address(address) if address.to_string() == "[::1]:9000"
        ));
        assert!(ConnectTarget::parse("https://example.org").is_err());
    }

    #[test]
    fn direct_cli_keeps_approval_and_the_pinned_target() {
        let cli = Cli::try_parse_from([
            "farwing",
            "connect",
            "--direct",
            "[::1]:9000",
            "--ask",
            "--device-id",
            "ABCD-EFGH-IJKL-MNOP",
        ])
        .unwrap();
        let JoinRequest(Destination::Direct(request)) = cli.command.into_request().unwrap() else {
            panic!("direct connection was routed through the server");
        };
        assert_eq!(request.label(), "[::1]:9000");
        assert_eq!(request.expected.as_deref(), Some("abcdefghijklmnop"));
        assert!(matches!(
            request.auth,
            crate::transport::direct::client::Auth::Code(None)
        ));
        assert!(
            Cli::try_parse_from([
                "farwing",
                "connect",
                "--direct",
                "localhost",
                "--ask",
                "--code",
                "123456",
            ])
            .is_err()
        );
    }

    #[test]
    fn guest_cli_does_not_use_the_saved_account() {
        let cli = Cli::try_parse_from([
            "farwing",
            "assist",
            "123-456-789",
            "--guest",
            "--code",
            "123456",
        ])
        .unwrap();
        let JoinRequest(Destination::Assist(request, AssistAccount::Guest(None))) =
            cli.command.into_request().unwrap()
        else {
            panic!("explicit guest request required a saved account");
        };
        assert_eq!(request.target, "123456789");
        assert_eq!(request.connect_code, "123456");
    }
}
