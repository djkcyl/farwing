use super::*;
use crate::account::api::{ApiFailure, AssistJoined, ErrorCode};
use farwing_protocol::crypto;
use crate::account::assist::AssistRequest;
use crate::i18n::t;

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
                Ok(ticket) => AssistJoined {
                    ticket,
                    ..previous.clone()
                },
                // The server session ended with the host (restart, sleep):
                // ask the same verified device again with this session's code.
                Err(error) if session_ended(&error) => {
                    tracing::info!(%error, "assistance session ended; authorizing again");
                    self.request.pending = None;
                    self.request
                        .expected_publisher_id
                        .get_or_insert_with(|| previous.host.device_id.clone());
                    authorize(client, &self.request, reporter, cancel).await?
                }
                Err(error) => return Err(error),
            }
        } else {
            authorize(client, &self.request, reporter, cancel).await?
        };
        reply.validate(controller_id)?;
        if self
            .request
            .expected_publisher_id
            .as_ref()
            .is_some_and(|id| id != &reply.host.device_id)
        {
            bail!("{}", t().session.error_entry_identity_changed);
        }
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
        if self.remembered || !self.request.has_code() {
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

/// How a host reached through the server is called before its name is known.
pub(crate) fn target_label(target: &str) -> String {
    if crate::account::assist::is_short_number(target) {
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
    if !client.is_guest() {
        client.account_info().await?;
    }
    let controller_device_id = client.device_id();
    crate::account::api::validate_device_id(&controller_device_id)?;
    let (display, warning) = match detect_local_display() {
        Ok(display) => (display, None),
        Err(error) => (
            LocalDisplayInfo::FALLBACK,
            Some((t().session.display_detection_failed)(&error.to_string())),
        ),
    };
    let profile = options.resolve(display)?;
    Ok(ResolvedConnection {
        client,
        target_device_id: String::new(),
        controller_device_id,
        profile,
        default_codec: profile.codec,
        transport: options.transport,
        summary: ConnectionSummary {
            alias: alias.into(),
            stream_fps: profile.stream_fps,
            codec: profile.codec.label(),
            display_detection_warning: warning,
        },
        assist: Some(AssistConnection {
            request,
            room: None,
            remembered: false,
        }),
        preferences: None,
        audio_preferences: None,
        target_platform: farwing_protocol::Platform::Unknown,
        target_version: String::new(),
        refresh_after_upgrade: false,
        background: None,
    })
}

async fn authorize(
    client: &AuthenticatedClient,
    request: &AssistRequest,
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
            cancellable(cancel, client.assist_accept(&pending.request_id, confirm)).await?;
        anyhow::ensure!(
            joined.host.device_id == pending.host.device_id
                && joined.host.public_key == pending.host.public_key,
            "{}",
            t().session.error_peer_identity_changed
        );
        crate::transport::signal::identity::check_key(&joined.host.device_id, &joined.host.public_key)?;
        joined.pake_key = key;
        // Listed under "最近与收藏", and reached without a code next time
        // when the host trusts this device.
        crate::transport::direct::store::remember_host(crate::transport::direct::store::KnownHost {
            device_id: joined.host.device_id.clone(),
            public_key: joined.host.public_key.clone(),
            name: joined.host.alias.clone(),
            address: String::new(),
            connect_id: if crate::account::assist::is_short_number(&request.target) {
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
