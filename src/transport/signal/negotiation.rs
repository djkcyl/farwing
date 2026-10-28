//! Session negotiation payloads exchanged over the realtime plane.
use anyhow::{Context as _, Result};
use farwing_protocol::realtime::{Envelope, IceServer, RtcCandidate, RtcDescription, types};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

/// Controller-side state of an accepted `session.start`.
#[derive(Clone)]
pub struct ControlSessionInfo {
    /// Device identities of both peers for this attempt.
    pub(crate) binding: super::identity::Binding,
    /// How this controller relates to the host, as stated by the server.
    pub relation: farwing_protocol::api::Relation,
    /// Capabilities the server granted this participant.
    pub grants: Vec<String>,
    /// Server-assigned participant ID of this controller.
    pub participant_id: String,
    /// Server session ID; filters every session-scoped message.
    pub session_id: String,
    pub ice_servers: Vec<IceServer>,
    pub force_relay: bool,
    /// This controller's attempt ID (`session.start`).
    pub attempt_id: String,
    /// What this controller offered: its decoders and starting stream.
    pub(crate) offer: farwing_protocol::session::ConnectOptions,
}

impl std::fmt::Debug for ControlSessionInfo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ControlSessionInfo")
            .field("participant_id", &"***REDACTED***")
            .field("session_id", &"***REDACTED***")
            .field("ice_server_count", &self.ice_servers.len())
            .field("force_relay", &self.force_relay)
            .finish_non_exhaustive()
    }
}

/// The peer's SDP is not bound to its device identity: the session is ended
/// rather than waiting for another answer.
#[derive(Debug)]
pub(crate) struct IdentityFailure(pub String);

impl std::fmt::Display for IdentityFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&(crate::i18n::t().session.error_peer_identity_failed)(&self.0))
    }
}

impl std::error::Error for IdentityFailure {}

/// Whether a session-scoped message belongs to `expected`.
pub(super) fn same_session(envelope: &Envelope, expected: &ControlSessionInfo) -> bool {
    envelope.sid.as_deref() == Some(expected.session_id.as_str())
}

pub(super) enum RemoteRtc {
    Answer {
        sdp: String,
        restart_ice: bool,
        generation: u64,
    },
    Candidate(RTCIceCandidateInit),
}

/// Decode `rtc.answer` / `rtc.candidate` addressed to this controller attempt.
pub(super) fn parse_remote_rtc(
    envelope: &Envelope,
    expected: &ControlSessionInfo,
) -> Result<Option<RemoteRtc>> {
    if !same_session(envelope, expected) {
        return Ok(None);
    }
    match envelope.kind.as_str() {
        types::RTC_ANSWER => {
            let answer: RtcDescription = envelope.body().context("decode rtc.answer")?;
            if answer.attempt_id != expected.attempt_id {
                return Ok(None);
            }
            anyhow::ensure!(!answer.sdp.is_empty(), "rtc.answer did not contain SDP");
            anyhow::ensure!(
                answer.sdp.len() <= farwing_protocol::limits::MAX_SDP_BYTES,
                "remote SDP exceeds the safety limit"
            );
            expected
                .binding
                .verify(&answer.sdp)
                .map_err(|error| IdentityFailure(format!("{error:#}")))?;
            Ok(Some(RemoteRtc::Answer {
                sdp: answer.sdp,
                restart_ice: answer.restart,
                generation: envelope.generation.unwrap_or(0),
            }))
        }
        types::RTC_CANDIDATE => {
            let candidate: RtcCandidate = envelope.body().context("decode rtc.candidate")?;
            if candidate.attempt_id != expected.attempt_id {
                return Ok(None);
            }
            Ok(Some(RemoteRtc::Candidate(to_candidate_init(candidate.candidate)?)))
        }
        _ => Ok(None),
    }
}

pub(crate) fn to_candidate_init(
    candidate: farwing_protocol::realtime::Candidate,
) -> Result<RTCIceCandidateInit> {
    anyhow::ensure!(
        candidate.candidate.len() <= farwing_protocol::limits::MAX_CANDIDATE_BYTES,
        "ICE candidate exceeds the safety limit"
    );
    Ok(RTCIceCandidateInit {
        candidate: candidate.candidate,
        sdp_mid: candidate.sdp_mid,
        sdp_mline_index: candidate.sdp_mline_index,
        username_fragment: candidate.username_fragment,
    })
}

pub(crate) fn from_candidate_init(
    candidate: &RTCIceCandidateInit,
) -> farwing_protocol::realtime::Candidate {
    farwing_protocol::realtime::Candidate {
        candidate: candidate.candidate.clone(),
        sdp_mid: Some(
            candidate
                .sdp_mid
                .as_deref()
                .filter(|value| !value.is_empty())
                .unwrap_or("0")
                .to_owned(),
        ),
        sdp_mline_index: candidate.sdp_mline_index,
        username_fragment: candidate.username_fragment.clone(),
    }
}
