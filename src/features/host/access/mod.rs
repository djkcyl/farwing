//! Device access policy, account lifetime and individual connection leases.
use super::{capture, lock, settings};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;
use crate::i18n::t;

mod lease;
mod owner;
mod remote;
mod sessions;
mod status;
pub(crate) use status::*;
use sessions::*;

/// The host already admits as many participants as it allows; the session
/// ends as closed by the host, not as a media failure.
#[derive(Debug)]
pub(crate) struct ParticipantLimit;
impl std::fmt::Display for ParticipantLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(t().features.host_participant_limit)
    }
}
impl std::error::Error for ParticipantLimit {}

struct Ownership {
    permission: u64,
    settings_revision: u64,
    encoding: super::EncodingSettings,
    clipboard: super::clipboard::Settings,
    file_transfer: bool,
    port_mapping: bool,
    remote_power: bool,
    wol: bool,
    /// Concurrent participants this host admits (published as policy).
    max_participants: u32,
    /// How this host's picture passes the server's forwarder.
    forwarding: super::forwarding::Mode,
    power_executing: bool,
    port_revision: u64,
    audio_device: watch::Sender<Option<super::audio::Device>>,
    audio_defaults: super::audio::DefaultDevices,
    audio_quality: crate::media::audio::encoder::Quality,
    audio_inventory: super::audio::Inventory,
    /// Epoch of every media session; bumped to end them all.
    media: u64,
    stream_generations: std::collections::BTreeMap<usize, u64>,
    /// Concurrent participants, by local key in admission order.
    participants: std::collections::BTreeMap<u64, Participant>,
    next_participant: u64,
    allowed: bool,
    active: bool,
    dirty: bool,
    /// The last visitor left (for a code that changes after each visit).
    visit_ended: bool,
    last_controlled: Option<std::time::Instant>,
    connected_since: Option<std::time::Instant>,
    remote_action: Option<RemoteAction>,
    /// Sessions to end in the resident, one participant each.
    remote_disconnects: std::collections::VecDeque<String>,
    /// Control blocks to apply in the resident.
    remote_blocks: std::collections::VecDeque<(String, bool)>,
    updating: bool,
    update_notice: Option<Arc<super::peer::UpdateNotice>>,
    status: Status,
}

impl Ownership {
    fn settings(&self) -> settings::HostSettings {
        settings::HostSettings {
            allow_control: self.allowed,
            encoding: self.encoding,
            audio_device: self.audio_device.borrow().clone(),
            audio_defaults: self.audio_defaults,
            audio_quality: self.audio_quality,
            clipboard: self.clipboard,
            file_transfer: self.file_transfer,
            port_mapping: self.port_mapping,
            remote_power: self.remote_power,
            wol: self.wol,
            max_participants: self.max_participants,
            forwarding: self.forwarding,
        }
    }
    /// Take stored or observed settings as they are (no session effects).
    fn apply_settings(&mut self, settings: settings::HostSettings) {
        self.allowed = settings.allow_control;
        self.encoding = settings.encoding;
        self.audio_device.send_replace(settings.audio_device);
        self.audio_defaults = settings.audio_defaults;
        self.audio_quality = settings.audio_quality;
        self.clipboard = settings.clipboard;
        self.file_transfer = settings.file_transfer;
        self.port_mapping = settings.port_mapping;
        self.remote_power = settings.remote_power;
        self.wol = settings.wol;
        self.max_participants = settings.max_participants;
        self.forwarding = settings.forwarding;
        super::forwarding::set_mode(settings.forwarding);
    }
}

/// What a participant did, for its connection record
/// (`docs/connection-records.md`).
#[derive(Default)]
struct Record {
    /// The controller's visit (empty: nothing is reported).
    visit: String,
    controller_device_id: String,
    /// A connection without the server: what the record needs.
    direct: Option<farwing_protocol::api::DirectRecord>,
    controlled: bool,
    /// File transfers in order, by notice ID, and whether each has ended.
    files: Vec<(String, farwing_protocol::api::FileRecord, bool)>,
    more_files: u32,
    ports: Vec<String>,
}


