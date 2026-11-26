use super::{
    ConnectionProgressReporter, ConnectionSummary, ControllerConnection, cancellable,
    connection::ResolvedConnection, report_progress,
};
use crate::account::api::{ApiFailure, AssistJoined, ErrorCode};
use crate::account::assist::{AssistRequest, is_short_number};
use crate::account::client::AuthenticatedClient;
use crate::i18n::t;
use crate::media::{ConnectionMediaOptions, LocalDisplayInfo};
use anyhow::{Context as _, Result, anyhow, bail};
use farwing_protocol::crypto;
use std::{sync::Arc, time::Duration};
use tokio_util::sync::CancellationToken;

pub(super) struct AssistConnection {
    request: AssistRequest,
    room: Option<AssistJoined>,
    /// The code was saved after the first successful connection.
    remembered: bool,
}
impl AssistConnection {
    pub(super) async fn join(
        &mut self,
        client: &AuthenticatedClient,
        controller_id: &str,
        visit: &str,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
    ) -> Result<AssistJoined> {
        let mut reply = if let Some(previous) = &self.room {
            report_progress(reporter, 3, t().session.progress_resuming, "");
            // Rebuilding keeps the verified target; only the single-use
            // realtime ticket of the same server session is renewed.
            let session_id = previous
                .ticket
                .session_id
                .clone()
                .context(t().session.error_cannot_resume)?;
            match cancellable(cancel, client.assist_refetch(&session_id)).await {
                Ok(ticket) => {
                    let resumed = AssistJoined { ticket, ..previous.clone() };
                    check_target(&self.request, &resumed, controller_id)?;
                    resumed
                }
                // The server session ended with the host (restart, sleep):
                // ask the same verified device again with this session's code.
                Err(error) if session_ended(&error) => {
                    tracing::info!(%error, "assistance session ended; authorizing again");
                    self.request.pending = None;
                    self.request
                        .expected_publisher_id
                        .get_or_insert_with(|| previous.host.device_id.clone());
                    authorize(client, &self.request, controller_id, visit, reporter, cancel).await?
                }
                Err(error) => return Err(error),
            }
        } else {
            authorize(client, &self.request, controller_id, visit, reporter, cancel).await?
        };
        if reply.host.alias.trim().is_empty() {
            reply.host.alias = target_label(&self.request.target);
        }
        report_progress(
            reporter,
            3,
            t().session.progress_allowed,
            format!("{} · {}", reply.host.alias, reply.host.version),
        );
        self.room = Some(reply.clone());
        Ok(reply)
    }

    pub(super) async fn remember_success(&mut self, client: &AuthenticatedClient) -> Result<()> {
        // The code stays with the session: recovery may need to authorize again.
        if client.is_guest() || self.remembered || !self.request.has_code() {
            return Ok(());
        }
        let room = self.room.as_ref().context(t().session.error_no_connected_session)?;
        client
            .remember_assist_code(
                self.request.target.clone(),
                room.host.device_id.clone(),
                self.request.connect_code.clone(),
            )
            .await?;
        self.remembered = true;
        Ok(())
    }
}

fn check_target(request: &AssistRequest, reply: &AssistJoined, controller_id: &str) -> Result<()> {
    reply.validate(controller_id)?;
    crate::transport::signal::identity::check_key(&reply.host.device_id, &reply.host.public_key)?;
    anyhow::ensure!(
        (is_short_number(&request.target) || request.target == reply.host.device_id)
            && request.expected_publisher_id.as_deref()
                .is_none_or(|id| id == reply.host.device_id),
        "{}",
        t().session.error_entry_identity_changed
    );
    Ok(())
}

/// How a host reached through the server is called before its name is known.
pub(crate) fn target_label(target: &str) -> String {
    if is_short_number(target) {
        (t().session.target_short_number)(target)
    } else {
        (t().session.target_device)(&crate::account::api::device_id_text(target))
    }
}

