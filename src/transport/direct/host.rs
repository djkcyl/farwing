//! The host's direct endpoint (`docs/protocol/v2-design.md` §12.1–§12.2):
//! TLS listener, handshake and authorisation, then a bridge that presents
//! the session to the host's signalling loop like the server would.

use super::store;
use super::{LocalDevice, ROLE_CONTROLLER, ROLE_HOST, read_frame, refusal, write_frame};
use crate::session::host_client::HostClient;
use crate::transport::signal::router::Router;
use anyhow::{Context, Result, bail, ensure};
use farwing_protocol::ErrorCode;
use farwing_protocol::error::reason as refusal_reason;
use farwing_protocol::api::{Purpose, Relation};
use crate::features::host::assist::Decision;
use farwing_protocol::direct::{
    self as wire, DirectAuthorized, DirectConfirm, DirectHello, DirectPair, DirectPairNonce,
    DirectPake, DirectPaired, method, pake_kind, types as dtypes,
};
use farwing_protocol::realtime::{
    ControllerInfo, Envelope, PeerIdentity, SessionEnd, SessionIncoming, SessionStart,
    SessionStarted, reason, types,
};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::{Instant, timeout};
use tokio_util::sync::CancellationToken;

/// TCP port of this process's endpoint, 0 when not listening (a profile has
/// at most one endpoint).
static LISTENING: AtomicU16 = AtomicU16::new(0);

/// Whether this process accepts direct connections right now.
pub(crate) fn accepting() -> bool {
    listening_port().is_some()
}

/// The port this process accepts direct connections on.
pub(crate) fn listening_port() -> Option<u16> {
    Some(LISTENING.load(Ordering::Relaxed)).filter(|port| *port != 0)
}

/// Longest wait for the controller's `session.start` after authorisation.
const START_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest wait for the local user to confirm an assistance request.
const CONFIRMATION_TIMEOUT: Duration = Duration::from_secs(65);

pub(crate) struct Endpoint {
    cancel: CancellationToken,
    port: u16,
    _lan: Option<super::lan::Responder>,
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        self.cancel.cancel();
        let _ = LISTENING.compare_exchange(self.port, 0, Ordering::Relaxed, Ordering::Relaxed);
    }
}

impl Endpoint {
    /// Listen for direct connections when this profile allows them.
    pub(crate) fn start(client: HostClient, router: Arc<Router>) -> Option<Self> {
        let settings = crate::transport::nat::settings::current();
        if !settings.direct {
            router.set_port(None);
            return None;
        }
        match Self::bind(client, router, settings.direct_port, settings.lan_discovery) {
            Ok(endpoint) => Some(endpoint),
            Err(error) => {
                tracing::warn!(error = %format!("{error:#}"), "direct endpoint unavailable");
                None
            }
        }
    }

    fn bind(client: HostClient, router: Arc<Router>, port: u16, lan: bool) -> Result<Self> {
        let local = client.direct_device()?;
        let tls = tokio_rustls::TlsAcceptor::from(super::server_tls()?);
        let (listeners, port) = listen(port)?;
        router.set_port(Some(port));
        let cancel = CancellationToken::new();
        let shared = Arc::new(Shared {
            client: client.clone(),
            router,
            tls,
            local,
            port,
            cancel: cancel.clone(),
            pending: Arc::new(tokio::sync::Semaphore::new(wire::MAX_PENDING)),
            own_refreshed: std::sync::Mutex::new(None),
            pairing: std::sync::Mutex::new(PairingLimits::default()),
        });
        for listener in listeners {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.accept(listener).await });
        }
        LISTENING.store(port, Ordering::Relaxed);
        tracing::info!(port, "direct endpoint listening");
        let lan = lan
            .then(|| super::lan::Responder::start(client, Arc::clone(&shared) as Arc<dyn super::lan::Announcer>, cancel.child_token()))
            .flatten();
        Ok(Self {
            cancel,
            port,
            _lan: lan,
        })
    }
}

