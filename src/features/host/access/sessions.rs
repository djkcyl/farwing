//! Participants and their sessions: admission, ending, connection records.
use super::*;

impl Record {
    pub(super) fn file(&mut self, notice: &crate::features::file_transfer::host::notices::Notice) {
        use crate::features::file_transfer::host::notices as n;
        use farwing_protocol::api::{FileOutcome, FileRecord, HostReport};
        let (outcome, ended) = match notice.state {
            n::COMPLETE => (FileOutcome::Done, true),
            n::CANCELLED => (FileOutcome::Cancelled, true),
            n::FAILED => (FileOutcome::Failed, true),
            n::INTERRUPTED => (FileOutcome::Interrupted, true),
            _ => (FileOutcome::Interrupted, false),
        };
        let record = FileRecord {
            name: notice.name.chars().take(256).collect(),
            count: 0,
            bytes: notice.total,
            upload: notice.receiving,
            outcome,
        };
        if let Some(entry) = self.files.iter_mut().find(|(id, ..)| *id == notice.id) {
            (entry.1, entry.2) = (record, ended);
        } else if self.files.len() < HostReport::MAX_FILES {
            self.files.push((notice.id.clone(), record, ended));
        } else {
            self.more_files = self.more_files.saturating_add(1);
        }
    }

    /// The report when the participant leaves (`None`: nothing to report).
    pub(super) fn finish(self) -> Option<farwing_protocol::api::RecordReport> {
        if self.visit.is_empty() {
            return None;
        }
        let now = chrono::Utc::now().timestamp();
        Some(farwing_protocol::api::RecordReport {
            visit: self.visit,
            host_device_id: String::new(),
            controller_device_id: self.controller_device_id,
            direct: self.direct.map(|mut d| {
                d.ended_at = now;
                d
            }),
            viewer: None,
            host: Some(farwing_protocol::api::HostReport {
                controlled: self.controlled,
                camera: self.camera,
                // Unfinished transfers ended with the connection.
                files: self.files.into_iter().map(|(_, f, _)| f).collect(),
                more_files: self.more_files,
                ports: self.ports,
            }),
        })
    }
}
/// Upper bound of concurrent participants this host admits.
pub(super) fn participant_limit(state: &Ownership) -> usize {
    state
        .max_participants
        .clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS) as usize
}


pub(super) fn finish_session(state: &mut Ownership) {
    let participants = std::mem::take(&mut state.participants);
    if let Some(primary) = participants.values().next() {
        // The primary participant's clock covers single-connection views.
        state.connected_since = state.connected_since.or(primary.connected_since);
    }
    // Only the session owner has this clock. A GUI observer losing its IPC
    // connection must not manufacture a remote-session termination event.
    if let (Some(since), Some(connection)) = (state.connected_since, &state.status.connection) {
        let mut connection = connection.clone();
        connection.elapsed_seconds = Some(since.elapsed().as_secs());
        connection.observation_lost = false;
        state.status.ended_connection = Some(EndedConnection {
            id: uuid::Uuid::new_v4().simple().to_string(),
            connection,
            assistance: state.status.assistance,
            ended_at: chrono::Utc::now().timestamp(),
        });
    }
    for notice in &mut state.status.file_transfers {
        notice.interrupt();
    }
    clear_session(state);
}

pub(super) fn disconnect_session(state: &mut Ownership, expected: &str, managed: bool) -> bool {
    if !state.active || expected.is_empty() {
        return false;
    }
    if managed {
        if !state
            .status
            .connections
            .iter()
            .chain(state.status.connection.iter())
            .any(|c| c.session_id == expected && !c.observation_lost)
        {
            return false;
        }
        // Queued, so ending two participants in a row ends both.
        if !state.remote_disconnects.iter().any(|s| s == expected) {
            state.remote_disconnects.push_back(expected.to_owned());
        }
        return true;
    }
    let Some(key) = state
        .participants
        .iter()
        .find(|(_, p)| p.connection.session_id == expected)
        .map(|(key, _)| *key)
    else {
        return false;
    };
    end_participant(state, key);
    true
}

