//! A controller's direct connection to a host (`docs/protocol/v2-design.md`
//! §12.2, `docs/access-model.md` §2.4): connect, check the host's identity,
//! authorise, and hand the channel to a [`SignalSession`] for the session
//! itself.

use super::store::{self, KnownHost};
use super::{LocalDevice, ROLE_CONTROLLER, ROLE_HOST, Target, read_frame, write_frame};
use crate::account::api::ApiFailure;
use crate::transport::signal::SignalSession;
use anyhow::{Context, Result, bail, ensure};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::Relation;
use farwing_protocol::direct::{
    self as wire, DirectAuth, DirectAuthorized, DirectConfirm, DirectHello, DirectPair,
    DirectPairNonce, DirectPake, DirectPaired, method, pake_kind, types as dtypes,
};
use farwing_protocol::realtime::{Envelope, types};
use std::net::SocketAddr;
use std::time::Duration;
use tokio::io::AsyncRead;
use tokio::net::TcpStream;
use tokio::time::timeout;
use tokio_util::sync::CancellationToken;

/// How the controller proves it may connect.
#[derive(Clone)]
pub(crate) enum Auth {
    /// By its device key: one of the host account's devices, a friend's
    /// device, or a device the host trusts.
    Key,
    /// As a visitor: with the host's access code, or `None` to ask the
    /// host's user to confirm.
    Code(Option<String>),
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Key => "Key",
            Self::Code(Some(_)) => "Code",
            Self::Code(None) => "Confirmation",
        })
    }
}

/// An authorised direct connection, ready for `session.start`.
pub(crate) struct Connected {
    pub signal: SignalSession,
    pub host: DirectHello,
    pub relation: Relation,
    /// The PAKE key of an access code, bound into the session's SDP.
    pub pake_key: Option<[u8; 32]>,
}

/// Longest wait for a host decision that may involve its user.
const DECISION_TIMEOUT: Duration = Duration::from_secs(70);
const STEP_TIMEOUT: Duration = Duration::from_millis(wire::HANDSHAKE_TIMEOUT_MS);

/// Resolve `target` and connect to the first address that answers.
async fn dial(target: &Target) -> Result<(TcpStream, SocketAddr)> {
    let addresses: Vec<SocketAddr> = timeout(
        Duration::from_secs(5),
        tokio::net::lookup_host((target.host.as_str(), target.port)),
    )
    .await
    .context("解析被控端地址超时")?
    .with_context(|| format!("无法解析 {}", target.host))?
    .collect();
    ensure!(!addresses.is_empty(), "无法解析 {}", target.host);
    // Every address in parallel (staggered); the first that answers wins.
    let detail = match timeout(
        Duration::from_secs(3 * addresses.len().min(3) as u64),
        crate::transport::happy_eyeballs::connect(&addresses),
    )
    .await
    {
        Ok(Ok(stream)) => {
            let address = stream.peer_addr().context("被控端连接没有对端地址")?;
            return Ok((stream, address));
        }
        Ok(Err(error)) => error.to_string(),
        Err(_) => "连接超时".to_owned(),
    };
    // Nobody listening: the host is offline or not reachable from here.
    Err(ApiFailure::new(ErrorCode::DeviceOffline, format!("无法连接 {target}：{detail}")).into())
}

async fn read<R: AsyncRead + Unpin>(reader: &mut R, wait: Duration) -> Result<Envelope> {
    let envelope = timeout(wait, read_frame(reader))
        .await
        .context("等待被控端响应超时")??
        .context("被控端关闭了连接")?;
    if envelope.kind == types::ERROR {
        let error: farwing_protocol::ErrorBody = envelope.body().context("被控端错误格式无效")?;
        return Err(ApiFailure::new(error.code, error.message).into());
    }
    Ok(envelope)
}

/// Connect and authorise. `cancel` aborts every wait.
pub(crate) async fn connect(
    target: &Target,
    local: &LocalDevice,
    auth: Auth,
    cancel: &CancellationToken,
) -> Result<Connected> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("connection cancelled"),
        result = connect_admitted(target, local, auth) => result,
    }
}

/// A host that says it admits this device by key is asked so first. Its
/// answer comes from caches that it may refresh before deciding (a friend's
/// access withdrawn meanwhile): then the code the user gave, or asking the
/// host's user, still applies, on a new connection.
async fn connect_admitted(target: &Target, local: &LocalDevice, auth: Auth) -> Result<Connected> {
    match connect_inner(target, local, auth.clone(), true).await {
        Err(error)
            if matches!(auth, Auth::Code(_))
                && error
                    .downcast_ref::<ApiFailure>()
                    .is_some_and(|failure| failure.code == ErrorCode::Forbidden) =>
        {
            tracing::info!(%error, "host no longer admits this device by key; using the code or confirmation");
            connect_inner(target, local, auth, false).await
        }
        result => result,
    }
}

