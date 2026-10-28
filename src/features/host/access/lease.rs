//! One connection's lease on the host: grants, control, streams, reports.
use super::*;

impl std::ops::Deref for SessionLease {
    type Target = Lease;
    fn deref(&self) -> &Lease {
        &self.0
    }
}
impl Drop for SessionLease {
    fn drop(&mut self) {
        self.0.finish();
    }
}
impl Lease {
    pub(crate) fn controller(&self, device_id: &str, client_id: &str, public_key: &str) {
        let participant = self.participant;
        self.modify(|state| {
            let Some(p) = participant.and_then(|key| state.participants.get_mut(&key)) else {
                return;
            };
            p.connection.device_id = if crate::account::api::validate_device_id(device_id).is_ok() {
                device_id.to_owned()
            } else {
                String::new()
            };
            p.connection.client_id = client_id.to_owned();
            p.connection.public_key = if farwing_protocol::crypto::valid_public_key(public_key) {
                public_key.to_owned()
            } else {
                String::new()
            };
            refresh_connections(state);
        });
    }
    /// Capabilities this host gives an assisting participant.
    pub(crate) fn assist_grants(&self) -> Vec<String> {
        self.handle.assistance.settings().grants()
    }
    /// The controller device of this participant, empty when unknown.
    pub(crate) fn controller_device(&self) -> String {
        let state = lock(&self.handle.ownership);
        self.participant
            .and_then(|key| state.participants.get(&key))
            .map(|p| p.connection.device_id.clone())
            .unwrap_or_default()
    }
    /// The participant this lease belongs to.
    pub(crate) fn participant(&self) -> Option<u64> {
        self.participant
    }
    /// The connection record this participant's summary goes to: the
    /// controller's visit and device; a direct connection's details.
    pub(crate) fn set_record(
        &self,
        visit: &str,
        controller_device_id: &str,
        direct: Option<farwing_protocol::api::DirectRecord>,
    ) {
        let participant = self.participant;
        self.modify(|state| {
            if let Some(p) = participant.and_then(|key| state.participants.get_mut(&key)) {
                visit.clone_into(&mut p.record.visit);
                controller_device_id.clone_into(&mut p.record.controller_device_id);
                p.record.direct = direct;
            }
        });
    }
    /// The participant used the keyboard and mouse.
    pub(crate) fn record_control(&self) {
        let participant = self.participant;
        self.modify(|state| {
            if let Some(p) = participant.and_then(|key| state.participants.get_mut(&key)) {
                p.record.controlled = true;
            }
        });
    }
    /// The participant forwarded a port to `target` (`tcp 127.0.0.1:3389`).
    pub(crate) fn record_port(&self, target: String) {
        let participant = self.participant;
        self.modify(|state| {
            if let Some(p) = participant.and_then(|key| state.participants.get_mut(&key))
                && !p.record.ports.contains(&target)
                && p.record.ports.len() < farwing_protocol::api::HostReport::MAX_PORTS
            {
                p.record.ports.push(target);
            }
        });
    }
    /// How the participant is shown on this host.
    pub(crate) fn set_profile(&self, profile: &super::super::participants::Profile) {
        let participant = self.participant;
        self.modify(|state| {
            if let Some(p) = participant.and_then(|key| state.participants.get_mut(&key)) {
                p.connection.nickname.clone_from(&profile.nickname);
                p.connection.device_name.clone_from(&profile.device_name);
                p.connection.relation.clone_from(&profile.relation);
                refresh_connections(state);
            }
        });
    }
    /// The participant's profile as recorded by [`Self::set_profile`].
    pub(crate) fn profile(&self) -> super::super::participants::Profile {
        let state = lock(&self.handle.ownership);
        self.participant
            .and_then(|key| state.participants.get(&key))
            .map(|p| super::super::participants::Profile {
                nickname: p.connection.nickname.clone(),
                device_name: p.connection.device_name.clone(),
                relation: p.connection.relation.clone(),
            })
            .unwrap_or_default()
    }
    /// The server session this participant belongs to (for disconnects).
    pub(crate) fn bind_session(&self, session_id: &str) {
        let participant = self.participant;
        self.modify(|state| {
            if let Some(p) = participant.and_then(|key| state.participants.get_mut(&key)) {
                p.connection.session_id = session_id.to_owned();
                refresh_connections(state);
            }
        });
    }
    pub(crate) fn for_assistance(&self) -> anyhow::Result<Self> {
        let mut lease = self.clone();
        lease.assistance = Some(self.handle.assistance.admission()?);
        Ok(lease)
    }
    pub(crate) fn is_assistance(&self) -> bool {
        self.assistance.is_some()
    }
    /// Bind the participant's capabilities to this lease and its clones.
    pub(crate) fn with_grants(&self, grants: Vec<String>) -> Self {
        let mut lease = self.clone();
        lease.grants = Some(Arc::new(std::sync::RwLock::new(
            farwing_protocol::caps::normalize(&grants),
        )));
        lease
    }
    /// Replace the participant's capabilities (`session.permissions`).
    pub(crate) fn set_grants(&self, grants: Vec<String>) {
        if let Some(shared) = &self.grants {
            *shared.write().unwrap_or_else(|e| e.into_inner()) =
                farwing_protocol::caps::normalize(&grants);
        }
        super::super::participants::touch();
    }
    pub(crate) fn grants(&self) -> Vec<String> {
        let mut grants = self.grants.as_ref().map_or_else(Vec::new, |shared| {
            shared.read().unwrap_or_else(|e| e.into_inner()).clone()
        });
        if self.control_blocked() {
            grants.retain(|cap| cap != farwing_protocol::caps::CONTROL);
        } else if self.temporary_control() && !grants.iter().any(|c| c == farwing_protocol::caps::CONTROL) {
            grants.push(farwing_protocol::caps::CONTROL.to_owned());
            grants = farwing_protocol::caps::normalize(&grants);
        }
        grants
    }
    /// The host owner withdrew `control` from this participant.
    pub(super) fn control_blocked(&self) -> bool {
        let state = lock(&self.handle.ownership);
        self.participant
            .and_then(|key| state.participants.get(&key))
            .is_some_and(|p| p.control_blocked)
    }
    /// The host's user let this participant control for this session.
    pub(super) fn temporary_control(&self) -> bool {
        let state = lock(&self.handle.ownership);
        self.participant
            .and_then(|key| state.participants.get(&key))
            .is_some_and(|p| p.temporary_control)
    }
    /// Whether the participant holds session capability `cap`. A lease
    /// without bound grants (before the session) holds none.
    pub(crate) fn granted(&self, cap: &str) -> bool {
        if cap == farwing_protocol::caps::CONTROL {
            if self.control_blocked() {
                return false;
            }
            if self.temporary_control() {
                return true;
            }
        }
        self.grants.as_ref().is_some_and(|shared| {
            shared
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|value| value == cap)
        })
    }
    /// Ask the host's user to let this participant control for this
    /// session only. True once allowed.
    pub(crate) async fn ask_control(&self) -> bool {
        let Some(key) = self.participant else {
            return false;
        };
        let profile = self.profile();
        let name = if profile.nickname.trim().is_empty() {
            profile.device_name.clone()
        } else {
            profile.nickname.clone()
        };
        let device_id = {
            let state = lock(&self.handle.ownership);
            state
                .participants
                .get(&key)
                .map(|p| p.connection.device_id.clone())
                .unwrap_or_default()
        };
        let decision = self.handle.assistance.confirm_control(name, device_id).await;
        if !matches!(decision, super::super::assist::Decision::Allow | super::super::assist::Decision::Trust) {
            return false;
        }
        self.modify(|state| {
            if let Some(p) = state.participants.get_mut(&key) {
                p.temporary_control = true;
                p.control_blocked = false;
                refresh_connections(state);
            }
        });
        super::super::participants::touch();
        tracing::info!(participant = key, "host user allowed control for this session");
        true
    }
    /// Changing the host's displays: `display` together with `control`
    /// (changing them is operating the host).
    pub(crate) fn display_allowed(&self) -> bool {
        self.granted(farwing_protocol::caps::DISPLAY) && self.granted(farwing_protocol::caps::CONTROL)
    }
    /// Explicit extra-display requests need [`Self::display_allowed`]. This
    /// is separate from resolution conversion, headless recovery and
    /// releasing a screen.
    pub(crate) fn display_expansion_allowed(&self) -> bool {
        self.requested() && self.display_allowed()
    }
    pub(crate) fn set_update_notice(&self, notice: Arc<super::super::peer::UpdateNotice>) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            match self.participant.and_then(|key| state.participants.get_mut(&key)) {
                Some(p) => p.update_notice = Some(notice),
                None => state.update_notice = Some(notice),
            }
        }
    }
    pub(crate) fn audio_quality(&self) -> crate::media::audio::encoder::Quality {
        self.handle.audio_quality()
    }
    pub(crate) fn audio_defaults(&self) -> super::super::audio::DefaultDevices {
        self.handle.audio_defaults()
    }
    pub(crate) fn audio_device_changes(&self) -> watch::Receiver<Option<super::super::audio::Device>> {
        self.handle.audio_device_changes()
    }
    /// The host this lease belongs to.
    pub(crate) fn host(&self) -> Handle {
        self.handle.clone()
    }
    pub(crate) fn with_cancellation(&self, stop: tokio_util::sync::CancellationToken) -> Self {
        let mut lease = self.clone();
        lease.stop = Some(stop);
        lease
    }
    pub(crate) fn display_scope(&self) -> &str {
        &self.handle.scope
    }
    pub(crate) fn begin_stream(&self, track: usize) -> anyhow::Result<Self> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "{}", t().features.host_permission_expired);
        let generations = match self.participant {
            Some(key) => {
                &mut state
                    .participants
                    .get_mut(&key)
                    .ok_or_else(|| anyhow::anyhow!(t().features.host_participant_left))?
                    .stream_generations
            }
            None => &mut state.stream_generations,
        };
        let generation = generations.entry(track).or_default();
        *generation = generation.wrapping_add(1);
        let generation = *generation;
        let mut lease = self.clone();
        lease.track = track;
        lease.stream_generation = Some(generation);
        Ok(lease)
    }
    pub(crate) fn clear_stream(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            if self.primary(&state) {
                state.status.streams.remove(&self.track);
                update_media_summary(&mut state.status);
            }
            if let Some(p) = self.participant.and_then(|key| state.participants.get_mut(&key)) {
                p.streams.remove(&self.track);
            }
            let generations = match self.participant {
                Some(key) => match state.participants.get_mut(&key) {
                    Some(p) => &mut p.stream_generations,
                    None => return,
                },
                None => &mut state.stream_generations,
            };
            let generation = generations.entry(self.track).or_default();
            *generation = generation.wrapping_add(1);
        }
    }
    /// The first participant owns the single-connection status fields.
    pub(super) fn primary(&self, state: &Ownership) -> bool {
        match self.participant {
            Some(key) => state.participants.keys().next() == Some(&key),
            None => true,
        }
    }
    /// Update this stream's status for the participant, and in the
    /// single-connection status when this is the first participant.
    pub(super) fn modify_stream(&self, update: impl Fn(&mut StreamStatus)) {
        let mut state = lock(&self.handle.ownership);
        if !self.current(&state) {
            return;
        }
        if let Some(p) = self.participant.and_then(|key| state.participants.get_mut(&key)) {
            update(p.streams.entry(self.track).or_default());
        }
        if self.primary(&state) {
            update(state.status.streams.entry(self.track).or_default());
            update_media_summary(&mut state.status);
        }
    }
    pub(crate) fn video(&self, encoding: Option<ActiveEncoding>) {
        self.modify_stream(|stream| stream.video = encoding);
    }
    pub(crate) async fn prepare_media(
        &self,
        screen: capture::Screen,
        gpu_compatibility: bool,
    ) -> anyhow::Result<Arc<super::super::desktop::Capabilities>> {
        // A cold encoder probe can outlast the controller's connect deadline.
        // It runs for the host, not this participant, so a controller that
        // gives up and retries finds the result instead of a new probe.
        let host = self.handle.clone();
        let media = Arc::clone(&self.handle.media);
        let probe = tokio::spawn(async move {
            media
                .prepare(screen, gpu_compatibility, move || {
                    lock(&host.ownership).active
                })
                .await
        });
        let lease = self.clone();
        tokio::select! {
            result = probe => anyhow::Context::context(result, t().features.host_media_interrupted)?,
            _ = async move {
                while lease.requested() {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } => anyhow::bail!("{}", t().features.host_media_cancelled),
        }
    }
    pub(super) fn current(&self, state: &Ownership) -> bool {
        self.current_owner(state)
            && self.stop.as_ref().is_none_or(|stop| !stop.is_cancelled())
            && self
                .assistance
                .as_ref()
                .is_none_or(|stop| !stop.is_cancelled())
    }
    pub(super) fn current_owner(&self, state: &Ownership) -> bool {
        let generations = match self.participant {
            Some(key) => match state.participants.get(&key) {
                Some(p) => &p.stream_generations,
                None => return false,
            },
            None => &state.stream_generations,
        };
        state.active
            && state.allowed
            && self.permission == state.permission
            && self.media.is_none_or(|m| m == state.media)
            && self
                .stream_generation
                .is_none_or(|g| generations.get(&self.track) == Some(&g))
    }
    pub(super) fn modify(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            update(&mut state);
        }
    }
    /// Status fields that describe one session: written by the first participant.
    pub(super) fn modify_primary(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) && self.primary(&state) {
            update(&mut state);
        }
    }
    /// Admit one participant. Other participants keep their sessions.
    pub(crate) fn peer_lease(&self) -> anyhow::Result<SessionLease> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "{}", t().features.peer_lease_expired);
        anyhow::ensure!(!state.updating, "{}", t().features.upgrade_host_updating);
        anyhow::ensure!(!state.power_executing, "{}", t().features.host_power_executing);
        if state.participants.len() >= participant_limit(&state) {
            return Err(ParticipantLimit.into());
        }
        if state.participants.is_empty() {
            // A new first participant starts a fresh single-connection view.
            state.status.ended_connection = None;
            state.stream_generations.clear();
        }
        let key = state.next_participant;
        state.next_participant = state.next_participant.wrapping_add(1).max(1);
        state.participants.insert(
            key,
            Participant {
                stream_generations: Default::default(),
                streams: Default::default(),
                connection: ConnectionInfo {
                    session_id: uuid::Uuid::new_v4().simple().to_string(),
                    assistance: self.is_assistance(),
                    participant: key,
                    ..Default::default()
                },
                connected_since: None,
                connected: false,
                update_notice: None,
                control_blocked: false,
                temporary_control: false,
                record: Record::default(),
            },
        );
        refresh_connections(&mut state);
        state.status.error = None;
        Ok(SessionLease(Self {
            handle: self.handle.clone(),
            permission: self.permission,
            media: Some(state.media),
            track: 0,
            stream_generation: None,
            stop: None,
            assistance: self.assistance.clone(),
            grants: self.grants.clone(),
            participant: Some(key),
        }))
    }
    pub(crate) fn finish(&self) {
        let mut state = lock(&self.handle.ownership);
        if !self.current_owner(&state) {
            return;
        }
        match self.participant {
            Some(key) => end_participant(&mut state, key),
            None => {
                if self.media.is_some() {
                    state.media = state.media.wrapping_add(1);
                }
                finish_session(&mut state);
                state.status.message = t().features.host_waiting.into();
            }
        }
    }
    pub(crate) fn source(&self, screen: &capture::Screen, backend: &str) {
        self.modify_stream(|stream| {
            stream.capture = Some(backend.into());
            stream.screen = Some(screen.clone());
        });
    }
    pub(crate) fn encoder(
        &self,
        implementation: i32,
        maximum: (u32, u32),
        screen: &capture::Screen,
        capture: &str,
    ) {
        self.modify_stream(|stream| {
            stream.encoder = Some((implementation, maximum));
            stream.capture = Some(capture.into());
            stream.screen = Some(screen.clone());
        });
    }
    pub(crate) fn requested(&self) -> bool {
        self.current(&lock(&self.handle.ownership))
    }
    pub(crate) fn updating(&self) -> bool {
        lock(&self.handle.ownership).updating
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        let participant = self.participant;
        self.modify(|state| {
            let Some(p) = participant.and_then(|key| state.participants.get_mut(&key)) else {
                Handle::update_locked(state, ready, connected, message.into());
                return;
            };
            if connected && p.connected_since.is_none() {
                p.connected_since = Some(std::time::Instant::now());
            }
            if connected || p.connected {
                state.last_controlled = Some(std::time::Instant::now());
            }
            p.connected = connected;
            refresh_connections(state);
            state.status.ready = ready;
            state.status.message = if state.status.connected {
                t().features.host_being_accessed.into()
            } else {
                message.into()
            };
        });
    }
    pub(crate) fn frame(&self) {
        self.modify(|state| {
            state.status.error = None;
            state.status.message = t().features.host_being_accessed.into();
        });
    }
    pub(crate) fn audio_status(&self, audio: super::super::audio::Status) {
        self.modify_primary(|state| state.status.audio = audio);
    }
    /// Clipboard sync for this participant. It writes the host clipboard,
    /// so it needs `control` besides `clipboard` (files also `files`); with
    /// several participants only the holder of the control token syncs, so
    /// a viewer neither receives the host clipboard nor overwrites it
    /// (`docs/protocol/v2-design.md` §4.2).
    pub(crate) fn clipboard_settings(&self) -> super::super::clipboard::Settings {
        let settings = self.handle.clipboard_settings();
        let shared = lock(&self.handle.ownership).participants.len() > 1;
        if self.granted(farwing_protocol::caps::CLIPBOARD)
            && self.granted(farwing_protocol::caps::CONTROL)
            && (!shared || super::super::participants::holds_control(self.participant))
        {
            super::super::clipboard::Settings {
                enabled: settings.enabled,
                files: settings.files && self.granted(farwing_protocol::caps::FILES),
            }
        } else {
            super::super::clipboard::Settings {
                enabled: false,
                files: false,
            }
        }
    }
    /// Manual clipboard copy (CLIPBOARD v2 `Pull`, §8.2): any participant
    /// holding `clipboard`, within this host's clipboard switches, whether or
    /// not it currently controls; files also need `files`.
    pub(crate) fn clipboard_pull(&self) -> super::super::clipboard::Settings {
        if self.granted(farwing_protocol::caps::CLIPBOARD) {
            let settings = self.handle.clipboard_settings();
            super::super::clipboard::Settings {
                enabled: settings.enabled,
                files: settings.files && self.granted(farwing_protocol::caps::FILES),
            }
        } else {
            super::super::clipboard::Settings {
                enabled: false,
                files: false,
            }
        }
    }
    pub(crate) fn port_changes(&self) -> watch::Receiver<u64> {
        self.handle.port_changed.subscribe()
    }
    pub(crate) fn port_policy(&self) -> (bool, u64) {
        let state = lock(&self.handle.ownership);
        (
            self.current(&state)
                && self.granted(farwing_protocol::caps::PORTS)
                && !self.handle.guest
                && state.port_mapping,
            state.port_revision,
        )
    }
    pub(crate) fn port_status(&self, status: crate::features::port_mapping::host::Status) {
        self.modify_primary(|s| s.status.ports = status);
    }
    pub(crate) fn diagnostic_input(&self) -> crate::diagnostics::bundle::Input {
        let status = self.handle.status();
        crate::diagnostics::bundle::Input {
            summary: serde_json::json!({"host": status, "source": "remote_host"}),
            private_values: vec![self.handle.scope.clone()],
        }
    }
    pub(crate) fn file_access(&self) -> bool {
        self.requested()
            && self.granted(farwing_protocol::caps::FILES)
            && !self.handle.guest
            && self.handle.file_transfer_allowed()
    }
    pub(crate) fn diagnostics_access(&self) -> bool {
        self.requested()
            && self.granted(farwing_protocol::caps::DIAGNOSTICS)
            && !self.handle.guest
    }
    pub(crate) fn file_scope(&self) -> String {
        let state = lock(&self.handle.ownership);
        let device = match self.participant.and_then(|key| state.participants.get(&key)) {
            Some(p) => p.connection.device_id.as_str(),
            None => state
                .status
                .connection
                .as_ref()
                .map_or("", |c| c.device_id.as_str()),
        };
        format!("{}:{}", self.handle.scope, device)
    }
    pub(crate) fn file_notices(
        &self,
        updates: Vec<crate::features::file_transfer::host::notices::Notice>,
    ) {
        if updates.is_empty() {
            return;
        }
        let mut state = lock(&self.handle.ownership);
        // Permit this owner's final cancellation report, but never resurrect an old account/session.
        if !self.current_owner(&state) {
            return;
        }
        if let Some(p) = self.participant.and_then(|key| state.participants.get_mut(&key)) {
            for notice in &updates {
                p.record.file(notice);
            }
        }
        let notices = &mut state.status.file_transfers;
        let now = chrono::Utc::now().timestamp();
        notices.retain(|v| v.state == 0 || now.saturating_sub(v.updated) < 120);
        for value in updates {
            if let Some(old) = notices.iter_mut().find(|n| n.id == value.id) {
                *old = value;
            } else {
                if notices.len() >= 24 {
                    let index = notices.iter().position(|n| n.state != 0).unwrap_or(0);
                    notices.remove(index);
                }
                notices.push(value);
            }
        }
    }
    pub(crate) fn file_status(&self, status: super::super::files::Status) {
        self.modify_primary(|s| s.status.files = status);
    }
    pub(crate) fn clipboard_status(&self, status: super::super::clipboard::Status) {
        self.modify_primary(|s| s.status.clipboard = status);
    }
    /// The aggregate of every participant's microphone (the host mixer).
    pub(crate) fn microphone_status(&self, microphone: super::super::microphone::Status) {
        self.modify(|state| state.status.microphone = microphone);
    }
    pub(crate) fn input_status(&self, backend: Option<&str>, error: Option<String>) {
        self.modify_primary(|state| {
            state.status.input_backend = backend.map(str::to_owned);
            state.status.input_error = error;
        });
    }
    pub(crate) fn recovery_failed(&self, error: impl Into<String>) {
        let mut state = lock(&self.handle.ownership);
        if self.media == Some(state.media) {
            state.status.error = Some(error.into());
        }
    }
    pub(crate) fn fail(&self, error: impl Into<String>) {
        let error = error.into();
        self.modify(|state| {
            tracing::warn!(track = self.track, %error, "host media operation failed");
            state.status.error = Some(error);
        });
    }
}
