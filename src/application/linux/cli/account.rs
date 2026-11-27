use anyhow::{Context, Result};
use clap::Subcommand;
use farwing_protocol::{api::FriendRequestOutcome, caps};

use crate::account::api::{DeviceInfo, parse_device_id, platform_label};
use crate::account::client::AuthenticatedClient;
use crate::i18n::t;

#[derive(Subcommand)]
pub(in super::super) enum FriendAction {
    /// List friends and pending requests.
    List,
    /// Send a request by username (an existing incoming request becomes a friendship).
    Add { username: String },
    /// Accept an incoming request with the default friend grants.
    Accept { request_id: String },
    /// Reject an incoming request.
    Reject { request_id: String },
    /// Cancel an outgoing request.
    Cancel { request_id: String },
    /// Remove a friend and revoke grants between the two accounts.
    Remove { user_id: String },
}

#[derive(Subcommand)]
pub(in super::super) enum GrantAction {
    /// Show the default and per-device capabilities granted to this friend.
    Show { user_id: String },
    /// Change the capabilities granted to this friend. An empty list grants nothing.
    Set {
        user_id: String,
        /// Default capabilities, comma-separated (for example: view,control,clipboard).
        #[arg(long)]
        default: Option<String>,
        /// Device-specific capabilities: DEVICE_ID=CAPABILITIES. May be repeated.
        #[arg(long = "device")]
        devices: Vec<String>,
        /// Remove a device override so the default applies. May be repeated.
        #[arg(long = "clear-device")]
        clear: Vec<String>,
    },
}

pub(super) async fn with_client<T>(
    body: impl AsyncFnOnce(&AuthenticatedClient) -> Result<T>,
) -> Result<T> {
    let client = AuthenticatedClient::from_saved_session()?;
    let result = body(&client).await;
    client.close().await;
    result
}

pub(super) async fn logout() -> Result<()> {
    let client = AuthenticatedClient::from_saved_session()?;
    let outcome = client.logout().await;
    if let Some(error) = outcome.local_error {
        anyhow::bail!("Could not delete local sign-in credentials: {error}");
    }
    if let Some(error) = outcome.remote_error {
        println!("Local sign-in cleared; the server did not confirm sign-out: {error}");
    } else {
        println!("Signed out");
    }
    Ok(())
}

pub(super) async fn devices(client: &AuthenticatedClient) -> Result<()> {
    let list = client.list_devices().await?;
    println!("This device:");
    print_device(&list.current_device);
    println!("\nMy devices:");
    if list.devices.is_empty() {
        println!("  (none)");
    }
    for device in &list.devices {
        print_device(device);
    }
    if !list.shared.is_empty() {
        println!("\nFriends' shared devices:");
        for device in &list.shared {
            print_device(device);
            println!(
                "    Owner: {}; grants: {}",
                device
                    .owner
                    .as_ref()
                    .map_or("unknown", |owner| owner.display_name()),
                caps_line(&device.granted),
            );
        }
    }
    Ok(())
}

fn print_device(device: &DeviceInfo) {
    let controllable = device.controlled_support && device.controllable;
    println!(
        "  {} [{}] — {}, {}, controllable: {}, participants: {}",
        device.alias,
        device.device_id,
        device.status_label(),
        device.platform_label(),
        if controllable { "yes" } else { "no" },
        device.participant_count(),
    );
}

