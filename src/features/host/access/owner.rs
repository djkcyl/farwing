//! What the owner set: permissions and per-feature settings, loaded and saved.
use super::*;

impl Handle {
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
    pub(super) fn load_store(account: &str, device: &str, store: anyhow::Result<settings::Store>) -> Self {
        use sha2::{Digest, Sha256};
        let mut handle = Self::default();
        handle.scope = format!("{:x}", Sha256::digest(format!("{account}\0{device}")));
        let result = store.and_then(|store| {
            handle.store = Some(store.clone());
            store.load()
        });
        match result {
            Ok((settings, assistance)) => {
                let _ = handle.assistance.configure(assistance);
                handle.set_allowed(settings.allow_control);
                let mut state = lock(&handle.ownership);
                // An unavailable encoder choice must remain editable without
                // discarding unrelated saved permissions and device settings.
                state.status.settings_error =
                    settings.encoding.validate().err().map(|error| error.to_string());
                state.apply_settings(settings);
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
            camera: state.camera.allowed,
            listen: state.camera.listen,
        }
    }
    pub(crate) fn camera_settings(&self) -> super::settings::CameraSettings {
        lock(&self.ownership).camera.clone()
    }
    /// Whether others may see the cameras and hear the microphone, which
    /// microphone, and the owner's camera parameters.
    pub(crate) fn set_camera_settings(&self, camera: super::settings::CameraSettings) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
        if state.camera != camera {
            state.camera = camera;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn max_participants(&self) -> u32 {
        lock(&self.ownership).max_participants
    }
    /// Concurrent participants (1..=16). Connected participants beyond a
    /// lowered limit stay; only new ones are refused.
    pub(crate) fn set_max_participants(&self, limit: u32) -> anyhow::Result<()> {
        let limit = limit.clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS);
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
        if state.max_participants != limit {
            state.max_participants = limit;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn forwarding_mode(&self) -> super::super::forwarding::Mode {
        lock(&self.ownership).forwarding
    }
    /// How this host's picture passes the server's forwarder; a change
    /// applies to the next forwarding room.
    pub(crate) fn set_forwarding_mode(&self, mode: super::super::forwarding::Mode) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
        if state.forwarding != mode {
            state.forwarding = mode;
            super::super::forwarding::set_mode(mode);
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn audio_device(&self) -> Option<super::super::audio::Device> {
        lock(&self.ownership).audio_device.borrow().clone()
    }
    pub(crate) fn audio_defaults(&self) -> super::super::audio::DefaultDevices {
        lock(&self.ownership).audio_defaults
    }
    /// The desktop's sound leaves out this device's calls.
    pub(crate) fn exclude_calls(&self) -> bool {
        lock(&self.ownership).exclude_calls
    }
    pub(crate) fn set_exclude_calls(&self, on: bool) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
        if state.exclude_calls != on {
            state.exclude_calls = on;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn file_transfer_allowed(&self) -> bool {
        lock(&self.ownership).file_transfer
    }
    pub(crate) fn set_file_transfer_allowed(&self, allowed: bool) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
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
    pub(crate) fn wol_setup_status(&self, status: super::super::wol::setup::Status) {
        let mut s = lock(&self.ownership);
        if s.active {
            s.status.wol_setup = status;
        }
    }
    pub(crate) fn wol_setup_action(&self, action: super::super::wol::setup::Action) -> anyhow::Result<()> {
        anyhow::ensure!(!self.guest && lock(&self.ownership).active, "{}", t().common.signed_out);
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
            "{}", t().features.wol_assistance_not_allowed
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
    pub(crate) fn wol_status(&self, revision: u64, status: super::super::wol::Status) {
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
            "{}", t().features.power_not_allowed_for_account
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
            "{}", t().features.power_permission_revoked
        );
        state.power_executing = true;
        state.status.power_message = Some(t().features.power_executing.into());
        Ok(())
    }
    pub(crate) fn power_result(&self, result: Result<(), String>) {
        let mut state = lock(&self.ownership);
        // Windows may allow the user to cancel shutdown for unsaved applications.
        // Do not leave remote access permanently disabled after API acceptance.
        state.power_executing = false;
        match result {
            Ok(()) => state.status.power_message = Some(t().features.power_accepted.into()),
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
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
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
    pub(crate) fn clipboard_settings(&self) -> super::super::clipboard::Settings {
        lock(&self.ownership).clipboard
    }
    pub(crate) fn set_clipboard_settings(
        &self,
        settings: super::super::clipboard::Settings,
    ) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
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
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
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
        selected: super::super::audio::DefaultDevices,
    ) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
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
        device: Option<super::super::audio::Device>,
    ) -> anyhow::Result<()> {
        if let Some(device) = &device {
            device.validate()?;
        }
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
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
    pub(crate) fn audio_devices(&self) -> super::super::audio::Inventory {
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
        state.audio_inventory.error = Some(t().features.audio_devices_read_failed.into());
    }
    pub(crate) async fn refresh_audio_devices(&self) {
        let result =
            tokio::task::spawn_blocking(|| -> anyhow::Result<Vec<super::super::audio::Device>> {
                let devices = crate::platform::windows::loopback::Devices::new()?;
                let mut result: Vec<_> = devices
                    .list()?
                    .into_iter()
                    .map(|device| super::super::audio::Device {
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
            Ok(Err(error)) => list.error = Some((t().features.audio_devices_read_error)(&format!("{error:#}"))),
            Err(_) => list.error = Some(t().features.audio_devices_read_interrupted.into()),
        }
    }
    pub(crate) fn encoding_settings(&self) -> super::super::EncodingSettings {
        lock(&self.ownership).encoding
    }
    pub(crate) fn set_encoding_settings(
        &self,
        encoding: super::super::EncodingSettings,
    ) -> anyhow::Result<()> {
        encoding.validate()?;
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
        if state.encoding != encoding {
            state.encoding = encoding;
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    pub(crate) fn capabilities(&self) -> Option<Arc<super::super::desktop::Capabilities>> {
        self.media.snapshot()
    }
    pub(crate) fn set_allowed(&self, allowed: bool) {
        if !allowed {
            self.power.invalidate();
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
            t().features.host_enabling
        } else {
            t().features.host_disabled
        }
        .into();
        self.desired.send_replace(allowed.then_some(AccessRequest {
            generation: state.permission,
        }));
    }
    /// Whether the last visitor left since the previous call.
    pub(crate) fn take_visit_ended(&self) -> bool {
        std::mem::take(&mut lock(&self.ownership).visit_ended)
    }
    pub(crate) fn set_assistance(&self, settings: super::super::assist::Settings) -> anyhow::Result<()> {
        let mut state = lock(&self.ownership);
        anyhow::ensure!(state.active, "{}", t().features.host_account_session_ended);
        let was_on = self.assistance.snapshot().settings.enabled;
        let on = settings.enabled;
        if self.assistance.configure(settings)? {
            if on != was_on {
                use farwing_protocol::api::device_event::{ACCESS_CODE_OFF, ACCESS_CODE_ON};
                crate::account::records::device_event(
                    if on { ACCESS_CODE_ON } else { ACCESS_CODE_OFF },
                    "",
                    "",
                );
            }
            state.settings_revision = state.settings_revision.wrapping_add(1);
            state.dirty = true;
            state.status.saving = true;
            state.status.settings_error = None;
        }
        Ok(())
    }
    /// The owner's settings as they stand.
    pub(crate) fn host_settings(&self) -> settings::HostSettings {
        lock(&self.ownership).settings()
    }
    /// Change every setting as the owner did, each with its effects on
    /// running sessions; `persist_settings` saves them.
    pub(crate) fn set_host_settings(&self, settings: settings::HostSettings) -> anyhow::Result<()> {
        settings.encoding.validate()?;
        if let Some(device) = &settings.audio_device {
            device.validate()?;
        }
        self.set_encoding_settings(settings.encoding)?;
        self.set_audio_device(settings.audio_device)?;
        self.set_audio_defaults(settings.audio_defaults)?;
        self.set_audio_quality(settings.audio_quality)?;
        self.set_clipboard_settings(settings.clipboard)?;
        self.set_file_transfer_allowed(settings.file_transfer)?;
        self.set_port_mapping_allowed(settings.port_mapping)?;
        self.set_power_allowed(settings.remote_power)?;
        self.set_wol_allowed(settings.wol)?;
        self.set_max_participants(settings.max_participants)?;
        self.set_forwarding_mode(settings.forwarding)?;
        self.set_camera_settings(settings.camera)?;
        self.set_allowed(settings.allow_control);
        Ok(())
    }
    pub(crate) async fn persist_settings(&self) {
        let _saving = self.saving.lock().await;
        loop {
            let (revision, settings, assistance) = {
                let state = lock(&self.ownership);
                if !state.dirty {
                    return;
                }
                (state.settings_revision, state.settings(), self.assistance.settings())
            };
            let result = if crate::platform::windows::host_service::resident::managed() {
                crate::platform::windows::host_service::resident::request(
                    crate::platform::windows::host_service::resident::Request::Settings {
                        account: self.account.clone(),
                        settings,
                        assistance,
                    },
                )
                .await
                .map(|_| ())
            } else if let Some(store) = self.store.clone() {
                tokio::task::spawn_blocking(move || store.save(settings, assistance))
                .await
                .map_err(|_| anyhow::anyhow!(t().features.host_settings_save_interrupted))
                .and_then(|r| r)
            } else {
                Err(anyhow::anyhow!(t().features.host_settings_unavailable))
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
}
