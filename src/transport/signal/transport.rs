//! Realtime socket lifecycle, independent of session negotiation and media.
//!
//! The worker owns the WebSocket: ticket/resume authentication, heartbeat,
//! resume-token refresh, request bookkeeping and reconnection. A physical
//! reconnect resumes the same server-side presence or session and never
//! replays queued messages from the previous socket.

use super::{SignalPushHandler, SignalRole};
use crate::account::api::RealtimeTicket;
use crate::account::server::ServerConfig;
use anyhow::{Context, Result, anyhow};
use farwing_protocol::realtime::{self, Envelope, Hello, ResumeRefreshAck, close, types};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::collections::{BTreeSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpStream, lookup_host};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue, Request};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, client_async_with_config};

const IO_STAGE_TIMEOUT: Duration = Duration::from_secs(5);
const HELLO_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_RETRY_DELAY_MS: f64 = 25_000.0;
const RESUME_CHECK_INTERVAL: Duration =
    Duration::from_secs(farwing_protocol::timing::RESUME_CHECK_INTERVAL_S);
const RESUME_MAX_AGE: Duration = Duration::from_secs(farwing_protocol::timing::RESUME_MAX_AGE_S);
type Socket = WebSocketStream<tokio_rustls::client::TlsStream<TcpStream>>;
type Reader = SplitStream<Socket>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketState {
    Connecting,
    Connected,
    Reconnecting,
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) enum SignalFailure {
    /// The realtime socket could not be (re)established.
    Socket(String),
    /// The server closed the connection for good.
    Closed(String),
    /// The server rejected the ticket, resume token or protocol version.
    Rejected(String),
    /// The session or presence was ended by the other party, a revoked
    /// permission or credential; carries the server's reason when known.
    Kicked(String),
    /// The host went offline while this controller session existed.
    PublisherDisconnected,
    /// The media peer failed.
    Peer(String),
}

impl std::fmt::Display for SignalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = &crate::i18n::t().session;
        match self {
            Self::Socket(reason) => f.write_str(&(text.signal_connection_lost)(reason)),
            Self::Closed(reason) => f.write_str(&(text.signal_server_closed)(reason)),
            Self::Rejected(reason) => f.write_str(&(text.signal_server_rejected)(reason)),
            Self::Kicked(reason) => f.write_str(match reason.as_str() {
                realtime::reason::PERMISSION_REVOKED => text.ended_permission_revoked,
                realtime::reason::KICKED => text.ended_kicked,
                realtime::reason::REPLACED => text.ended_replaced,
                realtime::reason::IDENTITY_MISMATCH => text.ended_identity_mismatch,
                realtime::reason::REVOKED => text.ended_access_turned_off,
                realtime::reason::HOST_CLOSED => text.ended_by_host,
                _ => text.ended_session,
            }),
            Self::PublisherDisconnected => f.write_str(text.host_went_offline),
            Self::Peer(reason) => f.write_str(&(text.media_connection_failed)(reason)),
        }
    }
}
impl std::error::Error for SignalFailure {}

/// What the worker delivers to the session owner.
#[derive(Clone)]
pub(super) enum Incoming {
    /// A (re)connection completed with the server's `hello`.
    Opened(Box<Hello>),
    Message(Envelope),
}

enum Command {
    Send(Envelope),
    SendConnected(Envelope, oneshot::Sender<Result<()>>),
    Close,
}

pub(super) struct SignalTransport {
    commands: mpsc::UnboundedSender<Command>,
    packets: mpsc::UnboundedReceiver<Result<Incoming>>,
    state: watch::Receiver<SocketState>,
    task: Option<JoinHandle<()>>,
    /// A presence connection: the controller sessions it carries.
    carried: Option<Arc<Carried>>,
}

/// Controller sessions carried by a presence connection
/// (`docs/protocol/v2-design.md` §11). The socket worker hands their
/// envelopes, and the replies to their requests, to them instead of to the
/// host session.
#[derive(Default)]
pub(super) struct Carried {
    sessions: std::sync::Mutex<std::collections::HashMap<String, mpsc::UnboundedSender<Result<Incoming>>>>,
    /// Request ID → carried session that sent it.
    requests: std::sync::Mutex<std::collections::HashMap<String, String>>,
}