/// The server no longer knows the assistance session.
fn session_ended(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<ApiFailure>()
            .is_some_and(|failure| failure.code == ErrorCode::NotFound)
    })
}

pub(super) async fn resolve(
    client: Arc<AuthenticatedClient>,
    alias: &str,
    options: ConnectionMediaOptions,
    request: AssistRequest,
    reporter: Option<&ConnectionProgressReporter>,
) -> Result<ResolvedConnection> {
    request.validate()?;
    report_progress(reporter, 1, t().session.progress_reading_local, "");
    if client.is_guest() {
        // A device that never met this server registers on its first
        // request; its device ID is read only after that.
        client.ready().await?;
    } else {
        client.account_info().await?;
    }
    let controller_device_id = client.device_id();
    crate::account::api::validate_device_id(&controller_device_id)?;
    #[cfg(windows)]
    let display = crate::media::detect_local_display().unwrap_or_else(|error| {
        tracing::warn!(%error, "local display not detected; using 1080p60");
        LocalDisplayInfo::FALLBACK
    });
    #[cfg(target_os = "linux")]
    let display = LocalDisplayInfo::FALLBACK;
    let profile = options.resolve(display)?;
    Ok(ResolvedConnection {
        client,
        visit: uuid::Uuid::new_v4().simple().to_string(),
        target_device_id: String::new(),
        controller_device_id,
        profile,
        default_codec: profile.codec,
        transport: options.transport,
        summary: ConnectionSummary {
            alias: alias.into(),
        },
        assist: Some(AssistConnection {
            request,
            room: None,
            remembered: false,
        }),
        preferences: None,
        audio_preferences: None,
        target_platform: farwing_protocol::Platform::Unknown,
        #[cfg(windows)]
        refresh_after_upgrade: false,
        #[cfg(windows)]
        background: None,
    })
}

