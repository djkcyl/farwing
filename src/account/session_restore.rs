//! Saved-login restoration: device registration first, then the account check.
//! This validates the saved token; it is not a token refresh.
use std::time::Duration;

use crate::account::api::{ApiFailure, ErrorCode, ServerApi};
use crate::session::device_session::DeviceHandle;
use anyhow::Result;

#[derive(Clone, Copy, Debug)]
pub(crate) enum RestoreTrigger {
    DeviceStartup,
    ExplicitLogin,
}

pub(crate) async fn initialize(
    device: &DeviceHandle,
    trigger: RestoreTrigger,
) -> Result<crate::account::auth::NativeIdentity> {
    let mut long_retries = 0u32;
    loop {
        let explicit = matches!(trigger, RestoreTrigger::ExplicitLogin);
        match device.ensure(explicit || long_retries != 0).await {
            Ok(identity) => return Ok(identity),
            Err(error) => {
                let Some(failure) = error.downcast_ref::<ApiFailure>() else {
                    return Err(error);
                };
                let code = failure.code;
                // Only an unreachable server is retried here. A rejected
                // registration cannot be repaired by repeating it.
                if code != ErrorCode::Network && !code.retryable() {
                    return Err(error);
                }
                if long_retries != 0 && code != ErrorCode::Network {
                    return Err(error);
                }
                let delay = if explicit {
                    Duration::from_secs(if long_retries < 30 { 10 } else { 30 })
                } else {
                    device.startup_retry_delay().await?
                };
                long_retries = long_retries.saturating_add(1);
                tracing::warn!(?trigger, code = %code, long_retries, seconds = delay.as_secs(), %error, "device initialization retry scheduled");
                tokio::time::sleep(delay).await;
            }
        }
    }
}

pub(crate) async fn restore_user(
    api: &ServerApi,
    trigger: RestoreTrigger,
) -> Result<farwing_protocol::api::Me, ApiFailure> {
    let retries = match trigger {
        RestoreTrigger::DeviceStartup => 10,
        RestoreTrigger::ExplicitLogin => 5,
    };
    for attempt in 0..=retries {
        let failure = match api.me().await {
            Ok(me) => {
                tracing::info!(?trigger, attempt = attempt + 1, "saved account restored");
                return Ok(me);
            }
            Err(error) => match error.downcast::<ApiFailure>() {
                Ok(failure) => failure,
                Err(error) => ApiFailure::new(ErrorCode::Network, format!("{error:#}")),
            },
        };
        if failure.invalid_saved_credentials() || attempt == retries {
            return Err(failure);
        }
        tracing::warn!(
            ?trigger,
            attempt = attempt + 1,
            code = %failure.code,
            "saved account restoration failed; retrying in 1000 ms"
        );
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
    unreachable!("all restoration attempts return success or the final error")
}