impl Carried {
    fn sessions(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, mpsc::UnboundedSender<Result<Incoming>>>>
    {
        self.sessions.lock().unwrap_or_else(|e| e.into_inner())
    }
    fn requests(&self) -> std::sync::MutexGuard<'_, std::collections::HashMap<String, String>> {
        self.requests.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Hand `incoming` to the carried session it belongs to; give it back
    /// when it belongs to the host session.
    fn route(&self, incoming: Incoming) -> std::result::Result<(), Incoming> {
        let Incoming::Message(envelope) = &incoming else {
            return Err(incoming);
        };
        let sid = match envelope.sid.clone() {
            Some(sid) if self.sessions().contains_key(&sid) => sid,
            _ => match envelope
                .re
                .as_ref()
                .filter(|_| matches!(envelope.kind.as_str(), types::ACK | types::ERROR))
                .and_then(|re| self.requests().remove(re))
            {
                Some(sid) => sid,
                None => return Err(incoming),
            },
        };
        match self.sessions().get(&sid) {
            Some(route) => {
                let _ = route.send(Ok(incoming));
                Ok(())
            }
            // Ended meanwhile: late frames are dropped.
            None => Ok(()),
        }
    }

    /// A (re)connection of the presence socket.
    fn opened(&self, hello: &Hello) {
        for route in self.sessions().values() {
            let _ = route.send(Ok(Incoming::Opened(Box::new(hello.clone()))));
        }
    }

    /// The presence connection is gone for good.
    fn close_all(&self, why: &str) {
        for (_, route) in self.sessions().drain() {
            let _ = route.send(Err(SignalFailure::Socket(why.to_owned()).into()));
        }
        self.requests().clear();
    }
}

/// What a controller session needs to ride on a presence connection.
#[derive(Clone)]
pub(super) struct Carrier {
    commands: mpsc::UnboundedSender<Command>,
    state: watch::Receiver<SocketState>,
    carried: Arc<Carried>,
}

impl Carrier {
    pub(super) fn is_connected(&self) -> bool {
        *self.state.borrow() == SocketState::Connected && !self.commands.is_closed()
    }
}

impl SignalTransport {
    pub(super) fn spawn(
        server: ServerConfig,
        ticket: RealtimeTicket,
        role: SignalRole,
        push: Option<SignalPushHandler>,
    ) -> Self {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (packets_tx, packets) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(SocketState::Connecting);
        let carried = (role == SignalRole::Host).then(|| Arc::new(Carried::default()));
        let worker_carried = carried.clone();
        let task = tokio::spawn(async move {
            let mut worker = Worker::new(server, ticket, role, commands_rx, packets_tx, state_tx);
            worker.push = push;
            worker.carried = worker_carried;
            let result = worker.run().await;
            if let Some(carried) = &worker.carried {
                carried.close_all(&match &result {
                    Err(error) => (crate::i18n::t().session.warning_presence_lost)(&format!("{error:#}")),
                    Ok(()) => crate::i18n::t().session.online_connection_closed.to_owned(),
                });
            }
            if let Err(error) = result {
                let _ = worker.packets.send(Err(error));
            }
            worker.state.send_replace(SocketState::Closed);
        });
        Self {
            commands,
            packets,
            state,
            task: Some(task),
            carried,
        }
    }

    /// This presence connection, for controller sessions to ride on.
    pub(super) fn carrier(&self) -> Option<Carrier> {
        Some(Carrier {
            commands: self.commands.clone(),
            state: self.state.clone(),
            carried: Arc::clone(self.carried.as_ref()?),
        })
    }

    /// A controller session `sid` carried by a presence connection: its
    /// frames go out on that socket (requests are routed back by ID) and the
    /// server's frames for `sid` come back here. Closing it leaves the
    /// session (`session.leave`); the presence connection stays.
    pub(super) fn carried(carrier: &Carrier, sid: &str) -> Result<Self> {
        anyhow::ensure!(carrier.is_connected(), "{}", crate::i18n::t().session.online_connection_not_ready);
        let (route_tx, mut route) = mpsc::unbounded_channel();
        carrier.carried.sessions().insert(sid.to_owned(), route_tx);
        let (commands, mut commands_rx) = mpsc::unbounded_channel::<Command>();
        let (packets_tx, packets) = mpsc::unbounded_channel();
        let presence = carrier.commands.clone();
        let carried = Arc::clone(&carrier.carried);
        let sid = sid.to_owned();
        let task = tokio::spawn(async move {
            let register = |envelope: &Envelope| {
                if let Some(id) = &envelope.id {
                    carried.requests().insert(id.clone(), sid.clone());
                }
            };
            loop {
                tokio::select! {
                    command = commands_rx.recv() => match command {
                        Some(Command::Send(envelope)) => {
                            register(&envelope);
                            let _ = presence.send(Command::Send(envelope));
                        }
                        Some(Command::SendConnected(envelope, done)) => {
                            register(&envelope);
                            if let Err(mpsc::error::SendError(Command::SendConnected(_, done))) =
                                presence.send(Command::SendConnected(envelope, done))
                            {
                                let _ = done.send(Err(anyhow!("{}", crate::i18n::t().session.online_connection_closed)));
                            }
                        }
                        Some(Command::Close) | None => {
                            let leave = Envelope::new(types::SESSION_LEAVE, serde_json::json!({})).with_sid(&sid);
                            let _ = presence.send(Command::Send(leave));
                            break;
                        }
                    },
                    incoming = route.recv() => match incoming {
                        Some(Ok(Incoming::Message(envelope))) if envelope.kind == types::SESSION_ENDED => {
                            let reason = envelope
                                .data
                                .get("reason")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            tracing::info!(%reason, "server ended the controller session");
                            let failure = if reason == realtime::reason::HOST_OFFLINE {
                                SignalFailure::PublisherDisconnected
                            } else {
                                SignalFailure::Kicked(reason)
                            };
                            let _ = packets_tx.send(Err(failure.into()));
                            break;
                        }
                        Some(incoming) => {
                            let _ = packets_tx.send(incoming);
                        }
                        None => {
                            let _ = packets_tx.send(Err(SignalFailure::Socket(crate::i18n::t().session.online_connection_closed.into()).into()));
                            break;
                        }
                    },
                }
            }
            carried.sessions().remove(&sid);
            carried.requests().retain(|_, owner| *owner != sid);
        });
        Ok(Self {
            commands,
            packets,
            state: carrier.state.clone(),
            task: Some(task),
            carried: None,
        })
    }

