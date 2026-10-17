//! One authenticated account generation. Room owners decide when it ends;
//! a generic REST error is not a global logout notification.

use std::sync::Mutex;

use anyhow::{Context, Result, bail};
use tokio_util::sync::CancellationToken;

use crate::account::api::{ApiEnvelope, ApiFailure, DeviceList, NrdApi, RoomSession};
use crate::account::auth::{KeyringIdentityStore, KeyringSessionStore, LoginSession, SessionStore};
use crate::account::session_restore::{self, RestoreTrigger};
use crate::session::device_session::{DeviceHandle, DeviceRuntime};

#[derive(Debug)]
pub(crate) struct NoSavedSession;

impl std::fmt::Display for NoSavedSession {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("no saved login session; run `OpenUUYC login` first")
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
    api: Mutex<Option<NrdApi>>,
    session: LoginSession,
    device: DeviceHandle,
    owned_device: tokio::sync::Mutex<Option<DeviceRuntime>>,
    session_store: KeyringSessionStore,
    ended: CancellationToken,
    validated: tokio::sync::Mutex<RestoreState>,
    restore_trigger: RestoreTrigger,
    restore_progress: tokio::sync::watch::Sender<RestorationStage>,
    account_name: Mutex<String>,
    publication: Mutex<publication::Sync>,
    features: crate::account::feature_ability::FeatureCatalog,
    controlling_features: crate::account::feature_ability::FeatureCatalog,
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
            Self::Local(message) => anyhow::anyhow!("本机初始化失败：{message}"),
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
        let identity = device.identity().client_identity()?;
        let mut host = crate::features::host::Handle::load(session.user_id(), &identity.device_id);
        host.bind_account(session.generation());
        let mut api = NrdApi::new(identity)?;
        api.set_user_id(Some(session.user_id()))?;
        api.set_bearer_token(Some(session.token()))?;
        let ended = CancellationToken::new();

