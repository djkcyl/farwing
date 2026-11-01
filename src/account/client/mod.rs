//! One authenticated account generation. Owners decide when it ends; a
//! generic REST error is not a global logout notification.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;
use crate::i18n::t;

use crate::account::api::{
    ApiFailure, DeviceGroups, DeviceList, DevicePolicy, Purpose, RealtimeTicket, ServerApi,
};
use farwing_protocol::api as wire;
use crate::account::auth::{KeyringSessionStore, LoginSession, SessionStore};
use crate::account::server::ServerConfig;
use crate::account::session_restore::{self, RestoreTrigger};
use crate::session::device_session::{DeviceHandle, DeviceRuntime};

#[derive(Debug)]
pub(crate) struct NoSavedSession;

/// The local "account" of a guest client's per-user records (viewing
/// settings of hosts reached without an account).
const GUEST_USER: &str = "guest";

impl std::fmt::Display for NoSavedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(t().session.not_signed_in_to_server)
    }
}
impl std::error::Error for NoSavedSession {}

mod assist;
mod publication;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RestorationStage {
    Device,
    Account,
    Ready,
}

pub struct AuthenticatedClient {
    pub(crate) host: crate::features::host::Handle,
    /// A device without an account (guest token): it may only ask hosts to
    /// admit it (`docs/access-model.md` §2.5); nothing account-related.
    guest: bool,
    api: Mutex<Option<ServerApi>>,
    session: LoginSession,
    device: DeviceHandle,
    owned_device: tokio::sync::Mutex<Option<DeviceRuntime>>,
    session_store: KeyringSessionStore,
    ended: CancellationToken,
    validated: tokio::sync::Mutex<RestoreState>,
    restore_trigger: RestoreTrigger,
    restore_progress: tokio::sync::watch::Sender<RestorationStage>,
    /// The server was out of reach at the last attempt (restoring the login
    /// or any request since).
    unreachable: tokio::sync::watch::Sender<bool>,
    account_name: Mutex<String>,
    publication: Mutex<publication::Sync>,
    features: crate::account::feature_ability::FeatureCatalog,
}

#[derive(Default)]
struct RestoreState {
    ready: bool,
    failure: Option<RestoreFailure>,
}

#[derive(Clone)]
enum RestoreFailure {
    Api(ApiFailure),
    Local(String),
}
impl RestoreFailure {
    fn error(&self) -> anyhow::Error {
        match self {
            Self::Api(error) => error.clone().into(),
            Self::Local(message) => anyhow::anyhow!("{}", (t().session.local_init_failed)(message)),
        }
    }
}

pub struct LogoutOutcome {
    pub remote_error: Option<String>,
    pub local_error: Option<String>,
}

impl AuthenticatedClient {
    pub fn from_saved_session() -> Result<Self> {
        let runtime = DeviceRuntime::start()?;
        Self::load(runtime.handle(), Some(runtime))
    }

    pub(crate) fn from_saved_session_with_device(device: DeviceHandle) -> Result<Self> {
        Self::load(device, None)
    }

    fn load(device: DeviceHandle, owned_device: Option<DeviceRuntime>) -> Result<Self> {
        let session_store = KeyringSessionStore::new()?;
        let session = session_store.load()?.ok_or(NoSavedSession)?;
        // Without a configured server a saved login cannot be used; the
        // user signs in again (first run, or after `server set` was cleared).
        let server = ServerConfig::load()?.ok_or(NoSavedSession)?;
        if session.server() != server.key() {
            // A login belongs to the server that issued it.
            return Err(NoSavedSession.into());
        }
        let identity = device.identity().client_identity()?;
        let mut host = crate::features::host::Handle::load(session.user_id(), &identity.device_id);
        host.bind_account(session.generation());
        let mut api = ServerApi::new(&server)?;
        api.set_identity(identity);
        api.set_access_token(Some(session.token()))?;
        let ended = CancellationToken::new();

        let restore_trigger = if owned_device.is_some()
            || crate::platform::windows::host_service::resident::is_owner()
        {
            RestoreTrigger::DeviceStartup
        } else {
            RestoreTrigger::ExplicitLogin
        };
        // As the restored account shows it: the nickname, else the username.
        let account_name = Mutex::new(if session.nickname().is_empty() {
            session.username().to_owned()
        } else {
            session.nickname().to_owned()
        });
        Ok(Self {
            host,
            guest: false,
            api: Mutex::new(Some(api)),
            session,
            device,
            owned_device: tokio::sync::Mutex::new(owned_device),
            session_store,
            ended,
            validated: tokio::sync::Mutex::new(RestoreState {
                ready: false,
                failure: None,
            }),
            restore_trigger,
            restore_progress: tokio::sync::watch::channel(RestorationStage::Device).0,
            unreachable: tokio::sync::watch::channel(false).0,
            account_name,
            publication: Mutex::new(publication::Sync::default()),
            features: crate::account::feature_ability::FeatureCatalog::default(),
        })
    }

