//! Explicit foreground host adapter for a configured self-hosted signaling service.
//! This path owns its HTTP client and never creates an NrdApi.

use super::host_client::HostClient;
use crate::{account::api::RoomSession, transport::signal::{SignalRole, SignalSession}};
use anyhow::{Context, Result, ensure};
use reqwest::{Client, Method, Url, redirect::Policy};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::{sync::Arc, time::Duration};
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

const HTTP_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub(crate) struct LocalHostApi {
    client: Client,
    base: Url,
    device_id: String,
    token: Arc<str>,
}

impl std::fmt::Debug for LocalHostApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LocalHostApi")
            .field("base", &self.base)
            .field("device_id", &self.device_id)
            .field("token", &"***REDACTED***")
            .finish()
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PairRequest<'a> {
    role: &'static str,
    platform: &'static str,
    name: &'a str,
    code: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairReply {
    device_id: String,
    token: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct StoredPair {
    schema: u8,
    device_id: String,
    token: String,
}

impl std::fmt::Debug for StoredPair {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StoredPair")
            .field("schema", &self.schema)
            .field("device_id", &self.device_id)
            .field("token", &"***REDACTED***")
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RoomReply {
    ticket: String,
    signaling_url: String,
}

#[derive(Serialize)]
struct ControllableRequest {
    enabled: bool,
}

impl LocalHostApi {
    pub(crate) async fn open(
        server_url: &str,
        code: Option<&str>,
        name: &str,
    ) -> Result<Arc<Self>> {
        let base = api_base(server_url)?;
        let origin = base.origin().ascii_serialization();
        let key = {
            use sha2::{Digest, Sha256};
            format!("{:x}", Sha256::digest(origin.as_bytes()))
        };
        let credential = crate::account::auth::SecretEntry::new("com.openuuyc.self-host", &key)
            .context("open self-host credential storage")?;
        let client = http_client()?;
        let stored = match credential.get_secret() {
            Ok(bytes) => serde_json::from_slice::<StoredPair>(&bytes)
                .context("saved self-host pairing is invalid")?,
            Err(keyring::Error::NoEntry) => {
                let code =
                    code.context("first use requires --pair-code from the self-host service")?;
                ensure!(
                    !code.trim().is_empty() && code.len() <= 256,
                    "pairing code is empty or too long"
                );
                ensure!(
                    !name.trim().is_empty()
                        && name.len() <= 128
                        && !name.chars().any(char::is_control),
                    "host name is empty, too long, or contains a control character"
                );
                let pair_request = PairRequest {
                    role: "host",
                    platform: "windows",
                    name,
                    code,
                };
                let reply: PairReply = send_json(
                    &client,
                    base.join("v1/pair")?,
                    Method::POST,
                    None,
                    Some(&pair_request),
                )
                .await
                .context("pair with the configured self-host service")?;
                validate_credentials(&reply.device_id, &reply.token)?;
                let stored = StoredPair {
                    schema: 1,
                    device_id: reply.device_id,
                    token: reply.token,
                };
                credential
                    .set_secret(&serde_json::to_vec(&stored)?)
                    .context("save self-host pairing in Windows Credential Manager")?;
                stored
            }
            Err(error) => return Err(error).context("read self-host credentials"),
        };
        ensure!(
            stored.schema == 1,
            "unsupported saved self-host credential format"
        );
        validate_credentials(&stored.device_id, &stored.token)?;
        Ok(Arc::new(Self {
            client,
            base,
            device_id: stored.device_id,
            token: Arc::from(stored.token),
        }))
    }

    pub(crate) fn device_id(&self) -> &str {
        &self.device_id
    }

    pub(crate) async fn create_room(&self) -> Result<RoomSession> {
        let body = serde_json::json!({});
        let reply: RoomReply = send_json(
            &self.client,
            self.base.join("v1/host/room")?,
            Method::POST,
            Some(&self.token),
            Some(&body),
        )
        .await
        .context("request a self-host signaling room")?;
        let signaling = signaling_url(&self.base, &reply.signaling_url)?;
        RoomSession::from_local(reply.ticket, signaling)
    }

    pub(crate) async fn set_controllable(&self, enabled: bool) -> Result<()> {
        let response = self
            .client
            .put(self.base.join("v1/host/controllable")?)
            .bearer_auth(self.token.as_ref())
            .json(&ControllableRequest { enabled })
            .send()
            .await
            .context("update self-host controllable state")?;
        ensure!(
            response.status().is_success(),
            "self-host server rejected controllable state (HTTP {})",
            response.status()
        );
        Ok(())
    }
}

pub(crate) fn http_client() -> Result<Client> {
    Ok(crate::transport::http_client()
        .https_only(false)
        .no_proxy()
        .redirect(Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(HTTP_TIMEOUT)
        .http1_only()
        .user_agent(concat!("OpenUUYC/", env!("CARGO_PKG_VERSION")))
        .build()
        .context("build self-host HTTP client")?)
}

pub(crate) fn api_base(value: &str) -> Result<Url> {
    let mut url = Url::parse(value).context("self-host server URL must be absolute")?;
    ensure!(
        matches!(url.scheme(), "http" | "https")
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "self-host server URL must contain only an HTTP(S) origin"
    );
    if url.scheme() == "http" {
        ensure!(
            crate::transport::signal::is_loopback_host(url.host_str().unwrap()),
            "plain HTTP is allowed only for a loopback self-host service"
        );
    }
    ensure!(
        !is_official_nrd_host(url.host_str().unwrap()),
        "self-host mode cannot use the official NRD service"
    );
    url.set_path("/");
    Ok(url)
}

pub(crate) fn signaling_url(base: &Url, value: &str) -> Result<String> {
    let url = Url::parse(value).context("self-host service returned an invalid signaling URL")?;
    let expected_scheme = if base.scheme() == "https" { "wss" } else { "ws" };
    ensure!(
        url.scheme() == expected_scheme
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none(),
        "self-host signaling URL must use the configured secure scheme without query parameters"
    );
    let same_host = url
        .host_str()
        .zip(base.host_str())
        .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right));
    ensure!(
        same_host && url.port_or_known_default() == base.port_or_known_default(),
        "self-host signaling URL must use the configured API origin"
    );
    ensure!(
        url.path() == "/socket.io/",
        "self-host signaling URL must use /socket.io/"
    );
    if url.scheme() == "ws" {
        ensure!(
            crate::transport::signal::is_loopback_host(url.host_str().unwrap()),
            "plain WebSocket is allowed only on loopback"
        );
    }
    ensure!(
        !is_official_nrd_host(url.host_str().unwrap()),
        "self-host mode cannot signal through the official NRD service"
    );
    Ok(url.into())
}

fn validate_credentials(device_id: &str, token: &str) -> Result<()> {
    ensure!(
        device_id.len() == 16
            && device_id
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit()),
        "self-host service returned an invalid device ID"
    );
    ensure!(
        !token.is_empty() && token.len() <= 4096,
        "self-host service returned an invalid device token"
    );
    Ok(())
}