        let restore_trigger =
            if owned_device.is_some() || crate::platform::host_service::resident::is_owner() {
                RestoreTrigger::DeviceStartup
            } else {
                RestoreTrigger::ExplicitLogin
            };
        let account_name = Mutex::new(session.nickname().to_owned());
        Ok(Self {
            host,
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
            account_name,
            publication: Mutex::new(publication::Sync::default()),
            features: crate::account::feature_ability::FeatureCatalog::default(),
            controlling_features: crate::account::feature_ability::FeatureCatalog::new(true),
        })
    }

    pub(crate) fn account_generation(&self) -> String {
        self.session.generation()
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

    /// Retire this generation before asynchronous room teardown. Late responses
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

    async fn validate_saved_session(&self) -> Result<()> {
        let mut validated = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            validated = self.validated.lock() => validated,
        };
        if validated.ready {
            return Ok(());
        }
        if let Some(failure) = &validated.failure {
            return Err(failure.error());
        }
        let device = &self.device;
        let identity = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            result = session_restore::initialize(device, self.restore_trigger) => result,
        };
        let identity = match identity {
            Ok(identity) => identity,
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
        let response = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            response = session_restore::restore_user(&api, self.restore_trigger) => response,
        };
        match response {
            Ok(info) => {
                if let Some(name) = info.get("nickname").and_then(serde_json::Value::as_str) {
                    *self.account_name.lock().unwrap_or_else(|e| e.into_inner()) = name.to_owned();
                }
            }
            Err(failure) => {
                if failure.invalid_saved_credentials() {
                    self.retire();
                    self.clear_saved_generation()?;
                }
                validated.failure = Some(RestoreFailure::Api(failure.clone()));
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
        self.features.close().await;
        self.controlling_features.close().await;
        self.close_device_owner().await;
    }

    pub(crate) async fn restoration_failed(&self) -> bool {
        self.validated.lock().await.failure.is_some()
    }

    pub async fn logout(&self) -> LogoutOutcome {
        if crate::platform::host_service::resident::managed() {
            let _ = crate::platform::host_service::resident::request(
                crate::platform::host_service::resident::Request::Retire {
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
        let remote_error = match api {
            Some(api) => api.logout().await.err().map(|error| format!("{error:#}")),
            None => None,
        };
        // Server 3E46B0 clears local credentials on either API outcome.
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

    pub(crate) async fn request<T, F>(&self, request: impl FnOnce(NrdApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        self.request_envelope(request).await?.into_data()
    }

    async fn request_envelope<T, F>(
        &self,
        request: impl FnOnce(NrdApi) -> F,
    ) -> Result<ApiEnvelope<T>>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        self.validate_saved_session().await?;
        let api = self
            .api
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
            .context("account session has ended")?;
        let response = tokio::select! {
            biased;
            _ = self.ended.cancelled() => bail!("account session has ended"),
            response = request(api) => response?,
        };
        if !self.is_active() {
            bail!("account session has ended");
        }
        Ok(response)
    }

    pub(crate) async fn international_bitrate_limit(&self) -> Result<Option<u32>> {
        let mut configs = self
            .request(|api| async move {
                api.query_configures(&[("custom_bitrate_limit".into(), String::new())])
                    .await
            })
            .await?;
        let Some(entry) = configs
            .remove("custom_bitrate_limit")
            .filter(|v| v.status == 0)
        else {
            return Ok(None);
        };
        let Some(value) = entry.data else {
            return Ok(None);
        };
        let value = if let serde_json::Value::String(text) = value {
            serde_json::from_str(&text)?
        } else {
            value
        };
        if value.get("enable").and_then(|v| v.as_bool()) != Some(true) {
            return Ok(None);
        }
        Ok(value
            .get("bitrate_limit")
            .and_then(|v| v.as_u64())
            .filter(|v| *v > 0)
            .map(|v| {
                let n = v.min(500) as u32;
                if n <= 20 {
                    n
                } else if n <= 100 {
                    20 + (n - 20) / 5 * 5
                } else if n <= 200 {
                    100 + (n - 100) / 10 * 10
                } else {
                    200 + (n - 200) / 50 * 50
                }
            }))
    }
    pub async fn list_devices(&self) -> Result<DeviceList> {
        let list = self
            .request(|api| async move { api.list_devices().await })
            .await?;
        self.schedule_publication();
        self.schedule_feature_refresh(false);
        Ok(list)
    }

    pub async fn device_groups(&self) -> Result<crate::account::api::DeviceGroups> {
        self.management_request(|api| async move { api.device_groups().await })
            .await
    }

    pub async fn device_detail(&self, id: &str) -> Result<crate::account::api::DeviceDetail> {
        self.management_request(|api| async move { api.device_detail(id).await })
            .await
    }

    pub async fn account_info(&self) -> Result<serde_json::Value> {
        let info = self
            .request(|api| async move { api.get_user_info().await })
            .await?;
        self.schedule_feature_refresh(false);
        Ok(info)
    }

    pub(crate) fn feature_catalog(&self) -> crate::account::feature_ability::FeatureCatalog {
        self.features.clone()
    }
    pub(crate) fn controlling_features(&self) -> crate::account::feature_ability::FeatureCatalog {
        self.schedule_feature_refresh(true);
        self.controlling_features.clone()
    }

    pub(crate) fn schedule_feature_refresh(&self, session_create: bool) {
        if let Some(api) = self.api.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            self.features
                .refresh(api.clone(), self.ended.clone(), session_create);
            self.controlling_features
                .refresh(api, self.ended.clone(), session_create);
        }
    }

    pub(crate) fn account_name(&self) -> String {
        self.account_name
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    async fn management_request<T, F>(&self, request: impl FnOnce(NrdApi) -> F) -> Result<T>
    where
        F: std::future::Future<Output = Result<ApiEnvelope<T>>>,
    {
        let result = self.request(request).await;
        // These specific official management callbacks route 1120 to account
        // invalidation. Do not broaden every room/API failure into a logout.
        if result
            .as_ref()
            .err()
            .and_then(|e| e.downcast_ref::<ApiFailure>())
            .is_some_and(|f| f.code == 1120)
        {
            self.retire();
        }
        result
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
            bail!("设备已不在本账号绑定列表中，未发送改名请求");
        }
        let result = self
            .management_request(|api| async move { api.rename_device(id, alias).await })
            .await;
        let actual = match result {
            Ok(response)
                if response.device_id == id
                    && !response.alias.trim().is_empty()
                    && !response.alias.chars().any(char::is_control) =>
            {
                response.alias
            }
            Err(error) if error.downcast_ref::<ApiFailure>().is_some() => {
                return Err(error.context("设备改名被服务端拒绝"));
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
                        .unwrap_or_else(|| "返回的设备 ID 不匹配".into());
                    bail!("改名结果未确认，未自动重试：{error}。请刷新检查当前名称");
                }
            }
        };
        if id == self.device_id()
            && let Err(error) = self.device.set_name(id.into(), actual.clone()).await
        {
            return Ok((
                actual,
                format!("服务端已改名，但本地注册名称保存失败：{error:#}"),
            ));
        }
        Ok((actual, "设备名称已更新".into()))
    }

    pub(crate) async fn remove_account_device(&self, id: &str) -> Result<String> {
        if id == self.device_id() {
            bail!("移除本机设备请使用退出登录，以完成本地会话清理");
        }
        let present = self
            .device_groups()
            .await?
            .entries()
            .any(|(_, d)| d.device_id == id);
        if !present {
            bail!("设备或所有权已变化，未发送移除请求，请刷新列表");
        }
        let result = self
            .management_request(|api| async move { api.unbind_device(id).await })
            .await;
        if let Err(error) = result {
            if error.downcast_ref::<ApiFailure>().is_some() {
                return Err(error.context("设备移除被服务端拒绝"));
            }
            let absent = self
                .device_groups()
                .await
                .map(|g| !g.entries().any(|(_, d)| d.device_id == id));
            if !matches!(absent, Ok(true)) {
                bail!("移除结果未确认，未自动重试：{error:#}。请刷新列表检查");
            }
        }
        Ok("设备已移除".into())
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
            bail!("本机设备不支持电源操作，未发送请求");
        }
        let device = groups
            .desktop_devices
            .iter()
            .find(|d| d.device_id == id)
            .context("设备或所有权已变化，未发送电源请求，请刷新列表")?;
        if device.alias != expected.alias || device.platform != expected.platform {
            bail!("设备名称或平台已变化，请刷新后重新确认目标");
        }
        if device.participant_count() > expected.participant_count() {
            bail!("该设备新增了远控连接，请刷新后重新确认影响");
        }
        action.check(device, &self.features)?;
        // The envelope, unlike request(), distinguishes a preflight error from
        // a power request whose acknowledgement may have been lost. Never replay.
        on_send();
        let result = self
            .request_envelope(|api| async move { api.device_power(id, action).await })
            .await;
        match result {
            Ok(response) => response
                .into_data()
                .with_context(|| format!("{}请求被服务端拒绝", action.label())),
            Err(error) => bail!(
                "{}结果未确认，未自动重试；请先检查设备状态：{error:#}",
                action.label()
            ),
        }
    }

    pub async fn join_device(&self, device_id: &str, force_join: bool) -> Result<RoomSession> {
        let room: RoomSession = self
            .request(|api| async move { api.join_by_device(device_id, force_join).await })
            .await?;
        room.validate()?;
        Ok(room)
    }

    pub(crate) async fn update_owned_device(&self, device_id: &str, immediate: bool) -> Result<()> {
        crate::account::api::validate_device_id(device_id)?;
        let devices = self.list_devices().await?;
        if device_id == devices.current_device.device_id {
            bail!("不能通过远端更新入口更新本机设备");
        }
        let device = devices
            .my_binded_devices
            .iter()
            .find(|device| device.device_id == device_id)
            .context("设备已不在当前账号中，未发送更新请求")?;
        if device.platform != 1
            || !device.is_connected()
            || !device.controlled_support
            || !device.controllable
            || !self
                .features
                .policy(device.platform, &device.version_name)
                .supports(crate::account::feature_ability::Feature::ControlledUpdate)
        {
            bail!("当前设备不支持远端更新，未发送请求");
        }
        // One user choice produces one request. Do not retry an unknown result.
        let response = self
            .request_envelope(|api| async move {
                api.trigger_controlled_update(device_id, immediate).await
            })
            .await
            .context("更新请求结果未确认，请先检查被控端状态")?;
        if response.code != 0 {
            return Err(crate::account::api::ApiFailure {
                code: response.code,
                message: response.msg,
            }
            .into());
        }
        Ok(())
    }

    pub async fn set_controllable(&self, controllable: bool) -> Result<()> {
        let device = &self.device;
        self.request(|api| async move { api.set_controllable(controllable).await })
            .await?;
        device.set_controllable(controllable).await?;
        Ok(())
    }
}

impl Drop for AuthenticatedClient {
    fn drop(&mut self) {
        self.host.retire();
        self.ended.cancel();
    }
}
