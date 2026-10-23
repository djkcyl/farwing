//! Online host identity. Guests and accounts share hosting, never account management.
use super::device_session::DeviceHandle;
use crate::account::{
    api::{RealtimeTicket, ServerApi},
    client::AuthenticatedClient,
};
use crate::features::host;
use crate::i18n::t;
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
    /// No server at all: direct connections only
    /// (`docs/protocol/v2-design.md` §12.7).
    Local(Arc<Local>),
}
struct Local {
    device: crate::transport::direct::LocalDevice,
    generation: String,
    ended: CancellationToken,
}
struct Guest {
    device: DeviceHandle,
    generation: String,
    api: tokio::sync::Mutex<Option<ServerApi>>,
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
    /// This installation signed out: a guest of the server, or a local host
    /// when there is no server or only direct connections are allowed (a
    /// guest would wait for a server and never accept a session).
    pub fn signed_out(device: DeviceHandle) -> Result<Self> {
        if Self::serverless() {
            Self::local()
        } else {
            Self::guest(device)
        }
    }
    /// A signed-out host does not use a server.
    pub fn serverless() -> bool {
        Self::serverless_setting().unwrap_or(true)
    }
    /// [`Self::serverless`], or `None` when the server setting cannot be
    /// read just now.
    pub fn serverless_setting() -> Option<bool> {
        let server = crate::account::server::ServerConfig::load().ok()?;
        Some(server.is_none() || crate::transport::nat::settings::current().serverless)
    }
    /// A host without a server: this profile's device key and settings, the
    /// device ID derived from the key unless it is registered.
    pub fn local() -> Result<Self> {
        let device = crate::transport::direct::local_device()?;
        // The same signed-out installation as a guest: the GUI and the
        // background service may each decide on a server in between.
        let generation = format!("guest:{}", device.identity.client_id);
        let mut host = host::Handle::load_guest(&device.identity.client_id);
        host.bind_account(generation.clone());
        Ok(Self {
            host,
            identity: Identity::Local(Arc::new(Local {
                device,
                generation,
                ended: CancellationToken::new(),
            })),
        })
    }
    pub fn is_guest(&self) -> bool {
        matches!(self.identity, Identity::Guest(_))
    }
    pub fn is_local(&self) -> bool {
        matches!(self.identity, Identity::Local(_))
    }
    /// No server is used: a local host, or "direct connections only".
    pub(crate) fn offline(&self) -> bool {
        matches!(self.identity, Identity::Local(_))
            || crate::transport::nat::settings::current().serverless
    }
    /// The logged-in account, if any.
    pub(crate) fn account_user_id(&self) -> Option<String> {
        match &self.identity {
            Identity::Account(c) => Some(c.user_id().to_owned()),
            _ => None,
        }
    }
    /// Refresh the account's device list and the friend devices that may
    /// control this one (the caches direct connections use).
    pub(crate) async fn refresh_account_cache(&self) -> Result<()> {
        match &self.identity {
            Identity::Account(c) if !self.offline() => {
                c.list_devices().await?;
                c.refresh_friend_access().await
            }
            _ => bail!("{}", t().session.error_no_account_connection),
        }
    }
    /// This device as a direct peer (its ID derived from its key).
    pub(crate) fn direct_device(&self) -> Result<crate::transport::direct::LocalDevice> {
        match &self.identity {
            Identity::Local(local) => Ok(local.device.clone()),
            _ => crate::transport::direct::local_device(),
        }
    }
    pub fn generation(&self) -> String {
        match &self.identity {
            Identity::Account(c) => c.account_generation(),
            Identity::Guest(c) => c.generation.clone(),
            Identity::Local(c) => c.generation.clone(),
        }
    }
    pub fn matches_saved(&self, saved: &str) -> bool {
        if matches!(self.identity, Identity::Local(_)) {
            return true;
        }
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
            Identity::Local(c) => c.device.identity.device_id.clone(),
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
            Identity::Local(c) => c.device.identity.client_id.clone(),
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
    async fn request<T, F>(&self, request: impl FnOnce(ServerApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        let Identity::Guest(guest) = &self.identity else {
            let Identity::Account(account) = &self.identity else {
                bail!("{}", t().session.error_direct_only_no_server);
            };
            // The host waits for an unreachable server and carries on.
            return account.request_waiting(request).await;
        };
        let operation = async {
            let mut active = guest.api.lock().await;
            if active.is_none() {
                // A guest host has a registered device but no account; its
                // guest session lives in memory only.
                let identity = guest.device.ensure(false).await?;
                let mut api = ServerApi::for_identity(identity.client_identity()?)?;
                let session = api.create_guest().await?;
                api.authenticate_guest(session)?;
                *active = Some(api);
            }
            let api = active.as_ref().unwrap().clone();
            drop(active);
            let response = request(api).await;
            if response
                .as_ref()
                .err()
                .and_then(|e| e.downcast_ref::<crate::account::api::ApiFailure>())
                .is_some_and(|failure| failure.retires_account() || failure.invalid_device())
            {
                self.retire();
            }
            response
        };
        tokio::select! { biased; _ = guest.ended.cancelled() => bail!("{}", t().session.error_guest_host_ended), result = operation => result }
    }
    pub(crate) async fn wol_info(
        &self,
        info: crate::features::host::wol::packet::LanInfo,
    ) -> Result<bool> {
        if self.is_guest() {
            bail!("{}", t().session.error_guest_no_wol);
        }
        self.request(|api| async move { api.wol_info(info).await })
            .await
    }
    pub(crate) async fn wol_enabled(&self) -> Result<bool> {
        anyhow::ensure!(!self.is_guest(), "{}", t().common.signed_out);
        let devices = self
            .request(|api| async move { api.list_devices().await })
            .await?;
        anyhow::ensure!(
            devices.current_device.device_id == self.device_id(),
            "{}",
            t().session.error_local_identity_changed
        );
        Ok(devices.current_device.support_wol)
    }
    pub(crate) async fn power_response(
        &self,
        request_id: String,
        code: i32,
        message: String,
    ) -> Result<()> {
        if self.is_guest() {
            bail!("{}", t().session.error_guest_no_power);
        }
        self.request(|api| async move { api.host_power_response(&request_id, code, message).await })
            .await
    }
    /// The server's multi-viewer forwarding, if it forwards.
    pub(crate) async fn forwarding(&self) -> Result<Option<farwing_protocol::api::Forwarding>> {
        self.request(|api| async move { api.server_info().await })
            .await
            .map(|info| info.features.forwarding)
    }
    pub(crate) async fn sfu_create_room(
        &self,
        sessions: Vec<String>,
        mode: farwing_protocol::api::ForwardMode,
        offer: String,
    ) -> Result<farwing_protocol::api::RoomCreated> {
        self.request(|api| async move { api.sfu_create_room(sessions, mode, offer).await })
            .await
    }
    pub(crate) async fn sfu_update_room(&self, room: String, sessions: Vec<String>) -> Result<()> {
        self.request(|api| async move { api.sfu_update_room(&room, Some(sessions), None).await })
            .await
            .map(|_| ())
    }
    pub(crate) async fn sfu_close_room(&self, room: String) -> Result<()> {
        self.request(|api| async move { api.sfu_close_room(&room).await })
            .await
    }
    /// A single-use ticket for this device's presence connection.
    pub async fn presence_ticket(&self) -> Result<RealtimeTicket> {
        self.request(|api| async move { api.presence_ticket().await })
            .await
    }
    /// This installation's identity (device key) for signing session SDPs.
    pub(crate) fn client_identity(&self) -> Result<crate::account::api::ClientIdentity> {
        match &self.identity {
            Identity::Account(c) => c.client_identity(),
            Identity::Guest(c) => c.device.identity().client_identity(),
            Identity::Local(c) => Ok(c.device.identity.clone()),
        }
    }
    /// The device policy this installation last published.
    pub(crate) fn published_policy(&self) -> crate::account::api::DevicePolicy {
        match &self.identity {
            Identity::Account(c) => c.published_policy(),
            Identity::Guest(c) => c.device.identity().policy(),
            Identity::Local(_) => self.host.device_policy(),
        }
    }
    /// The policy the local settings call for.
    pub(crate) fn desired_policy(&self) -> crate::account::api::DevicePolicy {
        crate::account::api::DevicePolicy {
            wol: self.published_policy().wol,
            ..self.host.device_policy()
        }
    }
    /// Publish what the owner allows on this device.
    pub(crate) async fn publish(&self, policy: crate::account::api::DevicePolicy) -> Result<()> {
        match &self.identity {
            Identity::Account(c) => c.set_policy(policy).await,
            Identity::Guest(c) => {
                self.request(|api| async move { api.set_policy(&policy).await })
                    .await?;
                c.device.set_policy(policy).await
            }
            // Nothing to publish without a server.
            Identity::Local(_) => Ok(()),
        }
    }
    /// Publish the local settings with remote control `allowed` or not.
    pub async fn publish_policy(&self, allowed: bool) -> Result<()> {
        self.publish(crate::account::api::DevicePolicy {
            control: allowed,
            ..self.desired_policy()
        })
        .await
    }
    /// Remote boot of this device (`policy.wol`).
    pub(crate) async fn set_wol_enabled(&self, enabled: bool) -> Result<()> {
        anyhow::ensure!(!self.is_guest(), "{}", t().common.signed_out);
        self.publish(crate::account::api::DevicePolicy {
            wol: enabled,
            ..self.desired_policy()
        })
        .await
    }
    pub async fn host_assist_identity(&self) -> Result<String> {
        let value = self
            .request(|api| async move { api.host_assist_identity().await })
            .await?;
        crate::account::assist::normalize_connect_id(&value)
    }
    pub async fn host_assist_pake(
        &self,
        id: &str,
        reply: farwing_protocol::api::AssistPakeReply,
    ) -> Result<()> {
        self.request(|api| async move { api.host_assist_pake(id, &reply).await })
            .await
    }
    pub async fn host_assist_decision(
        &self,
        id: &str,
        decision: farwing_protocol::api::AssistDecision,
    ) -> Result<()> {
        self.request(|api| async move { api.host_assist_decision(id, &decision).await })
            .await
    }
}
