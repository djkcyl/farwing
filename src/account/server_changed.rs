//! `server.changed` on the presence connection: the server's address or
//! certificate changed. The saved setting follows once the new address
//! answers; credentials stay bound to the server.
use crate::account::{api::ServerApi, server::ServerConfig};
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
                match follow(&changed).await {
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

async fn follow(changed: &ServerChanged) -> Result<Option<ServerConfig>> {
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
    let mut tries = 1;
    while let Err(error) = api.server_info().await {
        if tries == TRIES {
            return Err(error);
        }
        tries += 1;
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