pub(super) async fn friend(client: &AuthenticatedClient, action: FriendAction) -> Result<()> {
    match action {
        FriendAction::List => {
            let (friends, requests) = tokio::try_join!(client.friends(), client.friend_requests())?;
            println!("Friends:");
            if friends.friends.is_empty() {
                println!("  (none)");
            }
            for friend in &friends.friends {
                println!("  {}", user_line(&friend.user));
            }
            if !requests.incoming.is_empty() {
                println!("\nIncoming requests:");
                for request in &requests.incoming {
                    println!("  {} — {}", request.id, user_line(&request.user));
                }
            }
            if !requests.outgoing.is_empty() {
                println!("\nOutgoing requests:");
                for request in &requests.outgoing {
                    println!("  {} — {}", request.id, user_line(&request.user));
                }
            }
        }
        FriendAction::Add { username } => match client.send_friend_request(&username).await? {
            FriendRequestOutcome::Sent { request } => println!("Sent friend request {}", request.id),
            FriendRequestOutcome::Accepted { friend } => {
                println!("New friend: {}", user_line(&friend.user));
            }
        },
        FriendAction::Accept { request_id } => {
            client
                .answer_friend_request(&request_id, Some(caps::normalize(caps::DEFAULT_FRIEND)))
                .await?;
            println!("Accepted friend request");
        }
        FriendAction::Reject { request_id } => {
            client.answer_friend_request(&request_id, None).await?;
            println!("Rejected friend request");
        }
        FriendAction::Cancel { request_id } => {
            client.cancel_friend_request(&request_id).await?;
            println!("Cancelled friend request");
        }
        FriendAction::Remove { user_id } => {
            client.remove_friend(&user_id).await?;
            println!("Removed friend");
        }
    }
    Ok(())
}

fn user_line(user: &farwing_protocol::api::UserSummary) -> String {
    if user.nickname.is_empty() || user.nickname == user.username {
        format!("{} [{}]", user.username, user.user_id)
    } else {
        format!("{} ({}) [{}]", user.nickname, user.username, user.user_id)
    }
}

pub(super) async fn grant(client: &AuthenticatedClient, action: GrantAction) -> Result<()> {
    let grants = match action {
        GrantAction::Show { user_id } => client.grants(&user_id).await?,
        GrantAction::Set {
            user_id,
            default,
            devices,
            clear,
        } => {
            let mut grants = client.grants(&user_id).await?;
            if let Some(default) = default {
                grants.default = parse_caps(&default)?;
            }
            for entry in devices {
                let (id, capabilities) = entry
                    .split_once('=')
                    .context("Use DEVICE_ID=CAPABILITIES for --device")?;
                let id = parse_device_id(id.trim()).context(t().center.unrecognized_target)?;
                grants.devices.insert(id, parse_caps(capabilities)?);
            }
            for id in clear {
                let id = parse_device_id(id.trim()).context(t().center.unrecognized_target)?;
                grants.devices.remove(&id);
            }
            client.set_grants(&user_id, grants).await?;
            println!("Saved grants");
            client.grants(&user_id).await?
        }
    };
    println!("Default: {}", caps_line(&grants.default));
    for (id, capabilities) in &grants.devices {
        println!("Device {id}: {}", caps_line(capabilities));
    }
    Ok(())
}

fn caps_line(capabilities: &[String]) -> String {
    if capabilities.is_empty() {
        "(none)".into()
    } else {
        capabilities.join(",")
    }
}

fn parse_caps(text: &str) -> Result<Vec<String>> {
    let capabilities: Vec<_> = text
        .split(',')
        .map(str::trim)
        .filter(|cap| !cap.is_empty())
        .collect();
    if let Some(unknown) = capabilities.iter().find(|cap| !caps::is_known(cap)) {
        anyhow::bail!(
            "Unknown capability `{unknown}`; choose from {}",
            caps::ALL.join(",")
        );
    }
    Ok(caps::normalize(&capabilities))
}

pub(super) async fn sessions(client: &AuthenticatedClient, revoke: Option<String>) -> Result<()> {
    if let Some(id) = revoke {
        let id = parse_device_id(id.trim()).context(t().center.unrecognized_target)?;
        client.revoke_auth_session(&id).await?;
        println!("Revoked sign-in on device {id}");
    }
    for session in client.auth_sessions().await?.sessions {
        println!(
            "{} [{}] {}{}",
            session.device_alias,
            session.device_id,
            platform_label(session.platform),
            if session.current { " (this device)" } else { "" },
        );
    }
    Ok(())
}
