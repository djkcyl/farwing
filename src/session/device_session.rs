//! DeviceInitializer owner, independent of any one authenticated account.
//! One serialized registration request; identity changes are explicit operations.
use anyhow::{Context, Result, bail};
use futures_util::{FutureExt, StreamExt, future::BoxFuture, stream::FuturesUnordered};
use tokio::{
    sync::{mpsc, watch},
    task::JoinHandle,
};
use tokio_util::sync::CancellationToken;

use crate::account::api::{ApiFailure, DeviceInitResponse, DevicePolicy, ErrorCode, ServerApi};
use crate::account::server::ServerConfig;
use crate::account::auth::{KeyringIdentityStore, NativeIdentity};

#[derive(Clone)]
pub(crate) struct DeviceHandle {
    commands: mpsc::UnboundedSender<Command>,
    identity: watch::Receiver<NativeIdentity>,
}

pub(crate) struct DeviceRuntime {
    handle: DeviceHandle,
    cancel: CancellationToken,
    task: Option<JoinHandle<()>>,
}

enum Command {
    Ensure { force: bool, result: Completion },
    StartupRetry(tokio::sync::oneshot::Sender<std::time::Duration>),
    Policy(DevicePolicy, tokio::sync::oneshot::Sender<Result<()>>),
    Name(String, String, tokio::sync::oneshot::Sender<Result<()>>),
}

type Completion = tokio::sync::oneshot::Sender<Result<NativeIdentity>>;

// Network failures already arrive as `ApiFailure { code: Network }` from the
// API client; local identity/storage errors and malformed replies stay fatal.
fn registration_error(error: anyhow::Error) -> anyhow::Error {
    error
}

type Pending = FuturesUnordered<
    BoxFuture<'static, (Completion, Result<(DeviceInitResponse, NativeIdentity, String)>)>,
>;

struct Initializer {
    identity: NativeIdentity,
    published: watch::Sender<NativeIdentity>,
    identities: KeyringIdentityStore,
    waiters: Vec<Completion>,
    initialized: bool,
    date: Option<chrono::NaiveDate>,
    errors: u8,
    startup_retries: u32,
    pending: Pending,
}

impl DeviceRuntime {
    pub(crate) fn start() -> Result<Self> {
        let identities = KeyringIdentityStore::new()?;
        let identity = identities.load_or_create()?;
        let (published, receiver) = watch::channel(identity.clone());
        let (commands, incoming) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let state = Initializer {
            identity,
            published,
            identities,
            waiters: Vec::new(),
            initialized: false,
            date: None,
            errors: 0,
            startup_retries: 0,
            pending: Pending::new(),
        };
        let task = tokio::spawn(state.run(incoming, cancel.clone()));
        Ok(Self {
            handle: DeviceHandle {
                commands,
                identity: receiver,
            },
            cancel,
            task: Some(task),
        })
    }

    pub(crate) fn handle(&self) -> DeviceHandle {
        self.handle.clone()
    }

    pub(crate) async fn close(mut self) {
        self.cancel.cancel();
        if let Some(task) = self.task.take()
            && let Err(error) = task.await
        {
            tracing::warn!(%error, "device initializer task failed");
        }
    }
}

impl Drop for DeviceRuntime {
    fn drop(&mut self) {
        self.cancel.cancel();
        // Error/cancellation exits may drop the owner before normal close.
        // No HTTP completion may outlive it and write a new identity afterward.
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

impl DeviceHandle {
    pub(crate) fn identity(&self) -> NativeIdentity {
        self.identity.borrow().clone()
    }

    pub(crate) async fn ensure(&self, force: bool) -> Result<NativeIdentity> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Ensure { force, result })
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before completing the request")?
    }
    /// Save the policy that device registration reports.
    pub(crate) async fn set_policy(&self, value: DevicePolicy) -> Result<()> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Policy(value, result))
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before saving local permission")?
    }

    pub(crate) async fn set_name(&self, expected_id: String, value: String) -> Result<()> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::Name(expected_id, value, result))
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before saving name")?
    }

    pub(crate) async fn startup_retry_delay(&self) -> Result<std::time::Duration> {
        let (result, response) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::StartupRetry(result))
            .context("device initializer has stopped")?;
        response
            .await
            .context("device initializer stopped before scheduling retry")
    }
}