/// Runs the endpoint while the profile allows direct connections and
/// rebuilds it when the direct switches change (§12.1), so a new port or
/// LAN discovery setting applies without restarting the host. In service
/// mode the resident changes them on behalf of the user's GUI or CLI.
pub(crate) struct Supervisor {
    cancel: CancellationToken,
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}

impl Supervisor {
    pub(crate) fn start(client: HostClient, router: Arc<Router>) -> Self {
        let key = |s: &crate::transport::nat::settings::Settings| {
            (s.direct, s.direct_port, s.lan_discovery)
        };
        let mut changes = crate::transport::nat::settings::subscribe();
        let mut applied = key(&changes.borrow_and_update());
        let mut endpoint = Endpoint::start(client.clone(), Arc::clone(&router));
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = stop.cancelled() => break,
                    changed = changes.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        let next = key(&changes.borrow_and_update());
                        if next == applied {
                            continue;
                        }
                        applied = next;
                        drop(endpoint.take());
                        // Let the old listeners close before the port is bound again.
                        tokio::time::sleep(Duration::from_millis(300)).await;
                        tracing::info!(
                            direct = next.0,
                            port = next.1,
                            lan = next.2,
                            "direct settings changed; endpoint restarted"
                        );
                        endpoint = Endpoint::start(client.clone(), Arc::clone(&router));
                    }
                }
            }
            drop(endpoint);
        });
        Self { cancel }
    }
}

/// IPv4 (and, where possible, IPv6) listeners on the first free port from
/// `port`.
fn listen(port: u16) -> Result<(Vec<TcpListener>, u16)> {
    let mut last = None;
    for candidate in port..=port.saturating_add(wire::PORT_FALLBACKS) {
        match std::net::TcpListener::bind((Ipv4Addr::UNSPECIFIED, candidate)) {
            Ok(v4) => {
                v4.set_nonblocking(true)?;
                let mut listeners = vec![TcpListener::from_std(v4)?];
                if let Ok(v6) = std::net::TcpListener::bind((Ipv6Addr::UNSPECIFIED, candidate)) {
                    v6.set_nonblocking(true)?;
                    listeners.push(TcpListener::from_std(v6)?);
                }
                return Ok((listeners, candidate));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => last = Some(error),
            Err(error) => return Err(error).with_context(|| format!("无法监听直连端口 {candidate}")),
        }
    }
    Err(last.map_or_else(|| anyhow::anyhow!("没有可用端口"), anyhow::Error::from))
        .with_context(|| format!("直连端口 {port}–{} 均被占用", port.saturating_add(wire::PORT_FALLBACKS)))
}

struct Shared {
    client: HostClient,
    router: Arc<Router>,
    tls: tokio_rustls::TlsAcceptor,
    local: LocalDevice,
    port: u16,
    cancel: CancellationToken,
    pending: Arc<tokio::sync::Semaphore>,
    own_refreshed: std::sync::Mutex<Option<std::time::Instant>>,
    pairing: std::sync::Mutex<PairingLimits>,
}

/// Pairing requests pop up on the host's screen: at most one every few
/// seconds, and none from a device for a minute after this side said no.
#[derive(Default)]
struct PairingLimits {
    last: Option<std::time::Instant>,
    declined: std::collections::HashMap<String, std::time::Instant>,
}

const PAIRING_SPACING: Duration = Duration::from_secs(3);
const PAIRING_DECLINED: Duration = Duration::from_secs(60);

/// What an authorised controller may do.
struct Authorized {
    relation: Relation,
    grants: Vec<String>,
    key: Option<[u8; 32]>,
}

/// A refused handshake step, reported to the controller. The controller
/// shows a text for the code and `details.reason`; the message is for logs.
#[derive(Debug)]
struct Refused(farwing_protocol::ErrorBody);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} ({})", self.0.message, self.0.code)
    }
}
impl std::error::Error for Refused {}

fn refuse(code: ErrorCode, message: &str) -> anyhow::Error {
    Refused(refusal(code, message)).into()
}