    /// This device without an account, for connecting through the server
    /// to hosts that trust it or with an access code. Its guest token is
    /// requested on first use.
    pub(crate) fn guest(device: DeviceHandle) -> Result<Self> {
        let server = ServerConfig::load()?.context(t().session.no_server_set)?;
        let identity = device.identity().client_identity()?;
        // A placeholder login: never saved, never sent (the guest token is).
        let session = LoginSession::new(
            server.key(),
            "guest",
            GUEST_USER,
            "",
            "",
        )?;
        let mut api = ServerApi::new(&server)?;
        api.set_identity(identity);
        Ok(Self {
            host: crate::features::host::Handle::default(),
            guest: true,
            api: Mutex::new(Some(api)),
            session,
            device,
            owned_device: tokio::sync::Mutex::new(None),
            session_store: KeyringSessionStore::new()?,
            ended: CancellationToken::new(),
            validated: tokio::sync::Mutex::new(RestoreState {
                ready: false,
                failure: None,
            }),
            restore_trigger: RestoreTrigger::ExplicitLogin,
            restore_progress: tokio::sync::watch::channel(RestorationStage::Device).0,
            unreachable: tokio::sync::watch::channel(false).0,
            account_name: Mutex::new(String::new()),
            publication: Mutex::new(publication::Sync::default()),
            features: crate::account::feature_ability::FeatureCatalog::default(),
        })
    }

    /// Like [`Self::guest`], with a device runtime of its own.
    pub(crate) fn guest_with_runtime() -> Result<Self> {
        let runtime = DeviceRuntime::start()?;
        let client = Self::guest(runtime.handle())?;
        *client.owned_device.try_lock().expect("new client") = Some(runtime);
        Ok(client)
    }

    /// A device without an account (see [`Self::guest`]).
    pub(crate) fn is_guest(&self) -> bool {
        self.guest
    }

    pub(crate) fn account_generation(&self) -> String {
        self.session.generation()
    }

    /// This installation's identity (device key) for signing session SDPs.
    pub(crate) fn client_identity(&self) -> Result<crate::account::api::ClientIdentity> {
        self.device.identity().client_identity()
    }

    /// The device policy last published by this installation.
    pub(crate) fn published_policy(&self) -> DevicePolicy {
        self.device.identity().policy()
    }

    pub fn device_id(&self) -> String {
        self.device
            .identity()
            .client_identity()
            .expect("validated native identity")
            .device_id
    }
    pub(crate) fn presence_key(&self) -> String {
        self.device
            .identity()
            .client_identity()
            .expect("validated native identity")
            .client_id
    }
    pub(crate) fn viewing_settings_store(
        &self,
        publisher_id: &str,
    ) -> Result<crate::features::viewing_settings::ViewingSettingsStore> {
        crate::features::viewing_settings::ViewingSettingsStore::new(
            self.session.user_id(),
            publisher_id,
        )
    }
    pub(crate) fn port_mapping_store(
        &self,
        publisher_id: &str,
    ) -> Result<crate::features::port_mapping::store::Store> {
        crate::features::port_mapping::store::Store::new(self.session.user_id(), publisher_id)
    }
    pub(crate) fn file_transfer_store(
        &self,
        publisher_id: &str,
    ) -> Result<crate::features::file_transfer::Store> {
        crate::features::file_transfer::Store::new(self.session.user_id(), publisher_id)
    }
    pub fn ended(&self) -> CancellationToken {
        self.ended.clone()
    }
    pub fn is_active(&self) -> bool {
        !self.ended.is_cancelled()
    }
    /// The logged-in account.
    pub(crate) fn user_id(&self) -> &str {
        self.session.user_id()
    }