    pub(super) fn is_connected(&self) -> bool {
        *self.state.borrow() == SocketState::Connected
    }

    pub(super) fn state_receiver(&self) -> watch::Receiver<SocketState> {
        self.state.clone()
    }

    pub(super) async fn next(&mut self) -> Result<Incoming> {
        self.packets
            .recv()
            .await
            .context("signaling transport task ended")?
    }

    /// Queue a message. Messages queued while disconnected are delivered
    /// after the next successful (re)connection of the same socket owner.
    pub(super) fn send(&self, envelope: Envelope) -> Result<()> {
        self.commands
            .send(Command::Send(envelope))
            .map_err(|_| anyhow!("signaling transport stopped"))
    }

    /// Send only if the socket is connected right now; never queue.
    pub(super) async fn send_connected(&self, envelope: Envelope) -> Result<()> {
        anyhow::ensure!(self.is_connected(), "realtime socket is not connected");
        let (done, result) = oneshot::channel();
        self.commands
            .send(Command::SendConnected(envelope, done))
            .map_err(|_| anyhow!("signaling transport stopped"))?;
        result
            .await
            .context("signaling transport stopped before submission")?
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        let _ = self.commands.send(Command::Close);
        if let Some(task) = self.task.take() {
            task.await.context("join signaling transport")?;
        }
        Ok(())
    }

    /// No server connection: messages without a direct route are dropped and
    /// nothing is ever received (a host serving direct sessions only).
    pub(super) fn offline() -> Self {
        let (commands, mut commands_rx) = mpsc::unbounded_channel();
        let (packets_tx, packets) = mpsc::unbounded_channel();
        let (_, state) = watch::channel(SocketState::Closed);
        let task = tokio::spawn(async move {
            let _packets = packets_tx;
            while let Some(command) = commands_rx.recv().await {
                match command {
                    Command::Send(_) => {}
                    Command::SendConnected(_, done) => {
                        let _ = done.send(Err(anyhow!("{}", crate::i18n::t().session.no_server_connection)));
                    }
                    Command::Close => break,
                }
            }
        });
        Self {
            commands,
            packets,
            state,
            task: Some(task),
            carried: None,
        }
    }