/// A refusal whose code covers several cases: `details` names the reason
/// (`farwing_protocol::error::reason`) and its parameters.
fn refuse_with(code: ErrorCode, message: &str, details: serde_json::Value) -> anyhow::Error {
    Refused(refusal(code, message).with_details(details)).into()
}

impl super::lan::Announcer for Shared {
    fn announcement(&self) -> Option<(LocalDevice, u16, Vec<String>)> {
        self.client
            .host
            .requested()
            .then(|| (self.local.clone(), self.port, self.methods()))
    }
}

impl Shared {
    async fn accept(self: Arc<Self>, listener: TcpListener) {
        loop {
            let accepted = tokio::select! {
                _ = self.cancel.cancelled() => return,
                accepted = listener.accept() => accepted,
            };
            let (stream, peer) = match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    tracing::debug!(%error, "direct accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
            };
            let Ok(permit) = Arc::clone(&self.pending).try_acquire_owned() else {
                tracing::warn!("too many direct handshakes; connection dropped");
                continue;
            };
            let shared = Arc::clone(&self);
            tokio::spawn(async move {
                if let Err(error) = shared.serve(stream, peer, permit).await {
                    tracing::info!(error = %format!("{error:#}"), "direct connection ended");
                }
            });
        }
    }

    /// Methods this host accepts right now (`docs/access-model.md` §2.2).
    fn methods(&self) -> Vec<String> {
        let mut methods = Vec::new();
        if let Some(user) = self.client.account_user_id() {
            methods.push(method::OWN.to_owned());
            if !store::friend_devices(&user, &self.local.identity.device_id).is_empty() {
                methods.push(method::FRIEND.to_owned());
            }
        }
        methods.push(method::TRUSTED.to_owned());
        if self.client.host.assistance.settings().enabled {
            methods.push(method::ASSIST.to_owned());
        }
        methods
    }

    async fn serve(
        self: Arc<Self>,
        stream: TcpStream,
        peer: SocketAddr,
        permit: tokio::sync::OwnedSemaphorePermit,
    ) -> Result<()> {
        let _ = stream.set_nodelay(true);
        let handshake = Duration::from_millis(wire::HANDSHAKE_TIMEOUT_MS);
        let tls = timeout(handshake, self.tls.accept(stream))
            .await
            .context("TLS 握手超时")?
            .context("TLS 握手失败")?;
        let ekm = super::exported_key(tls.get_ref().1)?;
        let (mut reader, mut writer) = tokio::io::split(tls);
        let (hello, authorized, start_id, start) = match self
            .handshake(&mut reader, &mut writer, &ekm, peer)
            .await
        {
            Ok(Some(result)) => result,
            // Paired: no session follows.
            Ok(None) => return Ok(()),
            Err(error) => {
                if let Some(Refused(body)) = error.downcast_ref::<Refused>() {
                    let _ = write_frame(&mut writer, &Envelope::error(None, body.clone())).await;
                }
                return Err(error);
            }
        };
        drop(permit);
        self.bridge(reader, writer, hello, authorized, start_id, start, peer)
            .await
    }

    async fn read<R: AsyncRead + Unpin>(&self, reader: &mut R, wait: Duration) -> Result<Envelope> {
        tokio::select! {
            _ = self.cancel.cancelled() => bail!("被控端已停止直连"),
            frame = timeout(wait, read_frame(reader)) => frame
                .context("等待对端消息超时")??
                .context("对端关闭了连接"),
        }
    }