impl Initializer {
    async fn run(
        mut self,
        mut incoming: mpsc::UnboundedReceiver<Command>,
        cancel: CancellationToken,
    ) {
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                command = incoming.recv() => match command {
                    None => break,
                    Some(Command::StartupRetry(reply)) => {
                        // This counter belongs to the device session; marking
                        // it initialized does not reset it.
                        let seconds = if self.startup_retries < 60 { 3 } else { 30 };
                        self.startup_retries = self.startup_retries.saturating_add(1);
                        let _ = reply.send(std::time::Duration::from_secs(seconds));
                    }
                    Some(Command::Policy(value, reply)) => {
                        #[cfg(windows)]
                        if crate::platform::windows::host_service::resident::managed() {
                            let result = crate::platform::windows::host_service::resident::request(crate::platform::windows::host_service::resident::Request::Policy(value)).await.map(|_| ());
                            let _ = self.reload(); let _ = reply.send(result); continue;
                        }
                        let result = self.reload().and_then(|()| {
                            if self.identity.policy() == value { return Ok(()); }
                            let mut next = self.identity.clone(); next.set_policy(value); self.commit(next)
                        });
                        let _ = reply.send(result);
                    }
                    Some(Command::Name(expected_id, value, reply)) => {
                        #[cfg(windows)]
                        if crate::platform::windows::host_service::resident::managed() {
                            let result = crate::platform::windows::host_service::resident::request(crate::platform::windows::host_service::resident::Request::Name { device: expected_id, value }).await.map(|_| ());
                            let _ = self.reload(); let _ = reply.send(result); continue;
                        }
                        let result = self.reload().and_then(|()| {
                            if self.identity.client_identity()?.device_id != expected_id { bail!("{}", crate::i18n::t().session.error_local_identity_changed); }
                            let mut next = self.identity.clone(); next.set_device_name(value); self.commit(next)
                        });
                        let _ = reply.send(result);
                    }
                    Some(Command::Ensure { force, result }) => {
                        #[cfg(windows)]
                        if crate::platform::windows::host_service::resident::managed() {
                            use crate::platform::windows::host_service::resident::{self, Request, Reply};
                            let value = match resident::request(Request::Initialize { force }).await {
                                Ok(Reply::Identity(value)) => { self.identity = *value.clone(); self.published.send_replace(*value.clone()); Ok(*value) }
                                Ok(_) => Err(anyhow::anyhow!("{}", crate::i18n::t().session.error_background_reply_invalid)),
                                Err(e) => Err(e),
                            };
                            self.finish(result, value); continue;
                        }
                        if let Err(error) = self.reload() { self.finish(result, Err(error)); continue; }
                        if !self.pending.is_empty() { self.waiters.push(result); continue; }
                        let today = chrono::Local::now().date_naive();
                        if !force && self.initialized && self.date == Some(today) {
                            self.finish(result, Ok(self.identity.clone()));
                        } else {
                            // ensureInitialized resets immediate failures, but
                            // recursive initDevice calls below do not.
                            self.errors = 0;
                            self.start_request(result);
                        }
                    }
                },
                Some((completion, result)) = self.pending.next(), if !self.pending.is_empty() => {
                    self.received(completion, result);
                }
            }
        }
        // The HTTP futures live in this owner, not detached retry tasks.
        self.pending.clear();
        tracing::debug!("device initializer HTTP requests and callbacks released");
    }

    fn reload(&mut self) -> Result<()> {
        let identity = self.identities.load_or_create()?;
        if identity != self.identity {
            self.identity = identity;
            self.published.send_replace(self.identity.clone());
            self.initialized = false;
        }
        Ok(())
    }

    fn commit(&mut self, next: NativeIdentity) -> Result<()> {
        // Several processes (GUI, CLI, service) share one identity record. An
        // older process must not overwrite another process's registration.
        if !self.identities.replace_if_matches(&self.identity, &next)? {
            bail!(
                "device identity changed in another process; this initializer is no longer its owner"
            );
        }
        self.identity = next;
        self.published.send_replace(self.identity.clone());
        Ok(())
    }

    fn start_request(&mut self, completion: Completion) {
        self.date = Some(chrono::Local::now().date_naive());
        let request = (|| -> Result<_> {
            let server = ServerConfig::require()?;
            // A registration belongs to one server. Changing servers keeps the
            // installation ID but registers a new device there.
            if !self.identity.server().is_empty() && !self.identity.registered_for(&server.key()) {
                let mut next = self.identity.clone();
                next.clear_registration();
                self.commit(next)?;
                self.initialized = false;
            }
            // Device registration proves possession of the device key and
            // never carries an account token: login binds the device to an
            // account separately.
            let mut api = ServerApi::new(&server)?;
            api.set_identity(self.identity.client_identity()?);
            Ok((api, self.identity.clone(), server.key()))
        })();
        match request {
            Ok((api, identity, server_key)) => {
                tracing::debug!(
                    attempt = self.errors + 1,
                    "device initialization request submitted"
                );
                self.pending.push(
                    async move {
                        let result = async {
                            crate::account::reporting::update(|s| {
                                s.registration = "正在读取本机信息".into()
                            });
                            #[cfg(windows)]
                            let hardware = tokio::task::spawn_blocking(
                                crate::platform::windows::device_profile::Hardware::read,
                            )
                            .await??;
                            #[cfg(windows)]
                            let body = identity.device_init_request(&hardware)?;
                            #[cfg(target_os = "linux")]
                            let body = {
                                let profile = tokio::task::spawn_blocking(
                                    crate::platform::linux::device_profile::DeviceProfile::read,
                                )
                                .await??;
                                identity.device_init_request(&profile)?
                            };
                            crate::account::reporting::update(|s| {
                                #[cfg(windows)]
                                {
                                    s.hardware = Some(hardware);
                                }
                                s.reported_name = body.name.clone();
                                s.client_id = body.installation_id.clone();
                                s.reported_controllable = body.policy.control;
                                s.registration = "正在上报设备信息".into();
                            });
                            let response = api
                                .init_device(body)
                                .await
                                .map_err(registration_error)?;
                            Ok((response, identity, server_key))
                        }
                        .await;
                        (completion, result)
                    }
                    .boxed(),
                );
            }
            Err(error) => self.finish(completion, Err(error)),
        }
    }

    fn received(
        &mut self,
        completion: Completion,
        response: Result<(DeviceInitResponse, NativeIdentity, String)>,
    ) {
        match response {
            Ok((response, sent, server_key)) => {
                let result = (|| -> Result<_> {
                    let mut next = self.identity.clone();
                    anyhow::ensure!(
                        next.client_identity()?.client_id == sent.client_identity()?.client_id,
                        "{}",
                        crate::i18n::t().session.error_registration_identity_changed
                    );
                    let previous = next.client_identity()?.device_id;
                    if !previous.is_empty() && previous != response.device_id {
                        // The server maps the device key to its device; a
                        // removed device registers again under a new ID.
                        tracing::warn!("server assigned a new device ID to this device key");
                    }
                    next.complete_registration(&server_key, response.device_id.clone())?;
                    self.commit(next)?;
                    self.initialized = true;
                    crate::account::reporting::update(|s| {
                        s.device_id = response.device_id.clone();
                        s.registration = "上报成功".into();
                        s.registered_at = Some(chrono::Utc::now().timestamp());
                    });
                    self.errors = 0;
                    Ok(self.identity.clone())
                })();
                if let Err(error) = &result {
                    crate::account::reporting::update(|s| {
                        s.registration = format!("上报失败：{error:#}")
                    });
                }
                if result.is_ok() && !self.identity.same_registration_settings(&sent) {
                    self.start_request(completion);
                } else {
                    self.finish(completion, result);
                }
            }
            Err(error) => {
                crate::account::reporting::update(|s| {
                    s.registration = format!("上报失败：{error:#}")
                });
                let error = match error.downcast::<ApiFailure>() {
                    Ok(error) => error,
                    Err(error) => {
                        self.initialized = false;
                        self.finish(completion, Err(error));
                        return;
                    }
                };
                let code = error.code;
                tracing::warn!(code = %code, attempt = self.errors + 1, %error, "device initialization failed");
                if code == ErrorCode::DeviceUnauthorized && self.errors == 0 {
                    // The server rejected the proof for this registration.
                    // Register this installation anew with the same key.
                    let mut next = self.identity.clone();
                    next.clear_registration();
                    if let Err(error) = self.commit(next) {
                        self.initialized = false;
                        self.finish(completion, Err(error));
                        return;
                    }
                    self.errors += 1;
                    self.start_request(completion);
                    return;
                }
                if !matches!(
                    code,
                    ErrorCode::Network | ErrorCode::Internal | ErrorCode::Unavailable
                ) || self.errors >= 2
                {
                    self.initialized = false;
                    self.errors = 0;
                    self.finish(completion, Err(error.into()));
                    return;
                }
                self.errors += 1;
                self.start_request(completion);
            }
        }
    }

    fn finish(&mut self, completion: Completion, result: Result<NativeIdentity>) {
        self.waiters.push(completion);
        for completion in self.waiters.drain(..) {
            let value = match &result {
                Ok(identity) => Ok(identity.clone()),
                Err(error) => Err(if let Some(api) = error.downcast_ref::<ApiFailure>() {
                    api.clone().into()
                } else {
                    anyhow::anyhow!("{error:#}")
                }),
            };
            let _ = completion.send(value);
        }
    }
}
