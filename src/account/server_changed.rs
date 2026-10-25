//! `server.changed` on the presence connection: the server's address or
//! certificate changed. The saved setting follows once the new address
//! answers; credentials stay bound to the server. An installation that was
//! away follows the realtime endpoint the server hands out instead.
use crate::account::{
    api::{RealtimeTicket, ServerApi},
    server::ServerConfig,
};
use anyhow::Result;
use farwing_protocol::realtime::{Envelope, ServerChanged, types};
use std::time::Duration;

/// Tries of the new address (DNS or a port mapping may need a moment).
const TRIES: u32 = 6;
const TRY_AGAIN: Duration = Duration::from_secs(10);

/// Handle `push` if it is `server.changed`.
pub(crate) fn push(push: &Envelope) -> bool {
    if push.kind != types::SERVER_CHANGED {
        return false;
    }
    match push.body::<ServerChanged>() {
        Ok(changed) => {
            tokio::spawn(async move {
                match follow(&changed, TRIES).await {
                    Ok(Some(server)) => tracing::info!(url = server.origin(),
                        pinned = server.pinned_fingerprint().is_some(), "server setting follows the server"),
                    Ok(None) => tracing::debug!(url = %changed.url, "server change does not concern this setting"),
                    Err(error) => tracing::warn!(url = %changed.url, error = %format!("{error:#}"),
                        "server setting not changed"),
                }
            });
        }
        Err(_) => tracing::warn!("discarded malformed server push"),
    }
    true
}

/// The realtime endpoint `ticket` (from the saved server `server`) names
/// another host: the server's public address changed while this
/// installation was away. The setting follows once the new address
/// answers.
pub(crate) async fn follow_endpoint(server: &ServerConfig, ticket: &RealtimeTicket) -> Option<ServerConfig> {
    let endpoint = url::Url::parse(ticket.endpoints().first()?).ok()?;
    if server.accepts_endpoint(&endpoint) {
        return None;
    }
    let host = endpoint.host_str()?;
    let url = match endpoint.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    };
    let changed = ServerChanged {
        url,
        previous: server.origin().to_owned(),
        sha256: None,
    };
    match follow(&changed, 1).await {
        Ok(Some(next)) => {
            tracing::info!(url = next.origin(), "server setting follows the server's new address");
        }
        Ok(None) => {}
        Err(error) => {
            tracing::warn!(url = %changed.url, error = %format!("{error:#}"), "the server's new address does not answer");
        }
    }
    // Followed here, or by another connection of this process meanwhile.
    let current = ServerConfig::load().ok().flatten()?;
    current.accepts_endpoint(&endpoint).then_some(current)
}

/// Follow `changed`, trying the new address `tries` times.
async fn follow(changed: &ServerChanged, tries: u32) -> Result<Option<ServerConfig>> {
    if ServerConfig::overridden() {
        return Ok(None);
    }
    let Some(saved) = ServerConfig::load()? else {
        return Ok(None);
    };
    let Some(next) = saved.follow(changed)? else {
        return Ok(None);
    };
    let api = ServerApi::new(&next)?;
    let mut tried = 1;
    while let Err(error) = api.server_info().await {
        if tried >= tries {
            return Err(error);
        }
        tried += 1;
        tokio::time::sleep(TRY_AGAIN).await;
    }
    // Another change came meanwhile.
    if ServerConfig::load()?.as_ref() != Some(&saved) {
        return Ok(None);
    }
    next.save()?;
    next.followed();
    Ok(Some(next))
}
