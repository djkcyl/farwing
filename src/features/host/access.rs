//! Device access policy, account lifetime and individual connection leases.
use super::{capture, lock, settings};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    /// The first participant's connection (single-connection views).
    #[serde(default)]
    pub connection: Option<ConnectionInfo>,
    /// Every participant connected to this host.
    #[serde(default)]
    pub connections: Vec<ConnectionInfo>,
    pub ended_connection: Option<EndedConnection>,
    #[serde(default)]
    pub power_message: Option<String>,
    #[serde(default)]
    pub wol: super::wol::Status,
    #[serde(default)]
    pub wol_setup: super::wol::setup::Status,
    pub ready: bool,
    pub connected: bool,
    pub session_active: bool,
    pub assistance: bool,
    pub message: String,
    pub error: Option<String>,
    pub settings_error: Option<String>,
    pub input_backend: Option<String>,
    pub input_error: Option<String>,
    #[serde(default)]
    pub clipboard: super::clipboard::Status,
    #[serde(default)]
    pub files: super::files::Status,
    #[serde(default)]
    pub file_transfers: Vec<crate::features::file_transfer::host::notices::Notice>,
    #[serde(default)]
    pub ports: crate::features::port_mapping::host::Status,
    #[serde(default)]
    pub audio: super::audio::Status,
    #[serde(default)]
    pub microphone: super::microphone::Status,
    pub saving: bool,
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
    pub streams: std::collections::BTreeMap<usize, StreamStatus>,
}