#[cfg(target_os = "linux")]
impl ControllerConnection {
    /// Open an authorised assistance session for the Linux viewer.
    pub(crate) async fn connect_assistance(
        client: Arc<AuthenticatedClient>,
        mut request: AssistRequest,
        options: ConnectionMediaOptions,
        visit: &str,
        record: &super::record::VisitRecord,
        reporter: Option<&ConnectionProgressReporter>,
        cancel: &CancellationToken,
    ) -> Result<(Self, String)> {
        if !client.is_guest() {
            let (code, publisher_id) = cancellable(
                cancel,
                client.resolve_assist_code(
                    request.target.clone(),
                    std::mem::take(&mut request.connect_code),
                    request.expected_publisher_id.take(),
                ),
            )
            .await?;
            request.connect_code = code;
            request.expected_publisher_id = publisher_id;
        }
        let label = target_label(&request.target);
        let mut resolved = cancellable(
            cancel,
            resolve(Arc::clone(&client), &label, options, request, reporter),
        )
        .await?;
        resolved.visit = visit.to_owned();
        let mut retries = 0;
        let connection = resolved.connect(reporter, cancel, &mut retries).await?;
        if !cancel.is_cancelled()
            && let Some(assist) = &mut resolved.assist
            && let Err(error) = assist.remember_success(&client).await
        {
            tracing::warn!(%error, "connected assistance code was not saved");
        }
        record.bind(
            Some(client),
            &resolved.visit,
            &resolved.target_device_id,
            &resolved.controller_device_id,
            None,
        );
        Ok((connection, resolved.summary.alias))
    }
}
async fn authorize(
    client: &AuthenticatedClient,
    request: &AssistRequest,
    controller_id: &str,
    visit: &str,
    reporter: Option<&ConnectionProgressReporter>,
    cancel: &CancellationToken,
) -> Result<AssistJoined> {
    let mut dispatched = false;
    let mut pending_request = String::new();
    let result = async {
        // The controller half of the PAKE: one instance per host code slot,
        // all derived from the typed code. The code itself never leaves.
        let (pending, states) = match &request.pending {
            Some(pending) => (pending.clone(), Vec::new()),
            None => {
                report_progress(reporter, 2, t().session.progress_requesting, "");
                let (states, messages): (Vec<_>, Vec<_>) = if request.has_code() {
                    (0..crypto::ASSIST_PAKE_SLOTS)
                        .map(|_| crypto::pake_start(&request.connect_code))
                        .unzip()
                } else {
                    Default::default()
                };
                dispatched = true;
                let pending =
                    cancellable(cancel, client.assist_join(&request.target, messages)).await?;
                (pending, states)
            }
        };
        dispatched = true;
        pending_request = pending.request_id.clone();
        let key = if pending.trusted {
            // The host trusts this device: no code, no confirmation.
            None
        } else if pending.mode.needs_password() {
            if states.is_empty() {
                bail!(ApiFailure::new(ErrorCode::AssistCodeRequired, "access code required"));
            }
            let key = states
                .into_iter()
                .zip(pending.pake.iter().zip(&pending.key_confirm))
                .filter_map(|(state, (message, confirm))| {
                    let key = state.finish(message).ok()?;
                    crypto::key_confirm_matches(&key, "host", &pending.request_id, confirm)
                        .then_some(key)
                })
                .next();
            let Some(key) = key else {
                // Wrong code: detected locally, nothing about it was sent.
                if !client.is_guest()
                    && let Err(error) = client
                        .reject_assist_code(&request.target, &request.connect_code)
                        .await
                {
                    tracing::warn!(%error, "could not clear rejected access code");
                }
                bail!(ApiFailure::new(ErrorCode::AssistCodeInvalid, "wrong access code"));
            };
            Some(key)
        } else {
            None
        };
        let confirming = !pending.trusted && pending.mode.needs_confirmation();
        report_progress(
            reporter,
            3,
            if pending.trusted {
                t().session.progress_trusted
            } else if confirming {
                t().session.progress_waiting_confirmation
            } else {
                t().session.progress_code_accepted
            },
            "",
        );
        let confirm = key
            .map(|key| crypto::key_confirm(&key, "controller", &pending.request_id))
            .unwrap_or_default();
        let mut joined =
            cancellable(cancel, client.assist_accept(&pending.request_id, confirm, visit)).await?;
        check_target(request, &joined, controller_id)?;
        joined.pake_key = key;
        // Listed under "最近与收藏", and reached without a code next time
        // when the host trusts this device.
        crate::transport::direct::store::remember_host(crate::transport::direct::store::KnownHost {
            device_id: joined.host.device_id.clone(),
            public_key: joined.host.public_key.clone(),
            name: joined.host.alias.clone(),
            address: String::new(),
            connect_id: if is_short_number(&request.target) {
                request.target.clone()
            } else {
                String::new()
            },
            trusted: pending.trusted,
            last_seen: 0,
        });
        Ok(joined)
    }
    .await
    .map_err(|error| match crate::account::api::failure_code(&error) {
        // A specific reason (busy, locked, pairing declined…) says more.
        _ if error
            .downcast_ref::<ApiFailure>()
            .is_some_and(|failure| failure.reason.is_some()) =>
        {
            error
        }
        Some(ErrorCode::AssistCodeInvalid) => anyhow!("{}", t().session.error_code_invalid),
        Some(ErrorCode::AssistCodeRequired) => anyhow!("{}", t().session.error_code_required),
        Some(ErrorCode::AssistRejected) => anyhow!("{}", t().session.error_connection_declined),
        Some(ErrorCode::AssistUnavailable) => anyhow!("{}", t().session.error_assist_unavailable),
        Some(ErrorCode::Timeout) => anyhow!("{}", t().session.error_confirmation_timeout),
        _ => error,
    });
    if dispatched && !pending_request.is_empty() && (cancel.is_cancelled() || result.is_err()) {
        // Withdraw only the request started by this viewer. Never log out or
        // modify the target's allow-assistance settings.
        let _ = tokio::time::timeout(
            Duration::from_secs(5),
            client.assist_cancel(&pending_request),
        )
        .await;
    }
    result
}