    /// A controller's direct connection to a host
    /// (`docs/protocol/v2-design.md` §12): the same envelopes over the
    /// authenticated TLS stream, with a keep-alive.
    pub(super) fn direct<R, W>(mut reader: R, mut writer: W) -> Self
    where
        R: tokio::io::AsyncRead + Unpin + Send + 'static,
        W: tokio::io::AsyncWrite + Unpin + Send + 'static,
    {
        use farwing_protocol::direct::{IDLE_TIMEOUT_MS, PING_INTERVAL_MS};
        let (commands, mut commands_rx) = mpsc::unbounded_channel::<Command>();
        let (packets_tx, packets) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(SocketState::Connected);
        let task = tokio::spawn(async move {
            // Frame reads are not cancel-safe: one task owns the read half.
            let (frames_tx, mut frames) = mpsc::unbounded_channel();
            let reading = tokio::spawn(async move {
                loop {
                    let frame = crate::transport::direct::read_frame(&mut reader).await;
                    let end = !matches!(frame, Ok(Some(_)));
                    if frames_tx.send(frame).is_err() || end {
                        break;
                    }
                }
            });
            let (out_tx, mut out) = mpsc::unbounded_channel::<Envelope>();
            let writing = tokio::spawn(async move {
                while let Some(envelope) = out.recv().await {
                    crate::transport::direct::write_frame(&mut writer, &envelope).await?;
                }
                use tokio::io::AsyncWriteExt;
                let _ = writer.shutdown().await;
                Ok::<(), anyhow::Error>(())
            });
            let mut ping = tokio::time::interval(Duration::from_millis(PING_INTERVAL_MS));
            ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let idle = Duration::from_millis(IDLE_TIMEOUT_MS);
            let mut idle_deadline = Instant::now() + idle;
            let failure = loop {
                tokio::select! {
                    command = commands_rx.recv() => match command {
                        Some(Command::Send(envelope)) => { let _ = out_tx.send(envelope); }
                        Some(Command::SendConnected(envelope, done)) => {
                            let _ = done.send(out_tx.send(envelope).map_err(|_| anyhow!("{}", crate::i18n::t().session.direct_connection_lost)));
                        }
                        Some(Command::Close) | None => break None,
                    },
                    frame = frames.recv() => match frame {
                        Some(Ok(Some(envelope))) => {
                            idle_deadline = Instant::now() + idle;
                            match envelope.kind.as_str() {
                                types::PING => {
                                    let _ = out_tx.send(Envelope { kind: types::PONG.into(), ..envelope });
                                }
                                types::PONG => {}
                                types::SESSION_ENDED => {
                                    let reason = envelope
                                        .data
                                        .get("reason")
                                        .and_then(serde_json::Value::as_str)
                                        .unwrap_or_default()
                                        .to_owned();
                                    tracing::info!(%reason, "host ended the direct session");
                                    break Some(if reason == realtime::reason::HOST_OFFLINE {
                                        SignalFailure::PublisherDisconnected
                                    } else {
                                        SignalFailure::Kicked(reason)
                                    });
                                }
                                _ => { let _ = packets_tx.send(Ok(Incoming::Message(envelope))); }
                            }
                        }
                        // The host went away (quit, restart, network): wait for it.
                        Some(Ok(None)) | None => break Some(SignalFailure::PublisherDisconnected),
                        Some(Err(error)) => break Some(SignalFailure::Socket(format!("{error:#}"))),
                    },
                    _ = ping.tick() => {
                        let _ = out_tx.send(Envelope::new(types::PING, realtime::Ping { t: chrono::Utc::now().timestamp_millis() }));
                    }
                    _ = tokio::time::sleep_until(idle_deadline) => {
                        break Some(SignalFailure::Socket(crate::i18n::t().session.direct_connection_not_responding.into()));
                    }
                }
            };
            state_tx.send_replace(SocketState::Closed);
            if let Some(failure) = failure {
                let _ = packets_tx.send(Err(failure.into()));
            }
            drop(out_tx);
            let _ = timeout(CLOSE_TIMEOUT, writing).await;
            reading.abort();
        });
        Self {
            commands,
            packets,
            state,
            task: Some(task),
            carried: None,
        }
    }
}

impl Drop for SignalTransport {
    fn drop(&mut self) {
        // A dropped (cancelled) connect future must not abandon its socket.
        let _ = self.commands.send(Command::Close);
    }
}

struct Endpoint {
    url: String,
    uses: u32,
}

struct Worker {
    push: Option<SignalPushHandler>,
    /// Presence connection: controller sessions riding on it.
    carried: Option<Arc<Carried>>,
    server: ServerConfig,
    ticket: RealtimeTicket,
    role: SignalRole,
    endpoints: Vec<Endpoint>,
    commands: mpsc::UnboundedReceiver<Command>,
    packets: mpsc::UnboundedSender<Result<Incoming>>,
    state: watch::Sender<SocketState>,
    resume: Option<String>,
    resume_refreshed: Option<Instant>,
    queued: VecDeque<Envelope>,
    pending: BTreeSet<String>,
    next_id: u64,
    stop: bool,
}

enum SocketEnd {
    Retry(String),
    Terminal(SignalFailure),
    Stopped,
}

impl Worker {
    fn new(
        server: ServerConfig,
        ticket: RealtimeTicket,
        role: SignalRole,
        commands: mpsc::UnboundedReceiver<Command>,
        packets: mpsc::UnboundedSender<Result<Incoming>>,
        state: watch::Sender<SocketState>,
    ) -> Self {
        let endpoints = ticket
            .endpoints()
            .into_iter()
            .map(|url| Endpoint {
                url: url.to_owned(),
                uses: 0,
            })
            .collect();
        Self {
            push: None,
            carried: None,
            server,
            ticket,
            role,
            endpoints,
            commands,
            packets,
            state,
            resume: None,
            resume_refreshed: None,
            queued: VecDeque::new(),
            pending: BTreeSet::new(),
            next_id: 1,
            stop: false,
        }
    }

    fn command(&mut self, command: Option<Command>, socket_open: bool) {
        match command {
            Some(Command::SendConnected(envelope, done)) => {
                // A session-ending notice belongs to the current socket only;
                // it must never be saved and delivered to a later session.
                let result = if socket_open && *self.state.borrow() == SocketState::Connected {
                    self.command(Some(Command::Send(envelope)), true);
                    Ok(())
                } else {
                    Err(anyhow!("realtime socket disconnected before submission"))
                };
                let _ = done.send(result);
            }
            Some(Command::Send(envelope)) => {
                if let Some(id) = &envelope.id {
                    self.pending.insert(id.clone());
                }
                self.queued.push_back(envelope);
            }
            Some(Command::Close) | None => self.stop = true,
        }
    }