/// One connected controller or viewer.
struct Participant {
    stream_generations: std::collections::BTreeMap<usize, u64>,
    /// This participant's own streams; the first participant's also fill
    /// the single-connection `Status::streams`.
    streams: std::collections::BTreeMap<usize, StreamStatus>,
    connection: ConnectionInfo,
    connected_since: Option<std::time::Instant>,
    connected: bool,
    update_notice: Option<Arc<super::peer::UpdateNotice>>,
    /// The owner withdrew `control` for the rest of this session.
    control_blocked: bool,
    /// The host's user let this participant control for this session
    /// only, without the grant.
    temporary_control: bool,
    record: Record,
}

#[derive(Clone)]
pub(crate) struct Handle {
    pub(crate) wol: super::wol::Handle,
    pub(crate) wol_setup: super::wol::setup::Handle,
    pub(crate) power: super::power::Handle,
    pub(crate) assistance: super::assist::Handle,
    desired: watch::Sender<Option<AccessRequest>>,
    port_changed: watch::Sender<u64>,
    ownership: Arc<Mutex<Ownership>>,
    store: Option<settings::Store>,
    saving: Arc<tokio::sync::Mutex<()>>,
    media: Arc<super::desktop::Cache>,
    scope: String,
    account: String,
    guest: bool,
}

impl Default for Handle {
    fn default() -> Self {
        Self {
            wol: Default::default(),
            wol_setup: Default::default(),
            power: Default::default(),
            assistance: Default::default(),
            desired: watch::channel(None).0,
            port_changed: watch::channel(0).0,
            ownership: Arc::new(Mutex::new(Ownership {
                permission: 0,
                settings_revision: 0,
                encoding: Default::default(),
                clipboard: Default::default(),
                file_transfer: true,
                port_mapping: false,
                remote_power: false,
                wol: false,
                max_participants: farwing_protocol::limits::DEFAULT_MAX_PARTICIPANTS,
                forwarding: Default::default(),
                power_executing: false,
                port_revision: 0,
                audio_device: watch::channel(None).0,
                audio_defaults: Default::default(),
                audio_quality: Default::default(),
                audio_inventory: Default::default(),
                media: 0,
                stream_generations: Default::default(),
                participants: Default::default(),
                next_participant: 1,
                allowed: false,
                active: true,
                dirty: false,
                visit_ended: false,
                last_controlled: None,
                connected_since: None,
                remote_action: None,
                remote_disconnects: Default::default(),
                remote_blocks: Default::default(),
                updating: false,
                update_notice: None,
                status: Status::default(),
            })),
            store: None,
            saving: Arc::new(tokio::sync::Mutex::new(())),
            media: Arc::default(),
            scope: String::new(),
            account: String::new(),
            guest: false,
        }
    }
}