    /// Retire this generation before asynchronous teardown. Late responses
    /// cannot restore its headers or deliver authenticated results.
    pub fn retire(&self) {
        self.host.retire();
        let mut api = self.api.lock().unwrap_or_else(|error| error.into_inner());
        self.ended.cancel();
        api.take();
    }

    pub fn clear_saved_generation(&self) -> Result<()> {
        self.session_store.clear_if_matches(&self.session)?;
        Ok(())
    }

    pub(crate) fn restoration_stage(&self) -> RestorationStage {
        *self.restore_progress.borrow()
    }

    /// Restore the saved login before a request. With `wait`, keep trying
    /// while the server is unreachable (work that should carry on by itself
    /// once it returns); otherwise give up after one attempt, or at once when
    /// another request is already retrying against an unreachable server.
    async fn validate_saved_session(&self, wait: bool) -> Result<()> {
        let mut away = self.unreachable.subscribe();
        let mut validated = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            validated = self.validated.lock() => validated,
            _ = away.wait_for(|away| *away), if !wait => return Err(unreachable_error()),
        };
        loop {
            if validated.ready {
                return Ok(());
            }
            if let Some(failure) = &validated.failure {
                return Err(failure.error());
            }
            match self.restore(&mut validated, wait).await {
                Err(error) if wait && retry_later(&error) => {
                    tokio::select! {
                        biased;
                        _ = self.ended.cancelled() => bail!("account session has ended"),
                        _ = crate::session::reconnect::wait(RESTORE_RETRY) => {}
                    }
                }
                result => return result,
            }
        }
    }

    async fn restore(&self, validated: &mut RestoreState, wait: bool) -> Result<()> {
        let device = &self.device;
        let identity = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            result = session_restore::initialize(device, self.restore_trigger, wait, &self.unreachable) => result,
        };
        let identity = match identity {
            Ok(identity) => identity,
            // Not kept: the next request tries again.
            Err(error) if retry_later(&error) => return Err(error),
            Err(error) => {
                let failure = match error.downcast::<ApiFailure>() {
                    Ok(error) => RestoreFailure::Api(error),
                    Err(error) => RestoreFailure::Local(format!("{error:#}")),
                };
                let error = failure.error();
                validated.failure = Some(failure);
                return Err(error);
            }
        };
        let mut api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("account session has ended")?;
        api.set_identity(identity.client_identity()?);
        self.restore_progress
            .send_replace(RestorationStage::Account);
        if self.guest {
            // No account: a guest token for this device.
            let token = tokio::select! {
                biased;
                _ = self.ended.cancelled() => bail!("account session has ended"),
                token = api.create_guest() => token,
            };
            let token = match token {
                Ok(token) => token,
                Err(error) => {
                    if let Some(failure) = error.downcast_ref::<ApiFailure>()
                        && !failure.code.retryable()
                    {
                        validated.failure = Some(RestoreFailure::Api(failure.clone()));
                    }
                    return Err(error);
                }
            };
            api.authenticate_guest(token)?;
            let mut active_api = self.api.lock().unwrap_or_else(|error| error.into_inner());
            if !self.is_active() {
                bail!("account session has ended");
            }
            *active_api = Some(api);
            drop(active_api);
            validated.ready = true;
            self.restore_progress.send_replace(RestorationStage::Ready);
            return Ok(());
        }
        let response = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            response = session_restore::restore_user(&api, self.restore_trigger) => response,
        };
        match response {
            Ok(me) => {
                let name = if me.nickname.is_empty() {
                    me.username
                } else {
                    me.nickname
                };
                *self.account_name.lock().unwrap_or_else(|e| e.into_inner()) = name;
            }
            Err(failure) => {
                if failure.invalid_saved_credentials() {
                    self.retire();
                    self.clear_saved_generation()?;
                }
                if session_restore::out_of_reach(failure.code) {
                    self.unreachable.send_if_modified(|away| !std::mem::replace(away, true));
                }
                // An unreachable or busy server is tried again later.
                if !failure.code.retryable() {
                    validated.failure = Some(RestoreFailure::Api(failure.clone()));
                }
                return Err(failure.into());
            }
        }
        let mut active_api = self.api.lock().unwrap_or_else(|error| error.into_inner());
        if !self.is_active() {
            bail!("account session has ended");
        }
        *active_api = Some(api);
        drop(active_api);
        validated.ready = true;
        self.restore_progress.send_replace(RestorationStage::Ready);
        self.schedule_publication();
        Ok(())
    }

    pub(crate) async fn close_device_owner(&self) {
        if let Some(runtime) = self.owned_device.lock().await.take() {
            runtime.close().await;
        }
    }

    /// Finish process-owned initialization work without logging out the account.
    pub async fn close(&self) {
        let publication = self
            .publication
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .task
            .take();
        if let Some(task) = publication {
            task.abort();
            let _ = task.await;
        }
        self.close_device_owner().await;
    }

    pub(crate) async fn restoration_failed(&self) -> bool {
        // A restore still under way (waiting for the server) has not failed.
        self.validated
            .try_lock()
            .is_ok_and(|validated| validated.failure.is_some())
    }

    pub async fn logout(&self) -> LogoutOutcome {
        if crate::platform::windows::host_service::resident::managed() {
            let _ = crate::platform::windows::host_service::resident::request(
                crate::platform::windows::host_service::resident::Request::Retire {
                    account: self.account_generation(),
                },
            )
            .await;
        }
        let api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.ended.cancel();
        crate::transport::direct::store::clear_own_devices();
        let remote_error = match api {
            Some(api) => api.logout().await.err().map(|error| format!("{error:#}")),
            None => None,
        };
        // Local credentials are cleared on either server outcome.
        let local_error = self
            .clear_saved_generation()
            .err()
            .map(|error| format!("{error:#}"));
        self.close_device_owner().await;
        LogoutOutcome {
            remote_error,
            local_error,
        }
    }

    /// Run one authenticated request after the saved session is validated;
    /// while the server is unreachable it fails within one attempt.
    pub(crate) async fn request<T, F>(&self, request: impl FnOnce(ServerApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        self.request_with(false, request).await
    }

    /// The name the server's administrator gave it.
    pub(crate) async fn server_name(&self) -> Result<String> {
        self.request(|api| async move { api.server_info().await })
            .await
            .map(|info| info.server.instance_name)
    }

    /// The same, waiting for an unreachable server: for the host and the
    /// device list, which carry on by themselves once it returns.
    pub(crate) async fn request_waiting<T, F>(
        &self,
        request: impl FnOnce(ServerApi) -> F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        self.request_with(true, request).await
    }

    /// Whether the server was out of reach at the last attempt.
    pub(crate) fn server_unreachable(&self) -> bool {
        *self.unreachable.borrow()
    }

    /// The session (or a guest's device registration and token) is in
    /// place, as before a first request.
    pub(crate) async fn ready(&self) -> Result<()> {
        self.validate_saved_session(false).await
    }

    async fn request_with<T, F>(
        &self,
        wait: bool,
        request: impl FnOnce(ServerApi) -> F,
    ) -> Result<T>
    where
        F: std::future::Future<Output = Result<T>>,
    {
        self.validate_saved_session(wait).await?;
        let api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("account session has ended")?;
        let response = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            response = request(api) => response,
        };
        if !self.is_active() {
            bail!("account session has ended");
        }
        let away = response
            .as_ref()
            .err()
            .and_then(crate::account::api::failure_code)
            .is_some_and(session_restore::out_of_reach);
        self.unreachable
            .send_if_modified(|current| std::mem::replace(current, away) != away);
        if let Err(error) = &response
            && error
                .downcast_ref::<ApiFailure>()
                .is_some_and(ApiFailure::retires_account)
        {
            // The server revoked this login (logout elsewhere, device removed
            // from the account). End this generation; never retry it.
            tracing::info!("server revoked the account session; retiring this generation");
            self.retire();
        }
        response
    }

    /// Cache the friends' devices that may control this one, for admitting
    /// them on direct connections (`docs/access-model.md` §2.2).
    pub(crate) async fn refresh_friend_access(&self) -> Result<()> {
        if self.guest {
            bail!("{}", t().common.signed_out);
        }
        let list = self
            .request_waiting(|api| async move { api.friend_access().await })
            .await?;
        let devices = list
            .devices
            .into_iter()
            .map(|d| crate::transport::direct::store::FriendDevice {
                device_id: d.device_id,
                public_key: d.public_key,
                alias: d.alias,
                user_id: d.user_id,
                nickname: d.nickname,
                grants: d.grants,
            })
            .collect();
        crate::transport::direct::store::save_friend_devices(
            self.session.user_id(),
            &crate::transport::direct::local_device()?.identity.device_id,
            devices,
        );
        Ok(())
    }

    pub async fn list_devices(&self) -> Result<DeviceList> {
        self.list_devices_with(false).await
    }

    /// The device centre's list: waits for an unreachable server, so it
    /// arrives by itself once the server is back.
    pub(crate) async fn sync_devices(&self) -> Result<DeviceList> {
        self.list_devices_with(true).await
    }

    async fn list_devices_with(&self, wait: bool) -> Result<DeviceList> {
        if self.guest {
            bail!("{}", t().common.signed_out);
        }
        let list = self
            .request_with(wait, |api| async move { api.list_devices().await })
            .await?;
        for device in list.all() {
            self.features.remember(device);
        }
        // Direct connections recognise this account's devices by key, also
        // while the server is unreachable (`docs/protocol/v2-design.md` §12.2).
        crate::transport::direct::store::save_device_list(self.session.user_id(), &list);
        self.schedule_publication();
        Ok(list)
    }

    /// The account's own devices (including this one) for device management.
    pub async fn device_groups(&self) -> Result<DeviceGroups> {
        Ok(DeviceGroups::from_list(&self.list_devices().await?))
    }

    /// [`Self::device_groups`] for the device centre (see [`Self::sync_devices`]).
    pub(crate) async fn sync_device_groups(&self) -> Result<DeviceGroups> {
        Ok(DeviceGroups::from_list(&self.sync_devices().await?))
    }

    // --------------------------------------------------------------- chat

    pub(crate) async fn chat(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<serde_json::Value>,
    ) -> Result<serde_json::Value> {
        self.request(|api| {
            let (method, body) = (method.clone(), body.clone());
            async move { api.chat(method, path, body.as_ref()).await }
        })
        .await
    }

    pub(crate) async fn chat_bytes(
        &self,
        method: reqwest::Method,
        path: &str,
        bytes: bytes::Bytes,
    ) -> Result<serde_json::Value> {
        self.request(|api| {
            let (method, bytes) = (method.clone(), bytes.clone());
            async move { api.chat_bytes(method, path, bytes).await }
        })
        .await
    }

    pub(crate) async fn media_get(&self, path: &str, max: usize) -> Result<bytes::Bytes> {
        self.request(|api| async move { api.media_get(path, max).await }).await
    }

    pub(crate) async fn media_put(&self, path: &str, data: bytes::Bytes) -> Result<serde_json::Value> {
        self.request(|api| {
            let data = data.clone();
            async move { api.media_put(path, data).await }
        })
        .await
    }

    pub(crate) async fn media_download(&self, path: &str, dest: &std::path::Path) -> Result<()> {
        self.request(|api| async move { api.media_download(path, dest).await }).await
    }

    /// The server this account is on.
    pub(crate) fn server_config(&self) -> Option<crate::account::server::ServerConfig> {
        self.api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .map(|api| api.server().clone())
    }

    // ------------------------------------------------------------ friends

    pub async fn friends(&self) -> Result<wire::FriendList> {
        self.request(|api| async move { api.friends().await }).await
    }

    pub async fn friend_requests(&self) -> Result<wire::FriendRequests> {
        self.request(|api| async move { api.friend_requests().await })
            .await
    }

    pub async fn send_friend_request(&self, username: &str) -> Result<wire::FriendRequestOutcome> {
        self.request(|api| async move { api.send_friend_request(username).await })
            .await
    }

    pub async fn answer_friend_request(&self, id: &str, accept: Option<Vec<String>>) -> Result<()> {
        self.request(|api| {
            let accept = accept.clone();
            async move { api.answer_friend_request(id, accept).await }
        })
        .await
    }

    pub async fn cancel_friend_request(&self, id: &str) -> Result<()> {
        self.request(|api| async move { api.cancel_friend_request(id).await })
            .await
    }

    pub async fn remove_friend(&self, user_id: &str) -> Result<()> {
        self.request(|api| async move { api.remove_friend(user_id).await })
            .await
    }

    /// What this account grants a friend.
    pub async fn grants(&self, user_id: &str) -> Result<wire::Grants> {
        self.request(|api| async move { api.grants(user_id).await })
            .await
    }

    pub async fn set_grants(&self, user_id: &str, grants: wire::Grants) -> Result<()> {
        let grants = wire::Grants {
            default: farwing_protocol::caps::normalize(&grants.default),
            devices: grants
                .devices
                .into_iter()
                .map(|(device, caps)| (device, farwing_protocol::caps::normalize(&caps)))
                .collect(),
        };
        self.request(|api| async move { api.set_grants(user_id, &grants).await })
            .await
    }

    /// Logins of this account, one per device.
    pub async fn auth_sessions(&self) -> Result<wire::AuthSessions> {
        self.request(|api| async move { api.auth_sessions().await })
            .await
    }

    pub async fn revoke_auth_session(&self, device_id: &str) -> Result<()> {
        if device_id == self.device_id() {
            bail!("{}", t().session.cannot_remove_own_login);
        }
        self.request(|api| async move { api.revoke_auth_session(device_id).await })
            .await
    }

    pub async fn device_detail(&self, id: &str) -> Result<crate::account::api::DeviceDetail> {
        self.request(|api| async move { api.device_detail(id).await })
            .await
    }

    pub async fn account_info(&self) -> Result<farwing_protocol::api::Me> {
        self.request(|api| async move { api.me().await }).await
    }

    pub(crate) fn feature_catalog(&self) -> crate::account::feature_ability::FeatureCatalog {
        self.features.clone()
    }

    pub(crate) fn account_name(&self) -> String {
        self.account_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub(crate) fn suggested_device_name(&self) -> String {
        self.device.identity().suggested_name()
    }

    pub(crate) async fn rename_owned_device(
        &self,
        id: &str,
        alias: &str,
    ) -> Result<(String, String)> {
        let groups = self.device_groups().await?;
        if !groups.entries().any(|(_, d)| d.device_id == id) {
            bail!("{}", t().session.device_not_in_account);
        }
        let result = self
            .request(|api| async move { api.rename_device(id, alias).await })
            .await;
        let actual = match result {
            Ok(response)
                if response.device_id == id
                    && !response.alias.trim().is_empty()
                    && !response.alias.chars().any(char::is_control) =>
            {
                response.alias
            }
            Err(error) if is_server_failure(&error) => {
                return Err(error.context(t().session.rename_rejected));
            }
            result => {
                // A timeout/response mismatch is not evidence that a write did
                // not happen. Read back; never replay a management mutation.
                let query = self.device_groups().await;
                if query.as_ref().is_ok_and(|g| {
                    g.entries()
                        .any(|(_, d)| d.device_id == id && d.alias == alias)
                }) {
                    alias.to_owned()
                } else {
                    let error = result
                        .err()
                        .map(|e| format!("{e:#}"))
                        .unwrap_or_else(|| t().session.returned_device_id_mismatch.into());
                    bail!("{}", (t().session.rename_unconfirmed)(&error));
                }
            }
        };
        if id == self.device_id()
            && let Err(error) = self.device.set_name(id.into(), actual.clone()).await
        {
            return Ok((
                actual,
                (t().session.renamed_local_save_failed)(&format!("{error:#}")),
            ));
        }
        Ok((actual, t().session.device_renamed.into()))
    }

    pub(crate) async fn remove_account_device(&self, id: &str) -> Result<String> {
        if id == self.device_id() {
            bail!("{}", t().session.cannot_remove_this_device);
        }
        let present = self
            .device_groups()
            .await?
            .entries()
            .any(|(_, d)| d.device_id == id);
        if !present {
            bail!("{}", t().session.device_or_owner_changed);
        }
        let result = self
            .request(|api| async move { api.remove_device(id).await })
            .await;
        if let Err(error) = result {
            if is_server_failure(&error) {
                return Err(error.context(t().session.remove_rejected));
            }
            let absent = self
                .device_groups()
                .await
                .map(|g| !g.entries().any(|(_, d)| d.device_id == id));
            if !matches!(absent, Ok(true)) {
                bail!("{}", (t().session.remove_unconfirmed)(&format!("{error:#}")));
            }
        }
        Ok(t().session.device_removed.into())
    }

    pub(crate) async fn power_owned_device(
        &self,
        expected: &crate::account::api::DeviceInfo,
        action: crate::account::power::PowerAction,
        on_send: impl FnOnce(),
    ) -> Result<crate::account::power::PowerReceipt> {
        let id = expected.validated_device_id()?;
        let groups = self.device_groups().await?;
        if id == self.device_id() || id == groups.current_device_id {
            bail!("{}", t().session.power_not_for_this_device);
        }
        let device = groups
            .desktop_devices
            .iter()
            .find(|d| d.device_id == id)
            .context(t().session.device_or_owner_changed)?;
        if device.alias != expected.alias || device.platform != expected.platform {
            bail!("{}", t().session.device_name_or_platform_changed);
        }
        if device.participant_count() > expected.participant_count() {
            bail!("{}", t().session.device_new_connection);
        }
        action.check(device, &self.features)?;
        // One user decision produces one request; a lost acknowledgement is
        // reported as unconfirmed, never replayed.
        on_send();
        let result = self
            .request(|api| async move { api.device_power(id, action).await })
            .await;
        match result {
            Ok(mut receipt) => {
                if action == crate::account::power::PowerAction::Wake
                    && receipt.current_device_in_same_network == Some(true)
                    && let Some(target) = &receipt.target
                {
                    receipt.local_packets =
                        match crate::features::host::wol::wake_locally(target).await {
                            Ok(sent) => Some(sent),
                            Err(error) => {
                                tracing::warn!(%error, "local Wake-on-LAN broadcast failed");
                                Some(0)
                            }
                        };
                }
                Ok(receipt)
            }
            Err(error) if is_server_failure(&error) => {
                Err(error.context((t().session.power_rejected)(action.label())))
            }
            Err(error) => bail!(
                "{}",
                (t().session.power_unconfirmed)(action.label(), &format!("{error:#}"))
            ),
        }
    }

    /// Create a session with an own or shared device and obtain its realtime
    /// ticket. Other participants of the host are never displaced.
    /// `visit`: the connection record these sessions belong to (a viewer
    /// window); empty for a session of its own.
    pub async fn join_device(
        &self,
        device_id: &str,
        purpose: Purpose,
        visit: &str,
    ) -> Result<RealtimeTicket> {
        // Signal over this process's presence connection when it has one
        // (`docs/protocol/v2-design.md` §11).
        let presence =
            crate::transport::signal::presence_available(&self.device_id(), self.user_id());
        let (ticket, target) = self
            .request(|api| async move { api.create_session(device_id, purpose, presence, visit).await })
            .await?;
        if !target.device_id.is_empty() {
            self.features
                .remember_target(&target.device_id, &target.capabilities);
        }
        Ok(ticket)
    }

    /// Publish what this device's owner allows, then keep it for device
    /// registration.
    pub async fn set_policy(&self, policy: DevicePolicy) -> Result<()> {
        let device = &self.device;
        self.request_waiting(|api| async move { api.set_policy(&policy).await })
            .await?;
        device.set_policy(policy).await?;
        Ok(())
    }
}

/// How long a waiting request pauses before restoring the login again.
const RESTORE_RETRY: std::time::Duration = std::time::Duration::from_secs(10);

/// A failure that may pass: the server unreachable, busy or failing.
fn retry_later(error: &anyhow::Error) -> bool {
    crate::account::api::failure_code(error).is_some_and(|code| code.retryable())
}

fn unreachable_error() -> anyhow::Error {
    ApiFailure::new(
        crate::account::api::ErrorCode::Network,
        "cannot connect",
    )
    .into()
}

/// The server answered (as opposed to a lost or unanswered request).
fn is_server_failure(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<ApiFailure>()
        .is_some_and(|failure| failure.code != crate::account::api::ErrorCode::Network)
}

impl Drop for AuthenticatedClient {
    fn drop(&mut self) {
        self.host.retire();
        self.ended.cancel();
    }
}