    /// A lost socket drops its queued messages. Requests awaiting a reply are
    /// failed so their owners do not wait for an answer that cannot arrive.
    fn clear_disconnected(&mut self) {
        self.queued.clear();
        for id in std::mem::take(&mut self.pending) {
            let error = Envelope::error(
                Some(&id),
                farwing_protocol::ErrorBody::new(
                    farwing_protocol::ErrorCode::Network,
                    "realtime socket disconnected",
                ),
            );
            self.deliver(Incoming::Message(error));
        }
    }

    /// Hand `incoming` to the carried controller session it belongs to, or
    /// to this connection's owner.
    fn deliver(&self, incoming: Incoming) {
        let incoming = match &self.carried {
            Some(carried) => match carried.route(incoming) {
                Ok(()) => return,
                Err(incoming) => incoming,
            },
            None => incoming,
        };
        let _ = self.packets.send(Ok(incoming));
    }

    async fn run(&mut self) -> Result<()> {
        let tls = self.server.tls(false)?;
        for endpoint in &self.endpoints {
            let url = url::Url::parse(&endpoint.url).context(crate::i18n::t().session.realtime_address_invalid)?;
            anyhow::ensure!(
                self.server.accepts_endpoint(&url),
                "{}",
                crate::i18n::t().session.realtime_address_foreign
            );
        }
        let max_retries = self.endpoints.len().saturating_mul(2).max(2) as u32;
        let initial_delay = f64::from(self.ticket.retry_delay_ms);
        let mut attempts = 0_u32;
        let mut retry_reason = None::<String>;
        self.endpoints
            .first_mut()
            .context("no realtime endpoints")?
            .uses += 1;

        loop {
            if let Some(reason) = retry_reason.take() {
                self.clear_disconnected();
                if self.resume.is_none() {
                    // The single-use ticket may already be consumed; only a
                    // resume token can re-enter the same presence/session.
                    return Err(SignalFailure::Socket(format!("{reason}; no resume token")).into());
                }
                if attempts >= max_retries {
                    return Err(SignalFailure::Socket(format!(
                        "{reason}; retry budget {max_retries} exhausted"
                    ))
                    .into());
                }
                self.state.send_replace(SocketState::Reconnecting);
                let delay = (initial_delay * 1.5_f64.powi(attempts.min(32) as i32))
                    .min(MAX_RETRY_DELAY_MS.max(initial_delay)) as u64;
                tracing::warn!(%reason, attempts, max_retries, delay_ms = delay, "realtime socket retry scheduled; media is preserved");
                let wait = crate::session::reconnect::wait(Duration::from_millis(delay));
                tokio::pin!(wait);
                loop {
                    tokio::select! {
                        _ = &mut wait => break,
                        command = self.commands.recv() => self.command(command, false),
                    }
                    if self.stop {
                        return Ok(());
                    }
                }
                attempts += 1;
                self.endpoints.sort_by_key(|endpoint| endpoint.uses);
                if let Some(endpoint) = self.endpoints.iter_mut().find(|endpoint| endpoint.uses < 2)
                {
                    endpoint.uses += 1;
                }
            }
            if self.stop {
                return Ok(());
            }
            let request = realtime_request(
                &self.endpoints[0].url,
                self.resume.as_deref(),
                self.ticket.ticket(),
            )?;
            tracing::debug!(attempts, resume = self.resume.is_some(), role = ?self.role, "starting realtime socket connection");
            let connecting = connect_socket(request, Arc::clone(&tls));
            tokio::pin!(connecting);
            let result = loop {
                tokio::select! {
                    result = &mut connecting => break result,
                    command = self.commands.recv() => self.command(command, true),
                }
                if self.stop {
                    return Ok(());
                }
            };
            let socket = match result {
                Ok(socket) => socket,
                Err(error) => {
                    if let Some(failure) = rejected_handshake(&error) {
                        return Err(failure.into());
                    }
                    retry_reason = Some(format!("{error:#}"));
                    continue;
                }
            };
            attempts = 0;
            match self.connected(socket).await? {
                SocketEnd::Retry(reason) => retry_reason = Some(reason),
                SocketEnd::Terminal(error) => return Err(error.into()),
                SocketEnd::Stopped => return Ok(()),
            }
        }
    }

    async fn connected(&mut self, socket: Socket) -> Result<SocketEnd> {
        let (sink, mut reader) = socket.split();
        let (writer_tx, writer_rx) = mpsc::unbounded_channel();
        let (writer_result_tx, mut writer_result) = oneshot::channel();
        let mut writer = tokio::spawn(async move {
            let _ = writer_result_tx.send(write_messages(sink, writer_rx).await);
        });
        let result = self
            .read_connected(&mut reader, &writer_tx, &mut writer_result)
            .await;
        if matches!(result, Ok(SocketEnd::Terminal(_) | SocketEnd::Stopped)) {
            drop(writer_tx);
            if timeout(CLOSE_TIMEOUT, &mut writer).await.is_ok() {
                return result;
            }
        }
        writer.abort();
        let _ = writer.await;
        result
    }