impl Status {
    pub(crate) fn disconnect_target(&self) -> Option<&str> {
        self.connection
            .as_ref()
            .filter(|c| self.session_active && !c.observation_lost && !c.session_id.is_empty())
            .map(|c| c.session_id.as_str())
    }
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct ConnectionInfo {
    #[serde(default)]
    pub session_id: String,
    pub device_id: String,
    pub client_id: String,
    pub elapsed_seconds: Option<u64>,
    pub observation_lost: bool,
    /// Remote assistance rather than an account or friend session.
    #[serde(default)]
    pub assistance: bool,
    /// The participant's key within this host (`participants`).
    #[serde(default)]
    pub participant: u64,
    #[serde(default)]
    pub nickname: String,
    #[serde(default)]
    pub device_name: String,
    /// `own`, `friend`, `assist` or `paired`.
    #[serde(default)]
    pub relation: String,
    /// The host owner withdrew `control` from this participant.
    #[serde(default)]
    pub control_blocked: bool,
    /// The participant's device key (empty when unknown).
    #[serde(default)]
    pub public_key: String,
}

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct EndedConnection {
    pub id: String,
    pub connection: ConnectionInfo,
    pub assistance: bool,
    pub ended_at: i64,
}

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct StreamStatus {
    pub encoder: Option<(i32, (u32, u32))>,
    pub capture: Option<String>,
    pub screen: Option<capture::Screen>,
    pub video: Option<ActiveEncoding>,
}

fn update_media_summary(status: &mut Status) {
    let current = status
        .streams
        .values()
        .find(|s| s.video.is_some())
        .or_else(|| status.streams.values().next());
    status.encoder = current.and_then(|s| s.encoder);
    status.capture = current.and_then(|s| s.capture.clone());
    status.screen = current.and_then(|s| s.screen.clone());
    status.video = current.and_then(|s| s.video);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct ActiveEncoding {
    #[serde(default)]
    pub preferred_gpu: Option<crate::media::selection::GpuId>,
    #[serde(default)]
    pub preferred_codec: Option<i32>,
    #[serde(default)]
    pub selection: crate::media::selection::SelectionReason,
    pub backend: super::format::Backend,
    pub adapter: u64,
    pub format: super::format::Format,
    pub size: (u32, u32),
    pub fps: u32,
    pub target_bps: u32,
}

#[derive(Clone)]
pub(crate) struct AccessRequest {
    generation: u64,
}

pub(crate) enum RemoteAction {
    Retry,
    Disconnect(String),
    /// Withdraw or restore `control` for one participant's session.
    BlockControl(String, bool),
}
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
}

/// Upper bound of concurrent participants this host admits.
fn participant_limit(state: &Ownership) -> usize {
    state
        .max_participants
        .clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS) as usize
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

fn finish_session(state: &mut Ownership) {
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

fn disconnect_session(state: &mut Ownership, expected: &str, managed: bool) -> bool {
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
fn block_control(state: &mut Ownership, expected: &str, blocked: bool, managed: bool) -> bool {
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
    super::participants::touch();
    true
}

/// End one participant; the others keep their sessions.
fn end_participant(state: &mut Ownership, key: u64) {
    let Some(participant) = state.participants.remove(&key) else {
        return;
    };
    if participant.connected {
        state.last_controlled = Some(std::time::Instant::now());
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
        state.status.message = "等待连接".into();
    } else {
        refresh_connections(state);
    }
}

/// Derive the single-connection status from the participants.
fn refresh_connections(state: &mut Ownership) {
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
fn clear_session(state: &mut Ownership) {
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
    state.status.screen = None;
    state.status.video = None;
    state.status.input_backend = None;
    state.status.input_error = None;
    state.status.audio = Default::default();
    state.status.microphone = Default::default();
    state.status.streams.clear();
    state.stream_generations.clear();
}

impl Handle {
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
        if let Err(error) = super::update_resume::clear(&self.scope) {
            tracing::warn!(%error, "cancelled update media handoff cleanup failed");
        }
    }
    pub(crate) fn bind_account(&mut self, account: String) {
        self.account = account;
    }
    /// The account generation the resident checks on management requests.
    pub(crate) fn account_generation(&self) -> String {
        self.account.clone()
    }
    pub(crate) fn take_remote_action(&self) -> Option<RemoteAction> {
        let mut state = lock(&self.ownership);
        state
            .remote_action
            .take()
            .or_else(|| {
                state
                    .remote_disconnects
                    .pop_front()
                    .map(RemoteAction::Disconnect)
            })
            .or_else(|| {
                state
                    .remote_blocks
                    .pop_front()
                    .map(|(session, blocked)| RemoteAction::BlockControl(session, blocked))
            })
    }
    pub(crate) async fn apply_remote(
        &self,
        snapshot: crate::platform::windows::host_service::resident::Snapshot,
    ) {
        self.media.replace(snapshot.capabilities).await;
        let mut state = lock(&self.ownership);
        if !state.active {
            return;
        }
        if state.dirty {
            // Pending local preferences must not freeze the observed live session.
            let mut status = snapshot.status;
            status.saving = state.status.saving;
            status.settings_error = state.status.settings_error.clone();
            state.status = status;
            return;
        }
        state.allowed = snapshot.allowed;
        state.encoding = snapshot.encoding;
        state.audio_device.send_replace(snapshot.audio_device);
        state.audio_defaults = snapshot.audio_defaults;
        state.audio_quality = snapshot.audio_quality;
        state.clipboard = snapshot.clipboard;
        state.file_transfer = snapshot.file_transfer;
        state.port_mapping = snapshot.port_mapping;
        state.remote_power = snapshot.remote_power;
        state.wol = snapshot.wol;
        state.max_participants = snapshot.max_participants;
        state.status = snapshot.status;
        self.assistance.replace(snapshot.assistance);
    }
    pub(crate) fn remote_failed(&self, error: String) {
        let mut state = lock(&self.ownership);
        let observed = state.status.connection.clone();
        clear_session(&mut state);
        // Losing the IPC observer is not evidence that remote access ended.
        state.status.connection = observed
            .filter(|c| c.elapsed_seconds.is_some())
            .map(|mut c| {
                c.observation_lost = true;
                c
            });
        state.status.ready = false;
        state.status.error = Some(error);
        state.status.message = "后台连接中断".into();
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
    pub(crate) fn load(account: &str, device: &str) -> Self {
        Self::load_store(account, device, settings::Store::new(account, device))
    }
    pub(crate) fn load_guest(device: &str) -> Self {
        let mut handle = Self::load_store("guest", device, settings::Store::guest(device));
        handle.guest = true;
        handle
    }
    pub(crate) fn is_guest(&self) -> bool {
        self.guest
    }
    fn load_store(account: &str, device: &str, store: anyhow::Result<settings::Store>) -> Self {
        use sha2::{Digest, Sha256};
        let mut handle = Self::default();
        handle.scope = format!("{:x}", Sha256::digest(format!("{account}\0{device}")));
        let result = store.and_then(|store| {
            handle.store = Some(store.clone());
            store.load()
        });
        match result {
            Ok((
                allowed,
                encoding,
                audio_device,
                audio_defaults,
                audio_quality,
                assistance,
                clipboard,
                file_transfer,
                port_mapping,
                remote_power,
                wol,
                max_participants,
            )) => {
                let _ = handle.assistance.configure(assistance);
                handle.set_allowed(allowed);
                let mut state = lock(&handle.ownership);
                state.encoding = encoding;
                // An unavailable encoder choice must remain editable without
                // discarding unrelated saved permissions and device settings.
                state.status.settings_error =
                    encoding.validate().err().map(|error| error.to_string());
                state.audio_device.send_replace(audio_device);
                state.audio_defaults = audio_defaults;
                state.audio_quality = audio_quality;
                state.clipboard = clipboard;
                state.file_transfer = file_transfer;
                state.port_mapping = port_mapping;
                state.remote_power = remote_power;
                state.wol = wol;
                state.max_participants = max_participants;
                state.dirty = false;
                state.status.saving = false;
            }
            Err(error) => lock(&handle.ownership).status.settings_error = Some(error.to_string()),
        }
        handle
    }
    pub(crate) fn allowed(&self) -> bool {
        lock(&self.ownership).allowed
    }
    /// What the local settings allow, as published to the server. `wol`
    /// (remote boot of this device) is owned by the Wake-on-LAN setup.
    pub(crate) fn device_policy(&self) -> crate::account::api::DevicePolicy {
        let state = lock(&self.ownership);
        crate::account::api::DevicePolicy {
            control: state.allowed,
            files: state.file_transfer,
            ports: state.port_mapping,
            power: state.remote_power,
            wol: false,
            // Visitors (access code) or trusted devices may connect through
            // the server (`docs/access-model.md` §2.2).
            assist: self.assistance.settings().enabled
                || crate::transport::direct::store::has_trusted(),
            max_participants: state.max_participants,
        }
    }
    pub(crate) fn max_participants(&self) -> u32 {
        lock(&self.ownership).max_participants
    }
    /// Concurrent participants (1..=16). Connected participants beyond a
    /// lowered limit stay; only new ones are refused.
    pub(crate) fn set_max_participants(&self, limit: u32) -> anyhow::Result<()> {
        let limit = limit.clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS);
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.max_participants != limit {
            state.max_participants = limit;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn audio_device(&self) -> Option<super::audio::Device> {
        lock(&self.ownership).audio_device.borrow().clone()
    }
    pub(crate) fn audio_defaults(&self) -> super::audio::DefaultDevices {
        lock(&self.ownership).audio_defaults
    }
    pub(crate) fn file_transfer_allowed(&self) -> bool {
        lock(&self.ownership).file_transfer
    }
    pub(crate) fn set_file_transfer_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.file_transfer != allowed {
            state.file_transfer = allowed;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn wol_allowed(&self) -> bool {
        lock(&self.ownership).wol
    }
    pub(crate) fn wol_setup_status(&self, status: super::wol::setup::Status) {
        let mut s = lock(&self.ownership);
        if s.active {
            s.status.wol_setup = status;
        }
    }
    pub(crate) fn wol_setup_action(&self, action: super::wol::setup::Action) -> anyhow::Result<()> {
        anyhow::ensure!(!self.guest && lock(&self.ownership).active, "未登录");
        use crate::platform::windows::host_service::resident;
        if resident::managed() {
            resident::call(resident::Request::WolSetup {
                account: self.account.clone(),
                action,
            })
            .map(|_| ())
        } else {
            self.wol_setup.act(action)
        }
    }
    pub(crate) fn wol_permitted(&self) -> bool {
        let s = lock(&self.ownership);
        !self.guest && s.active && s.allowed && s.wol
    }
    pub(crate) fn wol_registration_permitted(&self) -> bool {
        let s = lock(&self.ownership);
        !self.guest && s.active && s.allowed && (s.wol || self.wol_setup.enabled())
    }
    pub(crate) fn set_wol_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        let mut s = lock(&self.ownership);
        anyhow::ensure!(
            s.active && (!allowed || !self.guest),
            "当前账号不可启用唤醒协助"
        );
        if s.wol != allowed {
            s.wol = allowed;
            s.settings_revision = s.settings_revision.wrapping_add(1);
            s.dirty = true;
            s.status.saving = true;
            s.status.settings_error = None;
            self.wol.change();
        }
        Ok(())
    }
    pub(crate) fn wol_status(&self, revision: u64, status: super::wol::Status) {
        let mut s = lock(&self.ownership);
        if s.active && self.wol.revision() == revision {
            s.status.wol = status;
        }
    }
    pub(crate) fn power_allowed(&self) -> bool {
        lock(&self.ownership).remote_power
    }
    pub(crate) fn power_permitted(&self) -> bool {
        let state = lock(&self.ownership);
        !self.guest && state.active && state.allowed && state.remote_power && !state.updating
    }
    pub(crate) fn set_power_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(
            state.active && (!allowed || !self.guest),
            "当前账号不可启用远程电源操作"
        );
        if state.remote_power != allowed {
            state.remote_power = allowed;
            state.power_executing = false;
            state.status.power_message = None;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
            self.power.invalidate();
        }
        Ok(())
    }
    pub(crate) fn begin_power(&self) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(
            !self.guest && state.active && state.allowed && state.remote_power && !state.updating,
            "远程电源许可已撤销"
        );
        state.power_executing = true;
        state.status.power_message = Some("正在执行电源操作…".into());
        Ok(())
    }
    pub(crate) fn power_result(&self, result: Result<(), String>) {
        let mut state = lock(&self.ownership);
        // Windows may allow the user to cancel shutdown for unsaved applications.
        // Do not leave remote access permanently disabled after API acceptance.
        state.power_executing = false;
        match result {
            Ok(()) => state.status.power_message = Some("Windows 已接受电源请求".into()),
            Err(error) => {
                state.power_executing = false;
                state.status.power_message = Some(error);
            }
        }
    }
    pub(crate) fn port_mapping_allowed(&self) -> bool {
        lock(&self.ownership).port_mapping
    }
    pub(crate) fn set_port_mapping_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.port_mapping != allowed {
            state.port_mapping = allowed;
            state.port_revision = state.port_revision.wrapping_add(1);
            self.port_changed.send_replace(state.port_revision);
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn clipboard_settings(&self) -> super::clipboard::Settings {
        lock(&self.ownership).clipboard
    }
    pub(crate) fn set_clipboard_settings(
        &self,
        settings: super::clipboard::Settings,
    ) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.clipboard != settings {
            state.clipboard = settings;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn audio_quality(&self) -> crate::media::audio::encoder::Quality {
        lock(&self.ownership).audio_quality
    }
    pub(crate) fn set_audio_quality(
        &self,
        quality: crate::media::audio::encoder::Quality,
    ) -> anyhow::Result<()> {
        quality.validate()?;
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.audio_quality != quality {
            state.audio_quality = quality;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn set_audio_defaults(
        &self,
        selected: super::audio::DefaultDevices,
    ) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.audio_defaults != selected {
            state.audio_defaults = selected;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn set_audio_device(
        &self,
        device: Option<super::audio::Device>,
    ) -> anyhow::Result<()> {
        if let Some(device) = &device {
            device.validate()?;
        }
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        let changed = *state.audio_device.borrow() != device;
        if changed {
            state.audio_device.send_replace(device);
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn audio_devices(&self) -> super::audio::Inventory {
        lock(&self.ownership).audio_inventory.clone()
    }
    pub(crate) fn request_audio_devices(&self) -> bool {
        let mut state = lock(&self.ownership);
        let list = &mut state.audio_inventory;
        if list.pending
            || list
                .updated
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(2))
        {
            return false;
        }
        list.pending = true;
        true
    }
    pub(crate) fn audio_devices_failed(&self) {
        let mut state = lock(&self.ownership);
        state.audio_inventory.pending = false;
        state.audio_inventory.updated = Some(std::time::Instant::now());
        state.audio_inventory.error = Some("读取播放设备失败".into());
    }
    pub(crate) async fn refresh_audio_devices(&self) {
        let result =
            tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<super::audio::Device>> {
                let devices = crate::platform::windows::loopback::Devices::new()?;
                let mut result: Vec<_> = devices
                    .list()?
                    .into_iter()
                    .map(|device| super::audio::Device {
                        name: device.name(),
                        id: device.id,
                    })
                    .collect();
                result.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
                Ok(result)
            })
            .await;
        let mut state = lock(&self.ownership);
        let list = &mut state.audio_inventory;
        list.pending = false;
        list.updated = Some(std::time::Instant::now());
        match result {
            Ok(Ok(devices)) => {
                list.devices = devices;
                list.error = None;
            }
            Ok(Err(error)) => list.error = Some(format!("读取播放设备失败：{error:#}")),
            Err(_) => list.error = Some("读取播放设备任务中断".into()),
        }
    }
    pub(crate) fn encoding_settings(&self) -> super::EncodingSettings {
        lock(&self.ownership).encoding
    }
    pub(crate) fn set_encoding_settings(
        &self,
        encoding: super::EncodingSettings,
    ) -> anyhow::Result<()> {
        encoding.validate()?;
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if state.encoding != encoding {
            state.encoding = encoding;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn capabilities(&self) -> Option<Arc<super::desktop::Capabilities>> {
        self.media.snapshot()
    }
    pub(crate) fn set_allowed(&self, allowed: bool) {
        if !allowed {
            self.power.invalidate();
            if !crate::platform::windows::host_service::resident::managed() {
                if let Err(error) = super::update_resume::clear(&self.scope) {
                    tracing::warn!(%error, "revoked update media handoff cleanup failed");
                }
            }
        }

        let mut state = lock(&self.ownership);
        if !state.active || state.allowed == allowed {
            return;
        }
        state.allowed = allowed;
        self.wol.change();
        state.settings_revision = state.settings_revision.wrapping_add(1);
        state.permission = state.permission.wrapping_add(1);
        state.dirty = true;
        finish_session(&mut state);
        state.status.ready = false;
        state.status.error = None;
        state.status.settings_error = None;
        state.status.saving = true;
        state.status.message = if allowed {
            "正在启用被控…"
        } else {
            "已禁止被控"
        }
        .into();
        self.desired.send_replace(allowed.then_some(AccessRequest {
            generation: state.permission,
        }));
    }
    pub(crate) fn set_assistance(&self, settings: super::assist::Settings) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        if self.assistance.configure(settings)? {
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) async fn persist_settings(&self) {
        let _saving = self.saving.lock().await;
        loop {
            let (
                revision,
                allowed,
                encoding,
                audio_device,
                audio_defaults,
                audio_quality,
                assistance,
                clipboard,
                file_transfer,
                port_mapping,
                remote_power,
                wol,
                max_participants,
            ) = {
                let state = lock(&self.ownership);
                if !state.dirty {
                    return;
                }
                (
                    state.settings_revision,
                    state.allowed,
                    state.encoding,
                    state.audio_device.borrow().clone(),
                    state.audio_defaults,
                    state.audio_quality,
                    self.assistance.settings(),
                    state.clipboard,
                    state.file_transfer,
                    state.port_mapping,
                    state.remote_power,
                    state.wol,
                    state.max_participants,
                )
            };
            let result = if crate::platform::windows::host_service::resident::managed() {
                crate::platform::windows::host_service::resident::request(
                    crate::platform::windows::host_service::resident::Request::Settings {
                        account: self.account.clone(),
                        allowed,
                        encoding,
                        audio_device,
                        audio_defaults,
                        audio_quality,
                        assistance,
                        clipboard,
                        file_transfer,
                        port_mapping,
                        remote_power,
                        wol,
                        max_participants,
                    },
                )
                .await
                .map(|_| ())
            } else if let Some(store) = self.store.clone() {
                tokio::task::spawn_blocking(move || {
                    store.save(
                        allowed,
                        encoding,
                        audio_device,
                        audio_defaults,
                        audio_quality,
                        assistance,
                        clipboard,
                        file_transfer,
                        port_mapping,
                        remote_power,
                        wol,
                        max_participants,
                    )
                })
                .await
                .map_err(|_| anyhow::anyhow!("被控设置保存任务中断"))
                .and_then(|r| r)
            } else {
                Err(anyhow::anyhow!("被控设置存储不可用"))
            };
            let mut state = lock(&self.ownership);
            if state.settings_revision != revision {
                continue;
            }
            state.dirty = false;
            state.status.saving = false;
            state.status.settings_error = result.err().map(|e| e.to_string());
            return;
        }
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
        state.status.message = "正在启用被控…".into();
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
        state.status.message = "本机离线".into();
        self.desired.send_replace(None);
        drop(state);
        if clear_update {
            if let Err(error) = super::update_resume::clear(&self.scope) {
                tracing::warn!(%error, "retired update media handoff cleanup failed");
            }
        }
    }
    pub(crate) fn room_closed(&self) {
        self.assistance.unavailable();
        let mut state = lock(&self.ownership);
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = "本机离线".into();
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
    /// How the participant is shown on this host.
    pub(crate) fn set_profile(&self, profile: &super::participants::Profile) {
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
    pub(crate) fn profile(&self) -> super::participants::Profile {
        let state = lock(&self.handle.ownership);
        self.participant
            .and_then(|key| state.participants.get(&key))
            .map(|p| super::participants::Profile {
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
        super::participants::touch();
    }
    pub(crate) fn grants(&self) -> Vec<String> {
        let mut grants = self.grants.as_ref().map_or_else(Vec::new, |shared| {
            shared.read().unwrap_or_else(|e| e.into_inner()).clone()
        });
        if self.control_blocked() {
            grants.retain(|cap| cap != farwing_protocol::caps::CONTROL);
        }
        grants
    }
    /// The host owner withdrew `control` from this participant.
    fn control_blocked(&self) -> bool {
        let state = lock(&self.handle.ownership);
        self.participant
            .and_then(|key| state.participants.get(&key))
            .is_some_and(|p| p.control_blocked)
    }
    /// Whether the participant holds session capability `cap`. A lease
    /// without bound grants (before the session) holds none.
    pub(crate) fn granted(&self, cap: &str) -> bool {
        if cap == farwing_protocol::caps::CONTROL && self.control_blocked() {
            return false;
        }
        self.grants.as_ref().is_some_and(|shared| {
            shared
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .iter()
                .any(|value| value == cap)
        })
    }
    /// Explicit extra-display requests need the `display` capability. This
    /// is separate from resolution conversion, headless recovery and
    /// releasing a screen.
    pub(crate) fn display_expansion_allowed(&self) -> bool {
        self.requested() && self.granted(farwing_protocol::caps::DISPLAY)
    }
    pub(crate) fn set_update_notice(&self, notice: Arc<super::peer::UpdateNotice>) {
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
    pub(crate) fn audio_defaults(&self) -> super::audio::DefaultDevices {
        self.handle.audio_defaults()
    }
    pub(crate) fn audio_device_changes(&self) -> watch::Receiver<Option<super::audio::Device>> {
        lock(&self.handle.ownership).audio_device.subscribe()
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
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        let generations = match self.participant {
            Some(key) => {
                &mut state
                    .participants
                    .get_mut(&key)
                    .ok_or_else(|| anyhow::anyhow!("参与者已离开"))?
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
    fn primary(&self, state: &Ownership) -> bool {
        match self.participant {
            Some(key) => state.participants.keys().next() == Some(&key),
            None => true,
        }
    }
    /// Update this stream's status for the participant, and in the
    /// single-connection status when this is the first participant.
    fn modify_stream(&self, update: impl Fn(&mut StreamStatus)) {
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
    /// This participant's streams (its own encoders and capture backends).
    pub(crate) fn streams(&self) -> std::collections::BTreeMap<usize, StreamStatus> {
        let state = lock(&self.handle.ownership);
        match self.participant {
            Some(key) => state
                .participants
                .get(&key)
                .map(|p| p.streams.clone())
                .unwrap_or_default(),
            None => state.status.streams.clone(),
        }
    }
    pub(crate) fn video(&self, encoding: Option<ActiveEncoding>) {
        self.modify_stream(|stream| stream.video = encoding);
    }
    pub(crate) async fn prepare_media(
        &self,
        screen: capture::Screen,
        gpu_compatibility: bool,
    ) -> anyhow::Result<Arc<super::desktop::Capabilities>> {
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
            result = probe => anyhow::Context::context(result, "媒体准备中断")?,
            _ = async move {
                while lease.requested() {
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            } => anyhow::bail!("媒体准备已取消"),
        }
    }
    fn current(&self, state: &Ownership) -> bool {
        self.current_owner(state)
            && self.stop.as_ref().is_none_or(|stop| !stop.is_cancelled())
            && self
                .assistance
                .as_ref()
                .is_none_or(|stop| !stop.is_cancelled())
    }
    fn current_owner(&self, state: &Ownership) -> bool {
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
    fn modify(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            update(&mut state);
        }
    }
    /// Status fields that describe one session: written by the first participant.
    fn modify_primary(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) && self.primary(&state) {
            update(&mut state);
        }
    }
    /// Admit one participant. Other participants keep their sessions.
    pub(crate) fn peer_lease(&self) -> anyhow::Result<SessionLease> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        anyhow::ensure!(!state.updating, "被控端正在更新");
        anyhow::ensure!(!state.power_executing, "被控端正在执行电源操作");
        anyhow::ensure!(
            state.participants.len() < participant_limit(&state),
            "被控端的连接数已达上限"
        );
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
                state.status.message = "等待连接".into();
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
                "正在被远程访问".into()
            } else {
                message.into()
            };
        });
    }
    pub(crate) fn frame(&self) {
        self.modify(|state| {
            state.status.error = None;
            state.status.message = "正在被远程访问".into();
        });
    }
    pub(crate) fn audio_status(&self, audio: super::audio::Status) {
        self.modify_primary(|state| state.status.audio = audio);
    }
    /// Clipboard sync for this participant. With several participants only
    /// the holder of the control token syncs automatically, so a viewer
    /// neither receives the host clipboard nor overwrites it
    /// (`docs/protocol/v2-design.md` §4.2).
    pub(crate) fn clipboard_settings(&self) -> super::clipboard::Settings {
        let settings = self.handle.clipboard_settings();
        let shared = lock(&self.handle.ownership).participants.len() > 1;
        if self.granted(farwing_protocol::caps::CLIPBOARD)
            && (!shared
                || (self.granted(farwing_protocol::caps::CONTROL)
                    && super::participants::holds_control(self.participant)))
        {
            settings
        } else {
            super::clipboard::Settings {
                enabled: false,
                files: false,
            }
        }
    }
    /// Manual clipboard copy (CLIPBOARD v2 `Pull`, §8.2): any participant
    /// holding `clipboard`, within this host's clipboard switches, whether or
    /// not it currently controls.
    pub(crate) fn clipboard_pull(&self) -> super::clipboard::Settings {
        if self.granted(farwing_protocol::caps::CLIPBOARD) {
            self.handle.clipboard_settings()
        } else {
            super::clipboard::Settings {
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
    pub(crate) fn file_status(&self, status: super::files::Status) {
        self.modify_primary(|s| s.status.files = status);
    }
    pub(crate) fn clipboard_status(&self, status: super::clipboard::Status) {
        self.modify_primary(|s| s.status.clipboard = status);
    }
    /// The aggregate of every participant's microphone (the host mixer).
    pub(crate) fn microphone_status(&self, microphone: super::microphone::Status) {
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