/// Withdraw or restore `control` for one participant. The participant
/// keeps watching; its held keys are released by the input thread.
pub(super) fn block_control(state: &mut Ownership, expected: &str, blocked: bool, managed: bool) -> bool {
    if !state.active || expected.is_empty() {
        return false;
    }
    if managed {
        if !state
            .status
            .connections
            .iter()
            .any(|c| c.session_id == expected && !c.observation_lost)
        {
            return false;
        }
        state.remote_blocks.retain(|(s, _)| s != expected);
        state.remote_blocks.push_back((expected.to_owned(), blocked));
        return true;
    }
    let Some(participant) = state
        .participants
        .values_mut()
        .find(|p| p.connection.session_id == expected)
    else {
        return false;
    };
    if participant.control_blocked != blocked {
        participant.control_blocked = blocked;
        tracing::info!(
            participant = participant.connection.participant,
            blocked,
            "host owner changed the control permission of a participant"
        );
    }
    refresh_connections(state);
    super::super::participants::touch();
    true
}

/// End one participant; the others keep their sessions.
pub(super) fn end_participant(state: &mut Ownership, key: u64) {
    let Some(mut participant) = state.participants.remove(&key) else {
        return;
    };
    if participant.connected {
        state.last_controlled = Some(std::time::Instant::now());
    }
    if participant.connection.assistance
        && participant.connected_since.is_some()
        && !state.participants.values().any(|p| p.connection.assistance)
    {
        state.visit_ended = true;
    }
    if participant.connected_since.is_some()
        && let Some(report) = std::mem::take(&mut participant.record).finish()
    {
        crate::account::records::submit(crate::account::records::Item::Report(report));
    }
    if let Some(since) = participant.connected_since {
        let mut connection = participant.connection.clone();
        connection.elapsed_seconds = Some(since.elapsed().as_secs());
        connection.observation_lost = false;
        state.status.ended_connection = Some(EndedConnection {
            id: uuid::Uuid::new_v4().simple().to_string(),
            assistance: connection.assistance,
            connection,
            ended_at: chrono::Utc::now().timestamp(),
        });
    }
    if state.participants.is_empty() {
        clear_session(state);
        state.status.message = t().features.host_waiting.into();
    } else {
        refresh_connections(state);
    }
}

/// Derive the single-connection status from the participants.
pub(super) fn refresh_connections(state: &mut Ownership) {
    state.status.connections = state
        .participants
        .values()
        .map(|p| {
            let mut connection = p.connection.clone();
            connection.elapsed_seconds = p.connected_since.map(|at| at.elapsed().as_secs());
            connection.control_blocked = p.control_blocked;
            connection
        })
        .collect();
    let primary = state.participants.values().next();
    state.status.connection = primary.map(|p| p.connection.clone());
    state.connected_since = primary.and_then(|p| p.connected_since);
    state.status.assistance = primary.is_some_and(|p| p.connection.assistance);
    state.status.session_active = !state.participants.is_empty();
    state.status.connected = state.participants.values().any(|p| p.connected);
}
pub(super) fn clear_session(state: &mut Ownership) {
    state.connected_since = None;
    state.status.connection = None;
    state.status.connections.clear();
    state.participants.clear();
    state.update_notice = None;
    if state.status.connected {
        state.last_controlled = Some(std::time::Instant::now());
    }
    state.status.connected = false;
    state.status.session_active = false;
    state.status.clipboard = Default::default();
    state.status.files = Default::default();
    state.status.ports = Default::default();
    state.status.assistance = false;
    state.status.encoder = None;
    state.status.capture = None;
    state.status.source = None;
    state.status.video = None;
    state.status.input_backend = None;
    state.status.input_error = None;
    state.status.audio = Default::default();
    state.status.microphone = Default::default();
    state.status.streams.clear();
    state.stream_generations.clear();
}