type Reader = tokio::io::ReadHalf<tokio_rustls::client::TlsStream<TcpStream>>;
type Writer = tokio::io::WriteHalf<tokio_rustls::client::TlsStream<TcpStream>>;

/// A TLS connection to a host that has shown who it is.
struct Opened {
    reader: Reader,
    writer: Writer,
    host: DirectHello,
    ekm: Vec<u8>,
    address: SocketAddr,
}

/// Connect, and exchange and check identities.
async fn open(target: &Target, local: &LocalDevice) -> Result<Opened> {
    let (stream, address) = dial(target).await?;
    let _ = stream.set_nodelay(true);
    let name = rustls::pki_types::ServerName::try_from("farwing.direct").expect("static name");
    let tls = timeout(
        STEP_TIMEOUT,
        tokio_rustls::TlsConnector::from(super::client_tls()?).connect(name, stream),
    )
    .await
    .context("建立加密连接超时")?
    .context("建立加密连接失败")?;
    let ekm = super::exported_key(tls.get_ref().1)?;
    let (mut reader, mut writer) = tokio::io::split(tls);
    let mine = super::hello(local, ROLE_CONTROLLER, &ekm, Vec::new(), 0);
    write_frame(&mut writer, &Envelope::new(dtypes::HELLO, &mine)).await?;
    let message = read(&mut reader, STEP_TIMEOUT).await?;
    ensure!(message.kind == dtypes::HELLO, "被控端未发送身份");
    let mut host: DirectHello = message.body().context("被控端身份格式无效")?;
    super::verify_hello(&host, ROLE_HOST, &ekm)?;
    host.name = super::sanitize_name(&host.name);
    // Keep what the list shows about this host current, also when it no
    // longer trusts this device.
    store::note_host_trust(&host.device_id, host.admits);
    Ok(Opened {
        reader,
        writer,
        host,
        ekm: ekm.to_vec(),
        address,
    })
}

async fn connect_inner(
    target: &Target,
    local: &LocalDevice,
    auth: Auth,
    by_key_if_admitted: bool,
) -> Result<Connected> {
    let Opened {
        mut reader,
        mut writer,
        host,
        ekm,
        address,
    } = open(target, local).await?;
    // A host that admits this device by its key needs neither a code nor a
    // confirmation, whichever was asked for.
    let auth = if host.admits && by_key_if_admitted { Auth::Key } else { auth };
    tracing::info!(%address, device = %host.device_id, methods = ?host.methods, ?auth, "direct host identified");
    let mut pake_key = None;
    match &auth {
        Auth::Key => {
            write_frame(
                &mut writer,
                &Envelope::new(
                    dtypes::AUTH,
                    DirectAuth {
                        method: if host.methods.iter().any(|m| m == method::OWN) {
                            method::OWN.into()
                        } else {
                            method::TRUSTED.into()
                        },
                    },
                ),
            )
            .await?;
        }
        Auth::Code(code) => {
            let states: Vec<_> = match code {
                Some(code) => (0..farwing_protocol::crypto::ASSIST_PAKE_SLOTS)
                    .map(|_| farwing_protocol::crypto::pake_start(code))
                    .collect(),
                None => Vec::new(),
            };
            let (states, messages): (Vec<_>, Vec<_>) = states.into_iter().unzip();
            write_frame(
                &mut writer,
                &Envelope::new(
                    dtypes::PAKE,
                    DirectPake {
                        kind: pake_kind::ASSIST.into(),
                        messages,
                        ..Default::default()
                    },
                ),
            )
            .await?;
            let reply: DirectPake = read(&mut reader, STEP_TIMEOUT)
                .await?
                .body()
                .context("协助应答格式无效")?;
            if !states.is_empty() {
                let key = states
                    .into_iter()
                    .zip(reply.messages.iter().zip(&reply.confirm))
                    .filter_map(|(state, (message, confirm))| {
                        let key = state.finish(message).ok()?;
                        farwing_protocol::crypto::direct_confirm_matches(&key, ROLE_HOST, &ekm, confirm)
                            .then_some(key)
                    })
                    .next();
                let Some(key) = key else {
                    return Err(ApiFailure::new(ErrorCode::AssistCodeInvalid, "访问码不正确").into());
                };
                write_frame(
                    &mut writer,
                    &Envelope::new(
                        dtypes::CONFIRM,
                        DirectConfirm {
                            confirm: farwing_protocol::crypto::direct_confirm(&key, ROLE_CONTROLLER, &ekm),
                        },
                    ),
                )
                .await?;
                pake_key = Some(key);
            }
            if reply.confirmation {
                tracing::info!("waiting for the host user to confirm the connection");
            }
        }
    }
    let wait = if matches!(auth, Auth::Code(_)) { DECISION_TIMEOUT } else { STEP_TIMEOUT };
    let message = read(&mut reader, wait).await?;
    ensure!(message.kind == dtypes::AUTHORIZED, "被控端未授权本次连接");
    let authorized: DirectAuthorized = message.body().context("授权消息格式无效")?;
    store::remember_host(KnownHost {
        device_id: host.device_id.clone(),
        public_key: host.public_key.clone(),
        name: host.name.clone(),
        address: target.to_string(),
        connect_id: String::new(),
        trusted: authorized.relation != Relation::Assist,
        last_seen: 0,
    });
    tracing::info!(device = %host.device_id, relation = ?authorized.relation, grants = ?authorized.grants, "direct connection authorized");
    Ok(Connected {
        signal: SignalSession::direct(reader, writer, address.ip()),
        host,
        relation: authorized.relation,
        pake_key,
    })
}