impl Handle {
    pub(crate) fn audio_device_changes(&self) -> watch::Receiver<Option<super::audio::Device>> {
        lock(&self.ownership).audio_device.subscribe()
    }
    pub(crate) async fn prepare_update(&self) -> anyhow::Result<()> {
        let notices: Vec<_> = {
            let mut state = lock(&self.ownership);
            state.updating = true;
            state
                .participants
                .values()
                .filter(|p| p.connected)
                .filter_map(|p| p.update_notice.clone())
                .collect()
        };
        let mut result = Ok(());
        for notice in notices {
            if let Err(error) = notice.send().await {
                result = Err(error);
            }
        }
        if let Err(error) = result {
            // PrepareUpdate closes admission, not a remote-consent gate. A
            // controller that cannot acknowledge must not veto a local update.
            // Pause/deployment still owns and verifies actual process exit.
            tracing::warn!(error=%format!("{error:#}"), "update notification unavailable; continuing local shutdown");
        }
        Ok(())
    }
    pub(crate) fn cancel_update(&self) {
        lock(&self.ownership).updating = false;
    }
    pub(crate) fn bind_account(&mut self, account: String) {
        self.account = account;
    }
    /// The account generation the resident checks on management requests.
    pub(crate) fn account_generation(&self) -> String {
        self.account.clone()
    }
    pub(crate) async fn prepare_startup(
        &self,
        cancel: tokio_util::sync::CancellationToken,
    ) -> anyhow::Result<()> {
        if crate::platform::windows::host_service::resident::managed() {
            return Ok(());
        }
        tokio::task::spawn_blocking(super::displays::recovery::recover_abandoned).await??;
        let screens = capture::screens()?;
        let Some(screen) = screens
            .iter()
            .find(|s| s.primary)
            .or(screens.first())
            .cloned()
        else {
            return Ok(());
        };
        let owner = self.clone();
        self.media
            .prepare(
                screen,
                self.encoding_settings().gpu_compatibility,
                move || !cancel.is_cancelled() && lock(&owner.ownership).active,
            )
            .await?;
        Ok(())
    }
    pub(crate) async fn invalidate_media(&self) {
        self.media.invalidate().await;
    }
    pub(crate) fn retry(&self) {
        if crate::platform::windows::host_service::resident::managed() {
            lock(&self.ownership).remote_action = Some(RemoteAction::Retry);
            return;
        }
        let mut state = lock(&self.ownership);
        if !state.active || !state.allowed {
            return;
        }
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = None;
        state.status.message = t().features.host_enabling.into();
        self.desired.send_replace(Some(AccessRequest {
            generation: state.permission,
        }));
    }
    pub(crate) fn retire(&self) {
        self.wol.online(false);
        self.power.invalidate();
        self.assistance.unavailable();
        let mut state = lock(&self.ownership);
        let clear_update =
            !state.updating && !crate::platform::windows::host_service::resident::managed();
        state.active = false;
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = t().features.host_offline.into();
        self.desired.send_replace(None);
        drop(state);
        if clear_update {
        }
    }
    pub(crate) fn room_closed(&self) {
        self.assistance.unavailable();
        let mut state = lock(&self.ownership);
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = t().features.host_offline.into();
        self.desired
            .send_replace((state.active && state.allowed).then_some(AccessRequest {
                generation: state.permission,
            }));
    }
    pub(crate) fn disconnect(&self) {
        let session = lock(&self.ownership)
            .status
            .connection
            .as_ref()
            .map(|c| c.session_id.clone());
        if let Some(session) = session {
            self.disconnect_session(&session);
        }
    }
    pub(crate) fn disconnect_session(&self, session: &str) -> bool {
        let managed = crate::platform::windows::host_service::resident::managed();
        disconnect_session(&mut lock(&self.ownership), session, managed)
    }
    /// Withdraw (`blocked`) or restore `control` for one participant.
    pub(crate) fn block_control(&self, session: &str, blocked: bool) -> bool {
        let managed = crate::platform::windows::host_service::resident::managed();
        block_control(&mut lock(&self.ownership), session, blocked, managed)
    }
    pub(crate) fn requested(&self) -> bool {
        let state = lock(&self.ownership);
        state.active && state.allowed && !state.power_executing
    }
    pub(crate) fn subscribe(&self) -> watch::Receiver<Option<AccessRequest>> {
        self.desired.subscribe()
    }
    pub(crate) fn status(&self) -> Status {
        let state = lock(&self.ownership);
        let mut status = state.status.clone();
        if let (Some(connection), Some(since)) = (&mut status.connection, state.connected_since) {
            connection.elapsed_seconds = Some(since.elapsed().as_secs());
        }
        for (connection, participant) in status
            .connections
            .iter_mut()
            .zip(state.participants.values())
        {
            connection.elapsed_seconds = participant.connected_since.map(|at| at.elapsed().as_secs());
        }
        status
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        Self::update_locked(&mut lock(&self.ownership), ready, connected, message.into());
    }
    fn update_locked(state: &mut Ownership, ready: bool, connected: bool, message: String) {
        if !state.participants.is_empty() {
            // Participants report their own connection state.
            state.status.ready = ready;
            if !state.status.connected {
                state.status.message = message;
            }
            return;
        }
        if connected && state.status.connection.is_some() && state.connected_since.is_none() {
            state.connected_since = Some(std::time::Instant::now());
        }
        if connected || state.status.connected {
            state.last_controlled = Some(std::time::Instant::now());
        }
        state.status.ready = ready;
        state.status.connected = connected;
        state.status.message = message;
    }
    pub(crate) fn lease(&self, request: &AccessRequest) -> Lease {
        Lease {
            handle: self.clone(),
            permission: request.generation,
            media: None,
            track: 0,
            stream_generation: None,
            stop: None,
            assistance: None,
            grants: None,
            participant: None,
        }
    }
}