fn is_official_nrd_host(host: &str) -> bool {
    let host = host.trim_matches(['[', ']']).trim_end_matches('.').to_ascii_lowercase();
    host == "nrd.nie.163.com" || host.ends_with(".nrd.nie.163.com")
}

async fn send_json<T: DeserializeOwned, B: Serialize>(
    client: &Client,
    url: Url,
    method: Method,
    token: Option<&str>,
    body: Option<&B>,
) -> Result<T> {
    let mut request = client.request(method, url);
    if let Some(token) = token {
        request = request.bearer_auth(token);
    }
    if let Some(body) = body {
        request = request.json(body);
    }
    let response = request.send().await.context("send self-host HTTP request")?;
    ensure!(
        response.status().is_success(),
        "self-host server returned HTTP {}",
        response.status()
    );
        response
            .json::<T>()
            .await
            .context("decode self-host server response")
}

pub(crate) async fn run(server_url: String, code: Option<String>, name: Option<String>) -> Result<()> {
    let cancel = CancellationToken::new();
    let signal_cancel = cancel.clone();
    let signal_task = tokio::spawn(async move {
        match tokio::signal::ctrl_c().await {
            Ok(()) => tracing::info!("Ctrl-C received; stopping the self-host session"),
            Err(error) => tracing::warn!(%error, "Ctrl-C listener failed; stopping the self-host session"),
        }
        signal_cancel.cancel();
    });
    let name = name.unwrap_or_else(|| {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "OpenUUYC Windows host".into())
    });

    let api = tokio::select! {
        _ = cancel.cancelled() => return Ok(()),
        result = LocalHostApi::open(&server_url, code.as_deref(), &name) => result?,
    };
    let _owner = reserve_device(api.device_id())?;
    let client = HostClient::local(api.clone())?;
    tracing::info!(device_id = api.device_id(), server = %api.base, "paired with self-host service");
    println!("Self-host host ID: {}. Press Ctrl+C to stop.", api.device_id());

    // Start closed; only the connected host room opens admission.
    if let Err(error) = api.set_controllable(false).await {
        tracing::warn!(%error, "could not clear stale self-host admission state");
    }
    let result = run_rooms(client.clone(), api.clone(), cancel.clone()).await;

    client.host.set_allowed(false);
    client.host.persist_settings().await;
    if let Err(error) = api.set_controllable(false).await {
        tracing::warn!(%error, "self-host controllable state did not close cleanly");
    }
    client.close().await;
    signal_task.abort();
    result
}