    fn write(writer: &mpsc::UnboundedSender<Message>, envelope: &Envelope) -> bool {
        writer
            .send(Message::Text(envelope.encode().into()))
            .is_ok()
    }

    async fn read_connected(
        &mut self,
        reader: &mut Reader,
        writer: &mpsc::UnboundedSender<Message>,
        writer_result: &mut oneshot::Receiver<Result<()>>,
    ) -> Result<SocketEnd> {
        let hello_deadline = Instant::now() + HELLO_TIMEOUT;
        let mut opened = false;
        let mut idle = Duration::from_millis(u64::from(
            farwing_protocol::timing::HEARTBEAT_MS + farwing_protocol::timing::IDLE_TIMEOUT_MS,
        ));
        let mut idle_deadline = Instant::now() + idle;
        let mut resume_check = Instant::now() + RESUME_CHECK_INTERVAL;
        let mut refresh_id = None::<String>;
        loop {
            if self.stop {
                self.close_socket(reader, writer, opened).await;
                return Ok(SocketEnd::Stopped);
            }
            if opened {
                while let Some(envelope) = self.queued.pop_front() {
                    if !Self::write(writer, &envelope) {
                        return Ok(SocketEnd::Retry("socket writer stopped".into()));
                    }
                }
            }
            tokio::select! {
                command = self.commands.recv() => self.command(command, true),
                result = &mut *writer_result => {
                    let reason = match result { Ok(Err(error)) => format!("{error:#}"), Err(error) => error.to_string(), Ok(Ok(())) => "socket writer ended".into() };
                    return Ok(SocketEnd::Retry(reason));
                }
                _ = tokio::time::sleep_until(hello_deadline), if !opened => {
                    return Ok(SocketEnd::Retry("server hello timed out".into()));
                }
                _ = tokio::time::sleep_until(idle_deadline) => {
                    self.protocol_close(reader, writer, "heartbeat timeout").await;
                    return Ok(SocketEnd::Retry("realtime heartbeat timed out".into()));
                }
                _ = tokio::time::sleep_until(resume_check), if opened && refresh_id.is_none() => {
                    resume_check = Instant::now() + RESUME_CHECK_INTERVAL;
                    if self.resume_refreshed.is_none_or(|at| at.elapsed() > RESUME_MAX_AGE) {
                        let id = format!("r-{}", self.next_id);
                        self.next_id += 1;
                        let request = Envelope::new(types::RESUME_REFRESH, serde_json::json!({})).with_id(id.clone());
                        if Self::write(writer, &request) {
                            refresh_id = Some(id);
                        }
                    }
                }
                message = reader.next() => {
                    let message = match message {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => return Ok(SocketEnd::Retry(error.to_string())),
                        None => return Ok(SocketEnd::Retry("realtime WebSocket EOF".into())),
                    };
                    idle_deadline = Instant::now() + idle;
                    match message {
                        Message::Close(frame) => return Ok(self.closed(frame)),
                        Message::Ping(payload) => { let _ = writer.send(Message::Pong(payload)); }
                        Message::Pong(_) | Message::Frame(_) | Message::Binary(_) => {}
                        Message::Text(text) => {
                            if text.len() > farwing_protocol::limits::MAX_WS_FRAME {
                                tracing::warn!(bytes = text.len(), "discarded oversized realtime frame");
                                continue;
                            }
                            let envelope = match Envelope::decode(&text) {
                                Ok(envelope) if envelope.v == farwing_protocol::PROTOCOL_VERSION => envelope,
                                Ok(envelope) => {
                                    tracing::warn!(version = envelope.v, "discarded realtime frame with another protocol version");
                                    continue;
                                }
                                Err(error) => { tracing::warn!(%error, "discarded invalid realtime frame"); continue; }
                            };
                            match envelope.kind.as_str() {
                                types::HELLO => {
                                    let hello: Hello = match envelope.body() {
                                        Ok(hello) => hello,
                                        Err(error) => return Ok(SocketEnd::Retry(format!("invalid server hello: {error}"))),
                                    };
                                    if !farwing_protocol::ids::is_token(&hello.resume_token, farwing_protocol::ids::RESUME_TOKEN_PREFIX) {
                                        return Ok(SocketEnd::Retry("server hello omitted its resume token".into()));
                                    }
                                    idle = Duration::from_millis(u64::from(hello.heartbeat_ms.saturating_add(hello.idle_timeout_ms)).max(5_000));
                                    idle_deadline = Instant::now() + idle;
                                    self.resume = Some(hello.resume_token.clone());
                                    self.resume_refreshed = Some(Instant::now());
                                    opened = true;
                                    for endpoint in &mut self.endpoints { endpoint.uses = 0; }
                                    self.state.send_replace(SocketState::Connected);
                                    tracing::info!(resumed = hello.resumed, epoch = hello.epoch, role = ?self.role, "realtime socket connected; existing media session retained");
                                    if let Some(carried) = &self.carried {
                                        carried.opened(&hello);
                                    }
                                    if !hello.probe_nodes.is_empty() {
                                        super::node_probe::spawn(hello.probe_nodes.clone(), writer.clone());
                                    }
                                    let _ = self.packets.send(Ok(Incoming::Opened(Box::new(hello))));
                                }
                                types::PING => {
                                    let pong = Envelope { kind: types::PONG.into(), ..envelope };
                                    let _ = Self::write(writer, &pong);
                                }
                                types::ACK | types::ERROR if envelope.re.is_some() && envelope.re == refresh_id => {
                                    refresh_id = None;
                                    resume_check = Instant::now() + RESUME_CHECK_INTERVAL;
                                    match envelope.body::<ResumeRefreshAck>() {
                                        Ok(ack) if envelope.kind == types::ACK && farwing_protocol::ids::is_token(&ack.resume_token, farwing_protocol::ids::RESUME_TOKEN_PREFIX) => {
                                            self.resume = Some(ack.resume_token);
                                            self.resume_refreshed = Some(Instant::now());
                                            tracing::debug!("realtime resume token refreshed");
                                        }
                                        _ => tracing::warn!("resume.refresh was rejected; keeping the current socket"),
                                    }
                                }
                                types::ACK | types::ERROR => {
                                    if let Some(re) = &envelope.re { self.pending.remove(re); }
                                    self.deliver(Incoming::Message(envelope));
                                }
                                _ => {
                                    if let Some(failure) = self.dispatch(envelope) {
                                        return Ok(SocketEnd::Terminal(failure));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn dispatch(&self, envelope: Envelope) -> Option<SignalFailure> {
        // Payloads can contain credentials and SDP; record metadata only.
        tracing::trace!(kind = %envelope.kind, has_sid = envelope.sid.is_some(), "received realtime message");
        if self.role == SignalRole::Host && is_push(&envelope.kind) {
            if let Some(observer) = &self.push {
                observer(&envelope);
            }
            return None;
        }
        if self.role == SignalRole::Controller && envelope.kind == types::SESSION_ENDED {
            let reason = envelope
                .data
                .get("reason")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default()
                .to_owned();
            tracing::info!(%reason, "server ended the controller session");
            return Some(if reason == realtime::reason::HOST_OFFLINE {
                SignalFailure::PublisherDisconnected
            } else {
                SignalFailure::Kicked(reason)
            });
        }
        self.deliver(Incoming::Message(envelope));
        None
    }

    fn closed(&self, frame: Option<CloseFrame>) -> SocketEnd {
        let code = frame
            .as_ref()
            .map_or(1005, |frame| u16::from(frame.code));
        let reason = frame
            .as_ref()
            .map(|frame| frame.reason.to_string())
            .unwrap_or_default();
        if close::reconnectable(code) {
            return SocketEnd::Retry(format!("WebSocket closed ({code} {reason})"));
        }
        SocketEnd::Terminal(match code {
            close::REVOKED | close::SUPERSEDED | close::SESSION_ENDED => {
                SignalFailure::Kicked(String::new())
            }
            close::INVALID_TICKET | close::VERSION_UNSUPPORTED => {
                SignalFailure::Rejected(format!("{code} {reason}"))
            }
            _ => SignalFailure::Closed(format!("{code} {reason}")),
        })
    }

    async fn protocol_close(
        &mut self,
        reader: &mut Reader,
        writer: &mpsc::UnboundedSender<Message>,
        reason: &'static str,
    ) {
        let _ = writer.send(Message::Close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: reason.into(),
        })));
        let _ = timeout(IO_STAGE_TIMEOUT, async {
            loop {
                tokio::select! {
                    message = reader.next() => if matches!(message, Some(Ok(Message::Close(_))) | Some(Err(_)) | None) { break; },
                    command = self.commands.recv() => self.command(command, false),
                }
                if self.stop {
                    break;
                }
            }
        })
        .await;
    }

    async fn close_socket(
        &self,
        reader: &mut Reader,
        writer: &mpsc::UnboundedSender<Message>,
        opened: bool,
    ) {
        if opened {
            let _ = Self::write(writer, &Envelope::new(types::BYE, serde_json::json!({})));
        }
        let _ = writer.send(Message::Close(Some(CloseFrame {
            code: CloseCode::Normal,
            reason: "".into(),
        })));
        let _ = timeout(CLOSE_TIMEOUT, async {
            while let Some(message) = reader.next().await {
                if matches!(message, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
        })
        .await;
    }
}

fn is_push(kind: &str) -> bool {
    matches!(
        kind,
        types::DEVICE_CHANGED
            | types::FRIEND_CHANGED
            | types::ASSIST_JOIN
            | types::ASSIST_ACCEPT
            | types::ASSIST_CANCELLED
            | types::POWER_REQUEST
            | types::WOL_WAKE
    )
}

fn rejected_handshake(error: &anyhow::Error) -> Option<SignalFailure> {
    use tokio_tungstenite::tungstenite::Error;
    error.chain().find_map(|cause| match cause.downcast_ref::<Error>() {
        Some(Error::Http(response))
            if matches!(response.status().as_u16(), 401 | 403 | 426) =>
        {
            Some(SignalFailure::Rejected(format!(
                "HTTP {}",
                response.status().as_u16()
            )))
        }
        _ => None,
    })
}

async fn write_messages(
    mut sink: SplitSink<Socket, Message>,
    mut messages: mpsc::UnboundedReceiver<Message>,
) -> Result<()> {
    while let Some(message) = messages.recv().await {
        sink.feed(message).await.context("queue realtime frame")?;
        while let Ok(message) = messages.try_recv() {
            sink.feed(message).await.context("queue realtime frame")?;
        }
        sink.flush().await.context("flush realtime frames")?;
    }
    Ok(())
}

fn realtime_request(endpoint: &str, resume: Option<&str>, ticket: &str) -> Result<Request<()>> {
    let mut request = endpoint
        .into_client_request()
        .map_err(|_| anyhow!("failed to build realtime WebSocket request"))?;
    let headers = request.headers_mut();
    headers.insert(
        HeaderName::from_static(farwing_protocol::headers::PROTOCOL),
        HeaderValue::from(farwing_protocol::PROTOCOL_VERSION),
    );
    let (name, value) = match resume {
        Some(token) => (farwing_protocol::headers::RESUME, token),
        None => (farwing_protocol::headers::TICKET, ticket),
    };
    let mut value = HeaderValue::from_str(value).context("realtime credential is not a valid header")?;
    value.set_sensitive(true);
    headers.insert(HeaderName::from_static(name), value);
    Ok(request)
}

async fn connect_socket(request: Request<()>, tls: Arc<rustls::ClientConfig>) -> Result<Socket> {
    let host = request
        .uri()
        .host()
        .context("realtime URL has no host")?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = request.uri().port_u16().unwrap_or(443);
    let addresses = timeout(IO_STAGE_TIMEOUT, lookup_host((host.as_str(), port)))
        .await
        .context("realtime DNS timed out")??
        .collect::<Vec<_>>();
    let tcp = timeout(IO_STAGE_TIMEOUT, crate::transport::happy_eyeballs::connect(&addresses))
        .await
        .context("realtime TCP connect timed out")??;
    let _ = tcp.set_nodelay(true);
    let name =
        rustls::pki_types::ServerName::try_from(host).context("invalid realtime TLS server name")?;
    let connector = tokio_rustls::TlsConnector::from(tls);
    let tls = timeout(IO_STAGE_TIMEOUT, connector.connect(name, tcp))
        .await
        .context("realtime TLS handshake timed out")??;
    let config = tokio_tungstenite::tungstenite::protocol::WebSocketConfig::default()
        .max_message_size(Some(farwing_protocol::limits::MAX_WS_FRAME * 2))
        .max_frame_size(Some(farwing_protocol::limits::MAX_WS_FRAME * 2));
    let (socket, _) = timeout(
        IO_STAGE_TIMEOUT,
        client_async_with_config(request, tls, Some(config)),
    )
    .await
    .context("realtime WebSocket handshake timed out")??;
    Ok(socket)
}

#[cfg(test)]
mod carried_tests {
    use super::*;

    fn message(kind: &str, sid: Option<&str>, re: Option<&str>) -> Incoming {
        let mut envelope = Envelope::new(kind, serde_json::json!({}));
        envelope.sid = sid.map(str::to_owned);
        envelope.re = re.map(str::to_owned);
        Incoming::Message(envelope)
    }

    fn kind(incoming: Result<Incoming>) -> String {
        match incoming.unwrap() {
            Incoming::Message(envelope) => envelope.kind,
            Incoming::Opened(_) => "opened".into(),
        }
    }

    #[test]
    fn routes_carried_sessions_and_their_replies() {
        let carried = Carried::default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        carried.sessions().insert("s1".into(), tx);
        carried.requests().insert("p1-1".into(), "s1".into());
        // The carried session's frames and the reply to its request.
        assert!(carried.route(message(types::RTC_ANSWER, Some("s1"), None)).is_ok());
        assert_eq!(kind(rx.try_recv().unwrap()), types::RTC_ANSWER);
        assert!(carried.route(message(types::ACK, None, Some("p1-1"))).is_ok());
        assert_eq!(kind(rx.try_recv().unwrap()), types::ACK);
        assert!(carried.requests().is_empty());
        // Everything else belongs to the host session.
        assert!(carried.route(message(types::SESSION_INCOMING, Some("s2"), None)).is_err());
        assert!(carried.route(message(types::ACK, None, Some("c-1"))).is_err());
        assert!(carried.route(message(types::PING, None, None)).is_err());
        // Losing the connection fails the carried sessions.
        carried.close_all("gone");
        assert!(rx.try_recv().unwrap().is_err());
        assert!(carried.sessions().is_empty());
    }
}