    async fn handshake<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        ekm: &[u8],
        peer: SocketAddr,
    ) -> Result<Option<(DirectHello, Authorized, String, SessionStart)>>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let step = Duration::from_millis(wire::HANDSHAKE_TIMEOUT_MS);
        let message = self.read(reader, step).await?;
        ensure!(message.kind == dtypes::HELLO, "对端未发送身份");
        let mut hello: DirectHello = message.body().context("对端身份格式无效")?;
        super::verify_hello(&hello, ROLE_CONTROLLER, ekm)?;
        hello.name = super::sanitize_name(&hello.name);
        ensure!(
            hello.public_key != self.local.identity.public_key(),
            "不能直连本机自己"
        );
        let mut mine = super::hello(&self.local, ROLE_HOST, ekm, self.methods(), self.port);
        mine.admits = self.admits(&hello);
        write_frame(writer, &Envelope::new(dtypes::HELLO, &mine)).await?;
        tracing::info!(%peer, device = %hello.device_id, "direct controller identified");
        if !self.client.is_active() || !self.client.host.requested() {
            return Err(refuse(ErrorCode::HostDisabled, "host access is off"));
        }
        let authorized = loop {
            let message = self.read(reader, step).await?;
            match message.kind.as_str() {
                dtypes::AUTH => break self.by_key(&hello).await?,
                dtypes::PAIR => {
                    let request: DirectPair = message.body().context("配对请求格式无效")?;
                    self.pair(reader, writer, ekm, &hello, &request).await?;
                    return Ok(None);
                }
                dtypes::PAKE => {
                    let pake: DirectPake = message.body().context("PAKE 消息格式无效")?;
                    break match pake.kind.as_str() {
                        pake_kind::ASSIST => self.assist(reader, writer, ekm, &hello, &pake).await?,
                        _ => return Err(refuse(ErrorCode::BadRequest, "unknown authentication method")),
                    };
                }
                types::PING => {
                    write_frame(writer, &Envelope { kind: types::PONG.into(), ..message }).await?;
                }
                _ => return Err(refuse(ErrorCode::BadRequest, "unexpected handshake message")),
            }
        };
        tracing::info!(
            device = %hello.device_id,
            relation = ?authorized.relation,
            "direct controller authorized"
        );
        write_frame(
            writer,
            &Envelope::new(
                dtypes::AUTHORIZED,
                DirectAuthorized {
                    relation: authorized.relation,
                    grants: authorized.grants.clone(),
                },
            ),
        )
        .await?;
        let message = loop {
            let message = self.read(reader, START_TIMEOUT).await?;
            match message.kind.as_str() {
                types::SESSION_START => break message,
                types::PING => {
                    write_frame(writer, &Envelope { kind: types::PONG.into(), ..message }).await?;
                }
                _ => return Err(refuse(ErrorCode::BadRequest, "unexpected handshake message")),
            }
        };
        let id = message.id.clone().context("session.start 缺少请求 ID")?;
        let start: SessionStart = message.body().context("session.start 格式无效")?;
        ensure!(
            farwing_protocol::ids::is_uuid(&start.attempt_id),
            "session.start 缺少尝试 ID"
        );
        Ok(Some((hello, authorized, id, start)))
    }

    /// Whether [`Self::by_key`] would admit `hello` with what this host has
    /// cached, without asking the server: told to the controller, so that a
    /// device admitted by its key never needs a code or a confirmation.
    fn admits(&self, hello: &DirectHello) -> bool {
        if let Some(user) = self.client.account_user_id() {
            let host_id = &self.local.identity.device_id;
            if store::own_device(&user, &hello.public_key).is_some()
                || store::friend_device(&user, host_id, &hello.public_key).is_some()
            {
                return true;
            }
        }
        store::find_trusted(&hello.public_key).is_some()
    }

    /// `direct.pair` (`docs/access-model.md` §7.4): exchange nonces, show
    /// the code, and once both users agreed trust each other with full
    /// control.
    async fn pair<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        ekm: &[u8],
        hello: &DirectHello,
        request: &DirectPair,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let commit = farwing_protocol::crypto::unb64(&request.commit)
            .ok()
            .filter(|c| c.len() == 32)
            .ok_or_else(|| refuse(ErrorCode::BadRequest, "invalid pairing request"))?;
        {
            let mut limits = self.pairing.lock().unwrap_or_else(|e| e.into_inner());
            let now = std::time::Instant::now();
            limits.declined.retain(|_, at| now.duration_since(*at) < PAIRING_DECLINED);
            if limits.declined.contains_key(&hello.device_id) {
                return Err(refuse_with(
                    ErrorCode::RateLimited,
                    "pairing recently declined",
                    serde_json::json!({ "reason": refusal_reason::PAIRING_RECENTLY_DECLINED }),
                ));
            }
            if limits.last.is_some_and(|at| now.duration_since(at) < PAIRING_SPACING) {
                return Err(refuse(ErrorCode::RateLimited, "pairing requests too frequent"));
            }
            limits.last = Some(now);
        }
        let nonce: [u8; 32] = rand::random();
        write_frame(
            writer,
            &Envelope::new(
                dtypes::PAIR_NONCE,
                DirectPairNonce {
                    nonce: farwing_protocol::crypto::b64(&nonce),
                },
            ),
        )
        .await?;
        let message = self
            .read(reader, Duration::from_millis(wire::HANDSHAKE_TIMEOUT_MS))
            .await?;
        ensure!(message.kind == dtypes::PAIR_REVEAL, "对端没有按协议配对");
        let reveal: DirectPairNonce = message.body().context("配对消息格式无效")?;
        let theirs = farwing_protocol::crypto::unb64(&reveal.nonce)
            .ok()
            .filter(|n| n.len() == 32)
            .ok_or_else(|| refuse(ErrorCode::BadRequest, "invalid pairing message"))?;
        if farwing_protocol::crypto::pairing_commit(&theirs).as_slice() != commit.as_slice() {
            return Err(refuse(ErrorCode::BadRequest, "pairing verification failed"));
        }
        let code = farwing_protocol::crypto::pairing_code(ekm, &theirs, &nonce);
        let declined = |this: &Self| {
            this.pairing
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .declined
                .insert(hello.device_id.clone(), std::time::Instant::now());
        };
        // This side's answer and the controller's agreement may come in
        // either order; one read of the connection serves until the latter.
        let asked = self.client.host.assistance.confirm_pairing(
            hello.name.clone(),
            hello.device_id.clone(),
            hello.public_key.clone(),
            code,
        );
        let mut asked = std::pin::pin!(asked);
        let mut controller = std::pin::pin!(read_frame(reader));
        let mut ours = false;
        let mut theirs_agreed = false;
        let waited = timeout(Duration::from_secs(70), async {
            while !(ours && theirs_agreed) {
                tokio::select! {
                    _ = self.cancel.cancelled() => bail!("被控端已停止直连"),
                    decision = &mut asked, if !ours => match decision {
                        Decision::Trust => ours = true,
                        Decision::Unavailable(reason) => {
                            return Err(refuse_with(
                                ErrorCode::AssistUnavailable,
                                "nobody can answer",
                                serde_json::json!({ "reason": reason }),
                            ));
                        }
                        Decision::Reject | Decision::Allow => {
                            declined(self);
                            return Err(refuse_with(
                                ErrorCode::AssistRejected,
                                "pairing declined",
                                serde_json::json!({ "reason": refusal_reason::PAIRING_DECLINED }),
                            ));
                        }
                    },
                    frame = &mut controller, if !theirs_agreed => match frame {
                        Ok(Some(envelope)) if envelope.kind == dtypes::PAIR_CONFIRM => {
                            theirs_agreed = true;
                        }
                        // Anything else, or a closed connection: it gave up.
                        _ => bail!("对端取消了配对"),
                    },
                }
            }
            Ok(())
        })
        .await;
        match waited {
            Ok(result) => result?,
            Err(_) => {
                declined(self);
                return Err(refuse(ErrorCode::Timeout, "pairing timed out"));
            }
        }
        // Each side trusts the other with full control; the controller
        // agreed, so it trusts this device too.
        crate::features::host::assist::trust_paired(&hello.name, &hello.device_id, &hello.public_key);
        store::remember_host(store::KnownHost {
            device_id: hello.device_id.clone(),
            public_key: hello.public_key.clone(),
            name: hello.name.clone(),
            address: String::new(),
            connect_id: String::new(),
            trusted: true,
            last_seen: 0,
        });
        write_frame(
            writer,
            &Envelope::new(dtypes::PAIRED, DirectPaired { grants: farwing_protocol::caps::all() }),
        )
        .await?;
        tracing::info!(device = %hello.device_id, "paired with a controller");
        Ok(())
    }

    /// `direct.auth` (`docs/access-model.md` §2.2): one of the account's
    /// devices, a friend's device under the friend's grants, or a device
    /// this host trusts. The device ID was checked against the key.
    async fn by_key(&self, hello: &DirectHello) -> Result<Authorized> {
        if let Some(user) = self.client.account_user_id() {
            let host_id = self.local.identity.device_id.clone();
            let mut own = store::own_device(&user, &hello.public_key);
            let mut friend = store::friend_device(&user, &host_id, &hello.public_key);
            // Friends' grants change on the server: ask it again when it is
            // reachable (at most every few seconds); the cache stands in
            // while it is not.
            if own.is_none() && self.refresh_account_cache().await {
                own = store::own_device(&user, &hello.public_key);
                friend = store::friend_device(&user, &host_id, &hello.public_key);
            }
            if own.is_some() {
                return Ok(Authorized {
                    relation: Relation::Own,
                    grants: farwing_protocol::caps::all(),
                    key: None,
                });
            }
            if let Some(friend) = friend {
                return Ok(Authorized {
                    relation: Relation::Friend,
                    grants: store::with_view(&friend.grants),
                    key: None,
                });
            }
        }
        if let Some(device) = store::find_trusted(&hello.public_key) {
            store::touch_trusted(&hello.public_key);
            return Ok(Authorized {
                relation: Relation::Trusted,
                grants: store::with_view(&device.grants),
                key: None,
            });
        }
        Err(refuse_with(
            ErrorCode::Forbidden,
            "device not trusted",
            serde_json::json!({ "reason": refusal_reason::NOT_TRUSTED }),
        ))
    }

    /// Fetch the account's devices and friend grants once in a while, for a
    /// device that is not cached yet.
    async fn refresh_account_cache(&self) -> bool {
        {
            let mut last = self.own_refreshed.lock().unwrap_or_else(|e| e.into_inner());
            if last.is_some_and(|at| at.elapsed() < Duration::from_secs(10)) {
                return false;
            }
            *last = Some(std::time::Instant::now());
        }
        match timeout(Duration::from_secs(5), self.client.refresh_account_cache()).await {
            Ok(Ok(())) => true,
            Ok(Err(error)) => {
                tracing::debug!(error = %format!("{error:#}"), "account device cache not refreshed");
                false
            }
            Err(_) => false,
        }
    }

    /// Remote assistance with the assistance code or local confirmation.
    async fn assist<R, W>(
        &self,
        reader: &mut R,
        writer: &mut W,
        ekm: &[u8],
        hello: &DirectHello,
        pake: &DirectPake,
    ) -> Result<Authorized>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let assistance = &self.client.host.assistance;
        let settings = assistance.settings();
        if !settings.enabled || settings.validate().is_err() {
            return Err(refuse_with(
                ErrorCode::AssistUnavailable,
                "no access code",
                serde_json::json!({ "reason": refusal_reason::NO_ACCESS_CODE }),
            ));
        }
        if assistance.locked() {
            return Err(refuse_with(
                ErrorCode::RateLimited,
                "too many wrong access codes",
                serde_json::json!({ "reason": refusal_reason::CODE_LOCKED, "minutes": 5 }),
            ));
        }
        let mut key = None;
        if settings.mode.needs_password() {
            if pake.messages.is_empty() {
                return Err(refuse(ErrorCode::AssistCodeRequired, "access code required"));
            }
            let (messages, confirm, keys) = assistance.direct_pake(ekm, &pake.messages);
            write_frame(
                writer,
                &Envelope::new(
                    dtypes::PAKE,
                    DirectPake {
                        kind: pake_kind::ASSIST.into(),
                        messages,
                        confirm,
                        confirmation: settings.mode.needs_confirmation(),
                    },
                ),
            )
            .await?;
            let received = match self
                .read(reader, Duration::from_millis(wire::HANDSHAKE_TIMEOUT_MS))
                .await
            {
                Ok(message) if message.kind == dtypes::CONFIRM => {
                    message.body::<DirectConfirm>().ok().map(|c| c.confirm)
                }
                _ => None,
            };
            key = received.and_then(|confirm| {
                keys.into_iter().flatten().find(|key| {
                    farwing_protocol::crypto::direct_confirm_matches(key, ROLE_CONTROLLER, ekm, &confirm)
                })
            });
            if key.is_none() {
                assistance.record_failure();
                tracing::warn!(device = %hello.device_id, "direct assistance refused: wrong code");
                return Err(refuse(ErrorCode::AssistCodeInvalid, "wrong access code"));
            }
        } else {
            write_frame(
                writer,
                &Envelope::new(
                    dtypes::PAKE,
                    DirectPake {
                        kind: pake_kind::ASSIST.into(),
                        confirmation: true,
                        ..Default::default()
                    },
                ),
            )
            .await?;
        }
        if settings.mode.needs_confirmation() {
            let asked = timeout(
                CONFIRMATION_TIMEOUT,
                assistance.confirm_direct(hello.name.clone(), hello.device_id.clone(), hello.public_key.clone()),
            );
            // Anything from the controller while it waits, or a closed
            // connection, means it gave up: the request ends here too.
            let decision = tokio::select! {
                _ = self.cancel.cancelled() => Decision::Reject,
                decision = asked => decision.unwrap_or(Decision::Reject),
                _ = read_frame(reader) => Decision::Reject,
            };
            match decision {
                Decision::Allow => {}
                // Trusted from now on: so is this session, and the
                // controller learns it needs no code next time.
                Decision::Trust => {
                    return Ok(Authorized {
                        relation: Relation::Trusted,
                        grants: store::find_trusted(&hello.public_key)
                            .map_or_else(|| store::with_view(&settings.grants()), |d| store::with_view(&d.grants)),
                        key,
                    });
                }
                Decision::Unavailable(reason) => {
                    return Err(refuse_with(
                        ErrorCode::AssistUnavailable,
                        "nobody can answer",
                        serde_json::json!({ "reason": reason }),
                    ));
                }
                Decision::Reject => return Err(refuse(ErrorCode::AssistRejected, "connection declined")),
            }
        }
        Ok(Authorized {
            relation: Relation::Assist,
            grants: settings.grants(),
            key,
        })
    }

    /// Present the authorised session to the host and relay its messages
    /// until either side ends it.
    #[allow(clippy::too_many_arguments)]
    async fn bridge<R, W>(
        &self,
        mut reader: R,
        mut writer: W,
        hello: DirectHello,
        authorized: Authorized,
        start_id: String,
        start: SessionStart,
        peer: SocketAddr,
    ) -> Result<()>
    where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let sid = uuid::Uuid::new_v4().to_string();
        let participant_id = uuid::Uuid::new_v4().to_string();
        let purpose = start.purpose.unwrap_or(Purpose::Desktop);
        let assist = authorized.relation == Relation::Assist;
        let incoming = SessionIncoming {
            participant_id: participant_id.clone(),
            attempt_id: start.attempt_id.clone(),
            purpose,
            connect_type: if assist {
                farwing_protocol::session::ConnectType::Assist
            } else {
                farwing_protocol::session::ConnectType::Account
            },
            relation: authorized.relation,
            // Assistance capabilities come from the host's template.
            grants: if assist { Vec::new() } else { authorized.grants.clone() },
            controller: ControllerInfo {
                device_id: hello.device_id.clone(),
                alias: hello.name.clone(),
                user_id: String::new(),
                username: String::new(),
                nickname: hello.name.clone(),
                platform: hello.platform,
                version: hello.version.clone(),
                public_key: hello.public_key.clone(),
                caps_limit: None,
            },
            options: start.options,
            capability: start.capability,
            ice_servers: Vec::new(),
            force_relay: false,
            network: Default::default(),
        };
        let (sender, mut outgoing) = mpsc::unbounded_channel();
        self.router.register(&sid, sender);
        if let Some(key) = authorized.key {
            self.router.set_session_key(&sid, key);
        }
        self.router
            .inject(Envelope::new(types::SESSION_INCOMING, &incoming).with_sid(&sid));
        let started = SessionStarted {
            participant_id,
            session_id: sid.clone(),
            ice_servers: Vec::new(),
            force_relay: false,
            network: Default::default(),
            host: PeerIdentity {
                device_id: self.local.identity.device_id.clone(),
                public_key: self.local.identity.public_key(),
            },
            relation: authorized.relation,
            grants: authorized.grants,
        };
        let result = async {
            write_frame(&mut writer, &Envelope::ack(&start_id, &started)).await?;
            tracing::info!(%peer, device = %hello.device_id, ?purpose, "direct session started");
            // Frame reads are not cancel-safe: one task owns the read half.
            let (frames_tx, mut frames) = mpsc::unbounded_channel();
            let reading = tokio::spawn(async move {
                loop {
                    let frame = read_frame(&mut reader).await;
                    let end = !matches!(frame, Ok(Some(_)));
                    if frames_tx.send(frame).is_err() || end {
                        break;
                    }
                }
            });
            let (out, mut out_rx) = mpsc::unbounded_channel::<Envelope>();
            let writing = tokio::spawn(async move {
                while let Some(envelope) = out_rx.recv().await {
                    write_frame(&mut writer, &envelope).await?;
                }
                use tokio::io::AsyncWriteExt;
                let _ = writer.shutdown().await;
                Ok::<(), anyhow::Error>(())
            });
            let mut ping = tokio::time::interval(Duration::from_millis(wire::PING_INTERVAL_MS));
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let idle = Duration::from_millis(wire::IDLE_TIMEOUT_MS);
            let mut idle_deadline = Instant::now() + idle;
            let ended = |why: &str| {
                Envelope::new(types::SESSION_ENDED, SessionEnd { reason: why.to_owned() }).with_sid(&sid)
            };
            loop {
                tokio::select! {
                    frame = frames.recv() => match frame {
                        Some(Ok(Some(envelope))) => {
                            idle_deadline = Instant::now() + idle;
                            match envelope.kind.as_str() {
                                types::PING => { let _ = out.send(Envelope { kind: types::PONG.into(), ..envelope }); }
                                types::PONG => {}
                                types::RTC_OFFER | types::RTC_CANDIDATE
                                    if envelope.sid.as_deref() == Some(sid.as_str()) =>
                                {
                                    self.router.inject(envelope);
                                }
                                types::SESSION_END => {
                                    self.router.inject(ended(reason::CONTROLLER_LEFT));
                                    break;
                                }
                                _ => {}
                            }
                        }
                        _ => {
                            self.router.inject(ended(reason::CONTROLLER_LEFT));
                            break;
                        }
                    },
                    envelope = outgoing.recv() => match envelope {
                        Some(envelope) if envelope.kind == types::SESSION_END => {
                            // The host ended this participant; tell the
                            // controller as the server would.
                            let why = envelope.body::<SessionEnd>().map(|e| e.reason).unwrap_or_default();
                            let _ = out.send(ended(&why));
                            break;
                        }
                        Some(envelope) => { let _ = out.send(envelope); }
                        None => break,
                    },
                    _ = ping.tick() => {
                        let _ = out.send(Envelope::new(types::PING, farwing_protocol::realtime::Ping { t: chrono::Utc::now().timestamp_millis() }));
                    }
                    _ = tokio::time::sleep_until(idle_deadline) => {
                        tracing::warn!("direct controller stopped responding");
                        self.router.inject(ended(reason::CONTROLLER_LEFT));
                        break;
                    }
                    _ = self.cancel.cancelled() => {
                        let _ = out.send(ended(reason::HOST_CLOSED));
                        self.router.inject(ended(reason::HOST_CLOSED));
                        break;
                    }
                }
            }
            drop(out);
            let _ = timeout(Duration::from_secs(2), writing).await;
            reading.abort();
            Ok(())
        }
        .await;
        self.router.unregister(&sid);
        tracing::info!(device = %hello.device_id, "direct session closed");
        result
    }
}