/// Pair with the host `device_id` at `target` (`docs/access-model.md`
/// §7.4). `shown` gets the code to compare; `agreed` resolves once this
/// side's user agrees, and pairing completes once the host's user agrees
/// too. Returns the host, which then trusts this device.
pub(crate) async fn pair(
    target: &Target,
    device_id: &str,
    local: &LocalDevice,
    shown: impl FnOnce(String),
    agreed: tokio::sync::oneshot::Receiver<()>,
    cancel: &CancellationToken,
) -> Result<DirectHello> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => bail!("配对已取消"),
        result = pair_inner(target, device_id, local, shown, agreed) => result,
    }
}

async fn pair_inner(
    target: &Target,
    device_id: &str,
    local: &LocalDevice,
    shown: impl FnOnce(String),
    agreed: tokio::sync::oneshot::Receiver<()>,
) -> Result<DirectHello> {
    let Opened {
        mut reader,
        mut writer,
        host,
        ekm,
        address,
    } = open(target, local).await?;
    // The hello is signed by the key the ID derives from: whatever answers
    // at the address, only the device the user chose pairs.
    ensure!(
        host.device_id == device_id,
        "{target} 上的设备不是所选的设备（{}）",
        crate::account::api::device_id_text(device_id)
    );
    tracing::info!(%address, device = %host.device_id, "pairing with a host");
    let nonce: [u8; 32] = rand::random();
    let commit = farwing_protocol::crypto::pairing_commit(&nonce);
    write_frame(
        &mut writer,
        &Envelope::new(
            dtypes::PAIR,
            DirectPair {
                commit: farwing_protocol::crypto::b64(&commit),
            },
        ),
    )
    .await?;
    let message = read(&mut reader, STEP_TIMEOUT).await?;
    ensure!(message.kind == dtypes::PAIR_NONCE, "被控端没有按协议配对");
    let theirs: DirectPairNonce = message.body().context("配对应答格式无效")?;
    let host_nonce = farwing_protocol::crypto::unb64(&theirs.nonce)
        .ok()
        .filter(|n| n.len() == 32)
        .context("配对应答格式无效")?;
    write_frame(
        &mut writer,
        &Envelope::new(
            dtypes::PAIR_REVEAL,
            DirectPairNonce {
                nonce: farwing_protocol::crypto::b64(&nonce),
            },
        ),
    )
    .await?;
    shown(farwing_protocol::crypto::pairing_code(&ekm, &nonce, &host_nonce));
    // Until this side agrees the host only ever refuses; one read serves
    // both phases, so nothing it sent is lost.
    let mut answer = std::pin::pin!(read(&mut reader, DECISION_TIMEOUT));
    tokio::select! {
        agreed = agreed => agreed.map_err(|_| anyhow::anyhow!("配对已取消"))?,
        message = &mut answer => {
            message?;
            bail!("被控端没有按协议配对");
        }
    }
    write_frame(
        &mut writer,
        &Envelope::new(dtypes::PAIR_CONFIRM, serde_json::Value::Object(Default::default())),
    )
    .await?;
    let message = answer.await?;
    ensure!(message.kind == dtypes::PAIRED, "被控端没有完成配对");
    let _: DirectPaired = message.body().context("配对应答格式无效")?;
    store::remember_host(KnownHost {
        device_id: host.device_id.clone(),
        public_key: host.public_key.clone(),
        name: host.name.clone(),
        address: target.to_string(),
        connect_id: String::new(),
        trusted: true,
        last_seen: 0,
    });
    tracing::info!(device = %host.device_id, "paired with a host");
    Ok(host)
}
