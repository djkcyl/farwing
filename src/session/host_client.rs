//! Online host identity. Guests and accounts share hosting, never account management.
use super::device_session::DeviceHandle;
use crate::account::{
    api::{ApiEnvelope, NrdApi, RoomSession},
    client::AuthenticatedClient,
};
use crate::features::host;
use anyhow::{Result, bail};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

#[derive(Clone)]
pub(crate) struct HostClient {
    pub host: host::Handle,
    identity: Identity,
}
#[derive(Clone)]
enum Identity {
    Account(Arc<AuthenticatedClient>),
    Guest(Arc<Guest>),
    Local(Arc<Local>),
}
struct Guest {
    device: DeviceHandle,
    generation: String,
    api: tokio::sync::Mutex<Option<NrdApi>>,
    ended: CancellationToken,
}
struct Local {
    api: Arc<super::local_host::LocalHostApi>,
    ended: CancellationToken,
}
impl Drop for Guest {
    fn drop(&mut self) {
        self.ended.cancel();
    }
}
impl From<Arc<AuthenticatedClient>> for HostClient {
    fn from(client: Arc<AuthenticatedClient>) -> Self {
        Self {
            host: client.host.clone(),
            identity: Identity::Account(client),
        }
    }
}
impl HostClient {
    pub fn guest(device: DeviceHandle) -> Result<Self> {
        // The installation exists before its first registration returns a device ID.
        let id = device.identity().client_identity()?.client_id;
        let generation = format!("guest:{id}");
        let mut host = host::Handle::load_guest(&id);
        host.bind_account(generation.clone());
        Ok(Self {
            host,
            identity: Identity::Guest(Arc::new(Guest {
                device,
                generation,
                api: tokio::sync::Mutex::new(None),
                ended: CancellationToken::new(),
            })),
        })
    }
    pub(crate) fn local(api: Arc<super::local_host::LocalHostApi>) -> Result<Self> {
        let host = host::Handle::self_host(api.device_id())?;
        Ok(Self {
            host,
            identity: Identity::Local(Arc::new(Local {
                api,
                ended: CancellationToken::new(),
            })),
        })
    }
    pub fn is_guest(&self) -> bool {
        matches!(self.identity, Identity::Guest(_))
    }
    pub(crate) fn is_self_hosted_account(&self) -> bool {
        matches!(&self.identity, Identity::Account(client) if client.is_self_hosted())
    }
    pub(crate) fn is_local(&self) -> bool {
        matches!(self.identity, Identity::Local(_))
    }
    pub(crate) fn membership(&self) -> Option<&crate::account::membership::Membership> {
        match &self.identity {
            Identity::Account(client) => Some(&client.membership),
            Identity::Guest(_) | Identity::Local(_) => None,
        }
    }
    pub(crate) async fn refresh_membership_profile(&self) {
        let Identity::Account(client) = &self.identity else {
            return std::future::pending().await;
        };
        if client.is_self_hosted() {
            return std::future::pending().await;
        }
        let mut refresh_after = Some(std::time::Duration::ZERO);
        let mut rights_updates = client.membership.rights_updates.subscribe();
        loop {
            tokio::select! {
                _ = client.membership.refresh_profile.notified() => {
                    *client.membership.rights.lock().await = None;
                }
                _ = async {
                    match refresh_after {
                        Some(delay)=>tokio::time::sleep(delay).await,
                        None=>std::future::pending().await,
                    }
                } => {}
                changed = rights_updates.changed() => {
                    if changed.is_err() { return; }
                    // A connection can recover the query after this worker's
                    // bounded retries ended. Resume TTL refresh from that table.
                    refresh_after = rights_updates.borrow_and_update().as_ref()
                        .map(|table| table.refresh_after().max(std::time::Duration::from_secs(1)));
                    continue;
                }
            }
            if let Err(error) = client.account_info().await {
                tracing::warn!(%error, "account membership profile refresh failed");
            }
            refresh_after = None;
            for retry in 0..4 {
                match client.account_entitlements().await {
                    Ok(table) => {
                        tracing::info!(
                            version = table.version,
                            features = table.snapshot.schedule.features.len(),
                            expires_at = table.expired_at,
                            checkpoint_at = table.checkpoint_at,
                            "account entitlements verified"
                        );
                        refresh_after =
                            Some(table.refresh_after().max(std::time::Duration::from_secs(1)));
                        break;
                    }
                    Err(error) => {
                        tracing::warn!(%error, "account entitlements query failed");
                        if retry == 3
                            || error
                                .downcast_ref::<crate::account::api::ApiFailure>()
                                .is_some_and(|e| e.invalid_saved_credentials())
                        {
                            break;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs([1, 2, 4][retry])).await;
                    }
                }
            }
        }
    }
    pub(crate) fn controlling_features(
        &self,
    ) -> Option<crate::account::feature_ability::FeatureCatalog> {
        match &self.identity {
            Identity::Account(client) => Some(client.controlling_features()),
            Identity::Guest(_) | Identity::Local(_) => None,
        }
    }
    pub fn generation(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.account_generation(),
            Identity::Guest(c) => c.generation.clone(),
            Identity::Local(c) => format!("self-host:{}", c.api.device_id()),
        }
    }
    pub fn matches_saved(&self, saved: &str) -> bool {
        if self.is_guest() {
            saved.is_empty()
        } else {
            self.generation() == saved
        }
    }
    pub fn device_id(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.device_id(),
            Identity::Guest(c) => {
                c.device
                    .identity()
                    .client_identity()
                    .expect("validated device")
                    .device_id
            }
            Identity::Local(c) => c.api.device_id().to_owned(),
        }
    }
    pub fn presence_key(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.presence_key(),
            Identity::Guest(c) => {
                c.device
                    .identity()
                    .client_identity()
                    .expect("validated installation")
                    .client_id
            }
            Identity::Local(c) => format!("self-host:{}", c.api.device_id()),
        }
    }
    pub fn ended(&self) -> CancellationToken {
        match &self.identity {
            Identity::Account(c) => c.ended(),
            Identity::Guest(c) => c.ended.clone(),
            Identity::Local(c) => c.ended.clone(),
        }
    }
    pub fn is_active(&self) -> bool {
        !self.ended().is_cancelled()
    }
    pub fn retire(&self) {
        self.host.retire();
        match &self.identity {
            Identity::Account(c) => c.retire(),
            Identity::Guest(c) => c.ended.cancel(),
            Identity::Local(c) => c.ended.cancel(),
        }
    }
    pub fn clear_saved_generation(&self) -> Result<()> {
        match &self.identity {
            Identity::Account(c) => c.clear_saved_generation(),
            Identity::Guest(_) | Identity::Local(_) => Ok(()),
        }
    }
    pub async fn close(&self) {
        match &self.identity {
            Identity::Account(c) => c.close().await,
            Identity::Guest(c) => {
                c.ended.cancel();
                c.api.lock().await.take();
            }
            Identity::Local(c) => c.ended.cancel(),
        }
    }
    async fn request<T, F>(&self, request: impl FnOnce(NrdApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        if self.is_local() {
            bail!("self-host mode does not use the account API");
        }
        let Identity::Guest(guest) = &self.identity else {
            let Identity::Account(account) = &self.identity else {
                unreachable!()
            };
            let result = account.request(request).await;
            if result
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<crate::account::api::ApiFailure>())
                .is_some_and(|e| e.code == 1120)
            {
                account.retire();
            }
            return result;
        };
        let operation = async {
            let mut active = guest.api.lock().await;
            if active.is_none() {
                let identity = guest.device.ensure(false).await?;
                let mut api = NrdApi::new(identity.client_identity()?)?;
                let credentials = api.create_guest().await?;
                api.authenticate_guest(credentials)?;
                *active = Some(api);
            }
            let api = active.as_ref().unwrap().clone();
            drop(active);
            let response = request(api).await?;
            if response.code == 1120 {
                self.retire();
            }
            response.into_data()
        };
        tokio::select! { biased; _ = guest.ended.cancelled() => bail!("游客协助已结束"), result = operation => result }
    }
    pub(crate) async fn wol_info(
        &self,
        info: crate::features::host::wol::packet::LanInfo,
    ) -> Result<bool> {
        if self.is_guest() || self.is_local() {
            bail!("游客不支持局域网唤醒协助");
        }
        self.request(|api| async move { api.wol_info(info).await })
            .await
            .map(|r| r.support_wol)
    }
    pub(crate) async fn wol_enabled(&self) -> Result<bool> {
        anyhow::ensure!(!self.is_guest() && !self.is_local(), "当前模式不支持局域网唤醒");
        let devices = self
            .request(|api| async move { api.list_devices().await })
            .await?;
        anyhow::ensure!(
            devices.current_device.device_id == self.device_id(),
            "本机设备身份已改变"
        );
        Ok(devices.current_device.support_wol)
    }
    pub(crate) async fn set_wol_enabled(&self, enabled: bool) -> Result<()> {
        anyhow::ensure!(!self.is_guest() && !self.is_local(), "当前模式不支持局域网唤醒");
        self.request(|api| async move { api.set_wol(enabled).await })
            .await
    }
    pub(crate) async fn power_response(
        &self,
        call_id: String,
        code: i32,
        message: String,
    ) -> Result<()> {
        if self.is_guest() || self.is_local() {
            bail!("当前模式不支持远程电源操作");
        }
        self.request(|api| async move { api.host_power_response(call_id, code, message).await })
            .await
    }
    pub async fn create_host_room(&self, interval: i64) -> Result<RoomSession> {
        if let Identity::Local(local) = &self.identity {
            return local.api.create_room().await;
        }
        let guest = self.is_guest();
        let room: RoomSession = self
            .request(|api| async move {
                if guest {
                    api.create_guest_room(interval).await
                } else {
                    api.create_room(interval).await
                }
            })
            .await?;
        room.validate()?;
        Ok(room)
    }
    pub async fn set_controllable(&self, allowed: bool) -> Result<()> {
        match &self.identity {
            Identity::Account(c) => c.set_controllable(allowed).await,
            Identity::Guest(c) => {
                self.request(|api| async move { api.set_controllable(allowed).await })
                    .await?;
                c.device.set_controllable(allowed).await
            }
            Identity::Local(c) => c.api.set_controllable(allowed).await,
        }
    }
    pub async fn host_assist_identity(&self) -> Result<String> {
        anyhow::ensure!(!self.is_local(), "self-host mode does not support assistance");
        let value = self
            .request(|api| async move { api.host_assist_identity().await })
            .await?;
        crate::account::assist::normalize_connect_id(&value.connect_id)
    }
    pub async fn host_assist_mode(
        &self,
        id: &str,
        allowed: bool,
        mode: &'static str,
    ) -> Result<()> {
        anyhow::ensure!(!self.is_local(), "self-host mode does not support assistance");
        self.request(|api| async move { api.host_assist_mode(id, allowed, mode).await })
            .await?;
        Ok(())
    }
    pub async fn host_assist_sign(
        &self,
        id: &str,
        allowed: bool,
        sign: &str,
        backup: Option<&str>,
        mode: &'static str,
        confirm: bool,
    ) -> Result<()> {
        anyhow::ensure!(!self.is_local(), "self-host mode does not support assistance");
        self.request(|api| async move {
            api.host_assist_sign(id, allowed, sign, backup, mode, confirm)
                .await
        })
        .await?;
        Ok(())
    }
    pub async fn host_assist_confirm(&self, id: &str, allowed: bool, password: bool) -> Result<()> {
        anyhow::ensure!(!self.is_local(), "self-host mode does not support assistance");
        self.request(|api| async move { api.host_assist_confirm(id, allowed, password).await })
            .await?;
        Ok(())
    }
    pub async fn host_input_configuration(&self) -> Result<host::input::config::Configuration> {
        if self.is_local() {
            return Ok(host::input::config::Configuration::default());
        }
        let configs = self
            .request(|api| async move {
                api.query_configures(&[
                    ("app_white_list".into(), String::new()),
                    ("win_keylock_optimize".into(), String::new()),
                ])
                .await
            })
            .await?;
        host::input::config::Configuration::from_response(configs)
    }
}
