//! Device access policy, account lifetime and individual connection leases.
use super::{capture, lock, settings};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone, Default, serde::Serialize, serde::Deserialize)]
pub(crate) struct Status {
    #[serde(default)]
    pub connection: Option<ConnectionInfo>,
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
    /// A terminal connection (`kind = 8`): the controller runs a shell here
    /// rather than watching the screen, and the access notice says so.
    #[serde(default)]
    pub terminal: bool,
    /// The terminal connection is attached to a shell opened on this machine
    /// (see `Handle::terminal_local_allowed`), so the notice says the other
    /// side is in the local user's own terminal.
    #[serde(default)]
    pub local_terminal: bool,
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
}
struct Ownership {
    permission: u64,
    settings_revision: u64,
    encoding: super::EncodingSettings,
    clipboard: super::clipboard::Settings,
    file_transfer: bool,
    port_mapping: bool,
    terminal: bool,
    terminal_local: bool,
    remote_power: bool,
    wol: bool,
    power_executing: bool,
    port_revision: u64,
    audio_device: watch::Sender<Option<super::audio::Device>>,
    audio_defaults: super::audio::DefaultDevices,
    audio_quality: crate::media::audio::encoder::Quality,
    audio_inventory: super::audio::Inventory,
    media: u64,
    stream_generations: std::collections::BTreeMap<usize, u64>,
    allowed: bool,
    active: bool,
    dirty: bool,
    last_controlled: Option<std::time::Instant>,
    connected_since: Option<std::time::Instant>,
    remote_action: Option<RemoteAction>,
    updating: bool,
    update_notice: Option<Arc<super::peer::UpdateNotice>>,
    status: Status,
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
                terminal: false,
                terminal_local: false,
                remote_power: false,
                wol: false,
                power_executing: false,
                port_revision: 0,
                audio_device: watch::channel(None).0,
                audio_defaults: Default::default(),
                audio_quality: Default::default(),
                audio_inventory: Default::default(),
                media: 0,
                stream_generations: Default::default(),
                allowed: false,
                active: true,
                dirty: false,
                last_controlled: None,
                connected_since: None,
                remote_action: None,
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
    if !state.active || state.status.disconnect_target() != Some(expected) {
        return false;
    }
    if managed {
        state.remote_action = Some(RemoteAction::Disconnect(expected.to_owned()));
    } else {
        state.media = state.media.wrapping_add(1);
        finish_session(state);
        state.status.message = "等待连接".into();
    }
    true
}
fn clear_session(state: &mut Ownership) {
    state.connected_since = None;
    state.status.connection = None;
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
        let notice = {
            let mut state = lock(&self.ownership);
            state.updating = true;
            if state.status.connected && !state.status.assistance {
                Some(
                    state
                        .update_notice
                        .clone()
                        .ok_or_else(|| anyhow::anyhow!("更新通知尚未准备完成")),
                )
            } else {
                None
            }
        };
        let result = match notice {
            Some(Ok(notice)) => notice.send().await,
            Some(Err(error)) => Err(error),
            None => Ok(()),
        };
        if let Err(error) = result {
            // PrepareUpdate closes admission, not a remote-consent gate. A
            // controller that cannot acknowledge must not veto a local update.
            // Pause/deployment still owns and verifies actual process exit.
            tracing::warn!(error=%format!("{error:#}"), "update notification unavailable; continuing local shutdown");
        }
        Ok(())
    }
    // Only the installed Windows service's resident owner cancels an update;
    // a desktop client's update exit never returns.
    #[cfg_attr(not(windows), allow(dead_code))]
    pub(crate) fn cancel_update(&self) {
        lock(&self.ownership).updating = false;
        if let Err(error) = super::update_resume::clear(&self.scope) {
            tracing::warn!(%error, "cancelled update media handoff cleanup failed");
        }
    }
    pub(crate) fn bind_account(&mut self, account: String) {
        self.account = account;
    }
    pub(crate) fn take_remote_action(&self) -> Option<RemoteAction> {
        lock(&self.ownership).remote_action.take()
    }
    pub(crate) async fn apply_remote(
        &self,
        snapshot: crate::platform::host_service::resident::Snapshot,
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
        state.terminal = snapshot.terminal;
        state.terminal_local = snapshot.terminal && snapshot.terminal_local;
        state.remote_power = snapshot.remote_power;
        state.wol = snapshot.wol;
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
        if crate::platform::host_service::resident::managed() {
            return Ok(());
        }
        tokio::task::spawn_blocking(super::displays::recovery::recover_abandoned).await??;
        if capture::needs_consent() {
            // Opening capture would ask the local user to share the screen
            // (the ScreenCast portal before its first grant); login must not
            // wait on that. An authorized incoming session prepares media.
            tracing::info!("host media probe deferred until an authorized incoming session");
            return Ok(());
        }
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
                terminal,
                remote_power,
                wol,
                terminal_local,
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
                state.terminal = terminal;
                state.terminal_local = terminal && terminal_local;
                state.remote_power = remote_power;
                state.wol = wol;
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
        anyhow::ensure!(!self.guest && lock(&self.ownership).active, "请先登录");
        use crate::platform::host_service::resident;
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
            "当前账号不可启用局域网唤醒协助"
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
        state.status.power_message = Some("正在结束连接并执行电源操作…".into());
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
    /// Whether the controller may open a shell on this machine.
    ///
    /// Off by default: a terminal is an interactive shell, so it is a larger
    /// grant than screen control and must be turned on deliberately.
    pub(crate) fn terminal_allowed(&self) -> bool {
        lock(&self.ownership).terminal
    }
    pub(crate) fn set_terminal_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        {
            let mut state = lock(&self.ownership);
            anyhow::ensure!(state.active, "账号会话已结束");
            if state.terminal != allowed {
                state.terminal = allowed;
                // The local-session grant hangs off this one. It does not
                // come back by itself when the terminal is turned on again:
                // that asks for its own confirmation.
                if !allowed {
                    state.terminal_local = false;
                }
                state.settings_revision = state.settings_revision.wrapping_add(1);
                state.dirty = true;
                state.status.saving = true;
                state.status.settings_error = None;
            }
        }
        // Turning the terminal off ends the shells it granted, connected or
        // not; the live terminal connection closes on its next pump tick.
        if !allowed {
            super::terminal::end_login(&self.account);
        }
        Ok(())
    }
    /// Whether a controller may also see and attach to the shells the local
    /// user opened on this machine (the sessions page, `OpenUUYC terminal`).
    ///
    /// A second switch under the terminal, off by default: the terminal
    /// grant lets the controller run commands, this one lets it watch and
    /// type into a terminal the local user is working in. Only effective
    /// while the terminal itself is allowed.
    pub(crate) fn terminal_local_allowed(&self) -> bool {
        let state = lock(&self.ownership);
        state.terminal && state.terminal_local
    }
    pub(crate) fn set_terminal_local_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "账号会话已结束");
        // Cannot be on without the terminal it extends.
        let allowed = allowed && state.terminal;
        if state.terminal_local != allowed {
            state.terminal_local = allowed;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        // Turning it off leaves the local shells running (they are the local
        // user's); every terminal connection lets go of them on its next
        // pump tick (`terminal::bind`).
        Ok(())
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
                let devices = crate::platform::loopback::Devices::new()?;
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
            // A shell is remote control too; it does not outlive the switch.
            super::terminal::end_login(&self.account);
            self.power.invalidate();
            if !crate::platform::host_service::resident::managed() {
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
                terminal,
                remote_power,
                wol,
                terminal_local,
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
                    state.terminal,
                    state.remote_power,
                    state.wol,
                    state.terminal_local,
                )
            };
            let result = if crate::platform::host_service::resident::managed() {
                crate::platform::host_service::resident::request(
                    crate::platform::host_service::resident::Request::Settings {
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
                        terminal,
                        terminal_local,
                        remote_power,
                        wol,
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
                        terminal,
                        remote_power,
                        wol,
                        terminal_local,
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
        if crate::platform::host_service::resident::managed() {
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
        let clear_update = !state.updating && !crate::platform::host_service::resident::managed();
        state.active = false;
        state.permission = state.permission.wrapping_add(1);
        finish_session(&mut state);
        state.status.ready = false;
        state.status.message = "本机离线".into();
        self.desired.send_replace(None);
        drop(state);
        // Every way a login generation ends (sign-out, another account,
        // unbinding, an expired credential) comes through here; its shells
        // end with it rather than outliving the login that opened them.
        super::terminal::end_login(&self.account);
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
        let managed = crate::platform::host_service::resident::managed();
        let disconnected = disconnect_session(&mut lock(&self.ownership), session, managed);
        // The local user sent the controller away; its shells go with it.
        if disconnected {
            super::terminal::end_login(&self.account);
        }
        disconnected
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
        status
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        Self::update_locked(&mut lock(&self.ownership), ready, connected, message.into());
    }
    fn update_locked(state: &mut Ownership, ready: bool, connected: bool, message: String) {
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
        }
    }
    pub(crate) fn last_controlled_interval(&self) -> i64 {
        lock(&self.ownership)
            .last_controlled
            .map_or(-1, |at| at.elapsed().as_secs().min(i64::MAX as u64) as i64)
    }
}

#[derive(Clone)]
pub(crate) struct Lease {
    handle: Handle,
    permission: u64,
    media: Option<u64>,
    track: usize,
    stream_generation: Option<u64>,
    stop: Option<tokio_util::sync::CancellationToken>,
    assistance: Option<tokio_util::sync::CancellationToken>,
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
    pub(crate) fn controller(&self, device_id: &str, client_id: &str, terminal: bool) {
        self.modify(|state| {
            let session_id = state
                .status
                .connection
                .as_ref()
                .map(|c| c.session_id.clone())
                .unwrap_or_default();
            state.status.connection = Some(ConnectionInfo {
                session_id,
                device_id: if crate::account::api::validate_device_id(device_id).is_ok() {
                    device_id.to_owned()
                } else {
                    String::new()
                },
                client_id: client_id.to_owned(),
                terminal,
                ..Default::default()
            });
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
    /// Explicit extra-display requests are account-only. This is separate
    /// from resolution conversion, headless recovery and releasing a screen.
    pub(crate) fn display_expansion_allowed(&self) -> bool {
        self.requested() && !self.is_assistance()
    }
    pub(crate) fn set_update_notice(&self, notice: Arc<super::peer::UpdateNotice>) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            state.update_notice = Some(notice);
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
    /// The account this lease acts for. Terminal sessions are scoped to it so
    /// a second account controlling the same machine cannot reach them.
    pub(crate) fn account(&self) -> &str {
        &self.handle.account
    }
    pub(crate) fn begin_stream(&self, track: usize) -> anyhow::Result<Self> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        let generation = state.stream_generations.entry(track).or_default();
        *generation = generation.wrapping_add(1);
        let mut lease = self.clone();
        lease.track = track;
        lease.stream_generation = Some(*generation);
        Ok(lease)
    }
    pub(crate) fn clear_stream(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            state.status.streams.remove(&self.track);
            let generation = state.stream_generations.entry(self.track).or_default();
            *generation = generation.wrapping_add(1);
            update_media_summary(&mut state.status);
        }
    }
    pub(crate) fn video(&self, encoding: Option<ActiveEncoding>) {
        self.modify(|state| {
            state.status.streams.entry(self.track).or_default().video = encoding;
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) async fn prepare_media(
        &self,
        screen: capture::Screen,
        gpu_compatibility: bool,
    ) -> anyhow::Result<Arc<super::desktop::Capabilities>> {
        let lease = self.clone();
        self.handle
            .media
            .prepare(screen, gpu_compatibility, move || lease.requested())
            .await
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
        state.active
            && state.allowed
            && self.permission == state.permission
            && self.media.is_none_or(|m| m == state.media)
            && self
                .stream_generation
                .is_none_or(|g| state.stream_generations.get(&self.track) == Some(&g))
    }
    fn modify(&self, update: impl FnOnce(&mut Ownership)) {
        let mut state = lock(&self.handle.ownership);
        if self.current(&state) {
            update(&mut state);
        }
    }
    pub(crate) fn peer_lease(&self) -> anyhow::Result<SessionLease> {
        let mut state = lock(&self.handle.ownership);
        anyhow::ensure!(self.current(&state), "本次被控许可已失效");
        anyhow::ensure!(!state.updating, "被控端正在更新，请稍后重新连接");
        anyhow::ensure!(!state.power_executing, "被控端正在执行电源操作");
        state.media = state.media.wrapping_add(1);
        finish_session(&mut state);
        state.status.session_active = true;
        state.status.connection = Some(ConnectionInfo {
            session_id: uuid::Uuid::new_v4().simple().to_string(),
            ..Default::default()
        });
        state.status.assistance = self.is_assistance();
        state.status.error = None;
        Ok(SessionLease(Self {
            handle: self.handle.clone(),
            permission: self.permission,
            media: Some(state.media),
            track: 0,
            stream_generation: None,
            stop: None,
            assistance: self.assistance.clone(),
        }))
    }
    pub(crate) fn finish(&self) {
        let mut state = lock(&self.handle.ownership);
        if self.current_owner(&state) {
            if self.media.is_some() {
                state.media = state.media.wrapping_add(1);
            }
            finish_session(&mut state);
            state.status.message = "等待连接".into();
        }
    }
    pub(crate) fn source(&self, screen: &capture::Screen, backend: &str) {
        self.modify(|state| {
            let stream = state.status.streams.entry(self.track).or_default();
            stream.capture = Some(backend.into());
            stream.screen = Some(screen.clone());
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) fn encoder(
        &self,
        implementation: i32,
        maximum: (u32, u32),
        screen: &capture::Screen,
        capture: &str,
    ) {
        self.modify(|state| {
            let stream = state.status.streams.entry(self.track).or_default();
            stream.encoder = Some((implementation, maximum));
            stream.capture = Some(capture.into());
            stream.screen = Some(screen.clone());
            update_media_summary(&mut state.status);
        });
    }
    pub(crate) fn requested(&self) -> bool {
        self.current(&lock(&self.handle.ownership))
    }
    pub(crate) fn updating(&self) -> bool {
        lock(&self.handle.ownership).updating
    }
    pub(crate) fn update(&self, ready: bool, connected: bool, message: impl Into<String>) {
        self.modify(|state| Handle::update_locked(state, ready, connected, message.into()));
    }
    pub(crate) fn frame(&self) {
        self.modify(|state| {
            state.status.error = None;
            state.status.message = "正在被远程访问".into();
        });
    }
    pub(crate) fn audio_status(&self, audio: super::audio::Status) {
        self.modify(|state| state.status.audio = audio);
    }
    pub(crate) fn clipboard_settings(&self) -> super::clipboard::Settings {
        self.handle.clipboard_settings()
    }
    pub(crate) fn port_changes(&self) -> watch::Receiver<u64> {
        self.handle.port_changed.subscribe()
    }
    pub(crate) fn port_policy(&self) -> (bool, u64) {
        let state = lock(&self.handle.ownership);
        (
            self.current(&state)
                && self.assistance.is_none()
                && !self.handle.guest
                && state.port_mapping,
            state.port_revision,
        )
    }
    /// Whether this lease may open a shell on the controlled machine.
    ///
    /// A terminal is a shell, so it is refused for guest assistance and for
    /// a controller that is not currently the active one, exactly like the
    /// other privileged grants.
    pub(crate) fn terminal_allowed(&self) -> bool {
        let state = lock(&self.handle.ownership);
        self.current(&state) && self.assistance.is_none() && !self.handle.guest && state.terminal
    }
    /// Whether this lease may also reach the shells opened on this machine;
    /// see [`Handle::terminal_local_allowed`].
    pub(crate) fn terminal_local_allowed(&self) -> bool {
        let state = lock(&self.handle.ownership);
        self.current(&state)
            && self.assistance.is_none()
            && !self.handle.guest
            && state.terminal
            && state.terminal_local
    }
    /// Records whether this terminal connection is attached to a shell opened
    /// on this machine, for the access notice.
    pub(crate) fn terminal_on_local(&self, attached: bool) {
        self.modify(|state| {
            if let Some(connection) = state.status.connection.as_mut() {
                connection.local_terminal = attached;
            }
        });
    }
    pub(crate) fn port_status(&self, status: crate::features::port_mapping::host::Status) {
        self.modify(|s| s.status.ports = status);
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
            && self.assistance.is_none()
            && !self.handle.guest
            && self.handle.file_transfer_allowed()
    }
    pub(crate) fn file_scope(&self) -> String {
        let state = lock(&self.handle.ownership);
        format!(
            "{}:{}",
            self.handle.scope,
            state
                .status
                .connection
                .as_ref()
                .map_or("", |c| c.device_id.as_str())
        )
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
        self.modify(|s| s.status.files = status);
    }
    pub(crate) fn clipboard_status(&self, status: super::clipboard::Status) {
        self.modify(|s| s.status.clipboard = status);
    }
    pub(crate) fn microphone_status(&self, microphone: super::microphone::Status) {
        self.modify(|state| state.status.microphone = microphone);
    }
    pub(crate) fn input_status(&self, backend: Option<&str>, error: Option<String>) {
        self.modify(|state| {
            // Input failures otherwise only change a status field; record each
            // transition so a controller that "cannot move anything" can be
            // explained from the log.
            if state.status.input_backend.as_deref() != backend {
                tracing::info!(backend, "host input backend");
            }
            if state.status.input_error != error {
                match &error {
                    Some(error) => tracing::warn!(backend, %error, "host input failed"),
                    None => tracing::info!(backend, "host input recovered"),
                }
            }
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