/// Session capabilities of one participant (`docs/protocol/v2-design.md`
/// §3): the server's grants for own devices and friends, the local template
/// for assistance. Shared by every clone of the session's lease so that a
/// `session.permissions` change applies at once.
pub(crate) type SessionGrants = Arc<std::sync::RwLock<Vec<String>>>;

#[derive(Clone)]
pub(crate) struct Lease {
    handle: Handle,
    permission: u64,
    media: Option<u64>,
    track: usize,
    stream_generation: Option<u64>,
    stop: Option<tokio_util::sync::CancellationToken>,
    assistance: Option<tokio_util::sync::CancellationToken>,
    /// `None` before a session binds its participant's grants.
    grants: Option<SessionGrants>,
    /// The participant this lease belongs to (`peer_lease`).
    participant: Option<u64>,
}

/// Unique connection owner. Callback clones only hold the checked lease.
pub(crate) struct SessionLease(Lease);

#[cfg(test)]
mod tests {
    use super::*;
    use farwing_protocol::caps::CONTROL;

    fn allowed() -> (Handle, Lease) {
        let handle = Handle::default();
        handle.set_allowed(true);
        let request = AccessRequest {
            generation: lock(&handle.ownership).permission,
        };
        let lease = handle
            .lease(&request)
            .with_grants(vec!["view".into(), CONTROL.into()]);
        (handle, lease)
    }

    #[test]
    fn blocking_control_masks_the_capability_for_one_participant() {
        // The portable path: on a machine with the service installed the
        // default profile would hand the block to the resident instead.
        let block = |handle: &Handle, session: &str, blocked: bool| {
            block_control(&mut lock(&handle.ownership), session, blocked, false)
        };
        let (handle, lease) = allowed();
        let first = lease.peer_lease().unwrap();
        first.bind_session("first");
        let second = lease.with_grants(vec![CONTROL.into()]).peer_lease().unwrap();
        second.bind_session("second");
        assert!(first.granted(CONTROL));
        assert!(block(&handle, "first", true));
        assert!(!first.granted(CONTROL));
        assert!(!first.grants().iter().any(|cap| cap == CONTROL));
        assert!(first.grants().iter().any(|cap| cap == "view"));
        assert!(second.granted(CONTROL));
        let blocked = |session: &str| {
            handle
                .status()
                .connections
                .iter()
                .find(|c| c.session_id == session)
                .map(|c| c.control_blocked)
        };
        assert_eq!(blocked("first"), Some(true));
        assert_eq!(blocked("second"), Some(false));
        assert!(block(&handle, "first", false));
        assert!(first.granted(CONTROL));
        assert!(!block(&handle, "missing", true));
    }

    #[test]
    fn participant_limit_follows_the_setting() {
        let (handle, lease) = allowed();
        assert_eq!(
            handle.device_policy().max_participants,
            farwing_protocol::limits::DEFAULT_MAX_PARTICIPANTS
        );
        handle.set_max_participants(1).unwrap();
        assert_eq!(handle.device_policy().max_participants, 1);
        let _first = lease.peer_lease().unwrap();
        assert!(lease.peer_lease().is_err());
        handle.set_max_participants(99).unwrap();
        assert_eq!(handle.max_participants(), farwing_protocol::limits::MAX_PARTICIPANTS);
        assert!(lease.peer_lease().is_ok());
    }
}