async fn run_rooms(
    client: HostClient,
    api: Arc<LocalHostApi>,
    cancel: CancellationToken,
) -> Result<()> {
    let mut retry = Duration::from_secs(1);
    loop {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = api.set_controllable(false).await {
            tracing::warn!(%error, "could not close self-host admission before room refresh");
        }
        let room = tokio::select! {
            _ = cancel.cancelled() => return Ok(()),
            result = client.create_host_room(-1) => match result {
                Ok(room) => room,
                Err(error) => {
                    tracing::warn!(error = %format!("{error:#}"), retry_seconds = retry.as_secs(), "self-host room request failed; retrying");
                    wait_retry(&cancel, retry).await;
                    retry = (retry * 2).min(Duration::from_secs(30));
                    continue;
                }
            }
        };
        let session = match SignalSession::connect_observed(room, SignalRole::Host, &cancel, None).await {
            Ok(session) => session,
            Err(_error) if cancel.is_cancelled() => return Ok(()),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), retry_seconds = retry.as_secs(), "self-host signaling login failed; requesting a fresh room");
                wait_retry(&cancel, retry).await;
                retry = (retry * 2).min(Duration::from_secs(30));
                continue;
            }
        };
        tracing::info!("self-host signaling room connected");
        retry = Duration::from_secs(1);
        let (shutdown, shutdown_receiver) = oneshot::channel();
        let alive = session.keep_alive_host(shutdown_receiver, client.clone());
        tokio::pin!(alive);
        tokio::select! {
            _ = cancel.cancelled() => {
                let _ = shutdown.send(());
                if let Err(error) = (&mut alive).await {
                    tracing::warn!(%error, "self-host session close reported an error");
                }
                return Ok(());
            }
            result = &mut alive => {
                if let Err(error) = result {
                    tracing::warn!(error = %format!("{error:#}"), "self-host room ended; acquiring a fresh ticket");
                }
            }
        }
        if cancel.is_cancelled() {
            return Ok(());
        }
        if let Err(error) = api.set_controllable(false).await {
            tracing::warn!(%error, "self-host controllable state did not close after room loss");
        }
        wait_retry(&cancel, retry).await;
        retry = (retry * 2).min(Duration::from_secs(30));
    }
}

async fn wait_retry(cancel: &CancellationToken, delay: Duration) {
    tokio::select! {
        _ = cancel.cancelled() => {},
        _ = tokio::time::sleep(delay) => {},
    }
}

fn reserve_device(device_id: &str) -> Result<crate::platform::windows::host_service::pipe::Handle> {
    use windows::{
        Win32::{Foundation::{ERROR_ALREADY_EXISTS, GetLastError}, System::Threading::CreateMutexW},
        core::PCWSTR,
    };
    use sha2::{Digest, Sha256};
    let digest = format!("{:x}", Sha256::digest(device_id.as_bytes()));
    let name: Vec<u16> = format!("Global\\OpenUUYC.SelfHost.{digest}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let handle = crate::platform::windows::host_service::pipe::Handle(unsafe {
        CreateMutexW(None, false, PCWSTR(name.as_ptr()))?
    });
    ensure!(
        unsafe { GetLastError() } != ERROR_ALREADY_EXISTS,
        "this self-host device is already running"
    );
    Ok(handle)
}
