//! UU's socket lifecycle, independent of room login and media ownership.
//!
//! Evidence: streamer C04840/C05140/C17060 (retry), B5C2F0 (endpoints),
//! C05BB0/B60FB0 (distinct reset points), C05600/C171B0 (heartbeat),
//! BC3650 (namespace deadline). A physical reconnect never repeats room_info.

use super::negotiation::signaling_request;
use super::wire::{EnginePacket, SocketPacket, decode, encode_event, resolve_binary_event};
use super::{PendingBinaryEvent, SignalPushHandler, SignalRole};
use crate::account::api::RoomSession;
use anyhow::{Context, Result, anyhow, bail};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::{
    collections::{BTreeSet, VecDeque},
    pin::Pin,
    task::{Context as TaskContext, Poll},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpStream, lookup_host},
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::http::Request;
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;
use tokio_tungstenite::{WebSocketStream, client_async};

const IO_STAGE_TIMEOUT: Duration = Duration::from_secs(5);
const NAMESPACE_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(3);
type Socket = WebSocketStream<SignalStream>;
type Reader = SplitStream<Socket>;

enum SignalStream {
    Plain(TcpStream),
    Tls(tokio_rustls::client::TlsStream<TcpStream>),
}

impl AsyncRead for SignalStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_read(cx, buffer),
            Self::Tls(stream) => Pin::new(stream).poll_read(cx, buffer),
        }
    }
}

impl AsyncWrite for SignalStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_write(cx, buffer),
            Self::Tls(stream) => Pin::new(stream).poll_write(cx, buffer),
        }
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_flush(cx),
            Self::Tls(stream) => Pin::new(stream).poll_flush(cx),
        }
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        match self.get_mut() {
            Self::Plain(stream) => Pin::new(stream).poll_shutdown(cx),
            Self::Tls(stream) => Pin::new(stream).poll_shutdown(cx),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SocketState {
    Connecting,
    Connected,
    Reconnecting,
    Closed,
}

#[derive(Clone, Debug)]
pub(crate) enum SignalFailure {
    Socket(String),
    NamespaceClosed,
    Rejected(String),
    Kicked,
    PublisherDisconnected,
    Peer(String),
}

impl std::fmt::Display for SignalFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Socket(reason) => write!(f, "signaling transport stopped: {reason} (2003)"),
            Self::NamespaceClosed => f.write_str("signaling namespace closed (2003)"),
            Self::Rejected(reason) => write!(f, "signaling session rejected: {reason} (2003)"),
            Self::Kicked => f.write_str("signaling room was released (2005)"),
            Self::PublisherDisconnected => f.write_str("signaling publisher disconnected (2006)"),
            Self::Peer(reason) => write!(f, "media peer failed: {reason}"),
        }
    }
}
impl std::error::Error for SignalFailure {}

enum Command {
    Send(Vec<Message>),
    SendConnected(Vec<Message>, tokio::sync::oneshot::Sender<Result<()>>),
    Controlling(bool),
    ReconnectKey(Option<String>),
    Close,
}

pub(super) struct SignalTransport {
    commands: mpsc::UnboundedSender<Command>,
    packets: mpsc::UnboundedReceiver<Result<EnginePacket>>,
    state: watch::Receiver<SocketState>,
    task: Option<JoinHandle<()>>,
}

impl SignalTransport {
    pub(super) fn spawn(
        room: RoomSession,
        role: SignalRole,
        push: Option<SignalPushHandler>,
    ) -> Self {
        let (commands, commands_rx) = mpsc::unbounded_channel();
        let (packets_tx, packets) = mpsc::unbounded_channel();
        let (state_tx, state) = watch::channel(SocketState::Connecting);
        let task = tokio::spawn(async move {
            let mut worker = Worker::new(room, role, commands_rx, packets_tx, state_tx);
            worker.push = push;
            if let Err(error) = worker.run().await {
                let _ = worker.packets.send(Err(error));
            }
            worker.state.send_replace(SocketState::Closed);
        });
        Self {
            commands,
            packets,
            state,
            task: Some(task),
        }
    }

    pub(super) fn is_connected(&self) -> bool {
        *self.state.borrow() == SocketState::Connected
    }

    pub(super) fn state_receiver(&self) -> watch::Receiver<SocketState> {
        self.state.clone()
    }

    pub(super) async fn next_packet(&mut self) -> Result<EnginePacket> {
        self.packets
            .recv()
            .await
            .context("signaling transport task ended")?
    }

    pub(super) fn send(&self, messages: Vec<Message>) -> Result<()> {
        self.commands
            .send(Command::Send(messages))
            .map_err(|_| anyhow!("signaling transport stopped"))
    }

    pub(super) async fn send_connected(&self, messages: Vec<Message>) -> Result<()> {
        anyhow::ensure!(self.is_connected(), "signaling namespace is not connected");
        let (done, result) = tokio::sync::oneshot::channel();
        self.commands
            .send(Command::SendConnected(messages, done))
            .map_err(|_| anyhow!("signaling transport stopped"))?;
        result
            .await
            .context("signaling transport stopped before submission")?
    }

    pub(super) fn set_controlling(&self, value: bool) {
        let _ = self.commands.send(Command::Controlling(value));
    }

    pub(super) fn set_reconnect_key(&self, key: Option<String>) {
        let _ = self.commands.send(Command::ReconnectKey(key));
    }

    pub(super) async fn close(&mut self) -> Result<()> {
        let _ = self.commands.send(Command::Close);
        if let Some(task) = self.task.take() {
            task.await.context("join signaling transport")?;
        }
        Ok(())
    }
}

impl Drop for SignalTransport {
    fn drop(&mut self) {
        // Dropping a cancelled room-login future must not silently abandon its
        // socket. The worker owns and closes both halves, even without a waiter.
        let _ = self.commands.send(Command::Close);
    }
}

struct Endpoint {
    url: String,
    uses: u32,
}

struct Worker {
    push: Option<SignalPushHandler>,
    room: RoomSession,
    role: SignalRole,
    endpoints: Vec<Endpoint>,
    commands: mpsc::UnboundedReceiver<Command>,
    packets: mpsc::UnboundedSender<Result<EnginePacket>>,
    state: watch::Sender<SocketState>,
    controlling: bool,
    reconnect_key: Option<String>,
    queued: VecDeque<Vec<Message>>,
    pending_acks: BTreeSet<u64>,
    stop: bool,
}

enum SocketEnd {
    Retry(String),
    Terminal(SignalFailure),
    Stopped,
}

impl Worker {
    fn new(
        room: RoomSession,
        role: SignalRole,
        commands: mpsc::UnboundedReceiver<Command>,
        packets: mpsc::UnboundedSender<Result<EnginePacket>>,
        state: watch::Sender<SocketState>,
    ) -> Self {
        let endpoints = room
            .signaling_endpoints()
            .into_iter()
            .map(|url| Endpoint {
                url: url.to_owned(),
                uses: 0,
            })
            .collect();
        Self {
            push: None,
            room,
            role,
            endpoints,
            commands,
            packets,
            state,
            controlling: false,
            reconnect_key: None,
            queued: VecDeque::new(),
            pending_acks: BTreeSet::new(),
            stop: false,
        }
    }

    fn command(&mut self, command: Option<Command>, socket_open: bool) {
        match command {
            Some(Command::SendConnected(messages, done)) => {
                if done.is_closed() {
                    return;
                }
                // A room-wide stop must be admitted by the current namespace,
                // never saved during reconnect for a later control session.
                // Acknowledge admission before the host processes a new peer.
                let result = if socket_open && *self.state.borrow() == SocketState::Connected {
                    self.command(Some(Command::Send(messages)), true);
                    Ok(())
                } else {
                    Err(anyhow!(
                        "signaling namespace disconnected before submission"
                    ))
                };
                let _ = done.send(result);
            }
            Some(Command::Send(messages)) => {
                if let Some(Message::Text(text)) = messages.first()
                    && let Ok(EnginePacket::Message(
                        SocketPacket::Event { id: Some(id), .. }
                        | SocketPacket::BinaryEvent { id: Some(id), .. },
                    )) = decode(text.as_str())
                {
                    self.pending_acks.insert(id);
                }
                self.queued.push_back(messages);
            }
            Some(Command::Controlling(value)) if socket_open => self.controlling = value,
            Some(Command::ReconnectKey(key)) if socket_open => self.reconnect_key = key,
            Some(Command::Controlling(_) | Command::ReconnectKey(_)) => {
                // BC3F40 rejects header updates while the socket is closed.
                tracing::debug!("ignored signaling header update while socket is closed");
            }
            Some(Command::Close) | None => self.stop = true,
        }
    }

    fn clear_disconnected_packets(&mut self) {
        self.queued.clear();
        for id in std::mem::take(&mut self.pending_acks) {
            let _ = self
                .packets
                .send(Ok(EnginePacket::Message(SocketPacket::Ack {
                    namespace: "/".into(),
                    id,
                    args: Vec::new(),
                })));
        }
    }

    async fn run(&mut self) -> Result<()> {
        crate::transport::init_tls();
        let endpoint_roster = self
            .endpoints
            .iter()
            .map(|endpoint| endpoint.url.clone())
            .collect::<Vec<_>>();
        let tls_roster = endpoint_roster
            .iter()
            .filter(|endpoint| endpoint_uses_tls(endpoint))
            .cloned()
            .collect::<Vec<_>>();
        let tls = if tls_roster.is_empty() {
            None
        } else {
            Some(
                tokio::task::spawn_blocking(move || super::tls::client_config(&tls_roster))
                    .await
                    .context("load signaling TLS roots")??,
            )
        };
        let max_retries = self.endpoints.len().saturating_mul(2) as u32;
        let initial_delay = self.room.streamer_retry_delta_ms.max(0) as u32;
        let max_delay = initial_delay.max(25_000);
        let mut attempts = 0_u32;
        let mut retry_reason = None::<String>;
        self.endpoints
            .first_mut()
            .context("no signaling endpoints")?
            .uses += 1;

        loop {
            if let Some(reason) = retry_reason.take() {
                self.clear_disconnected_packets();
                if attempts >= max_retries {
                    return Err(SignalFailure::Socket(format!(
                        "{reason}; retry budget {max_retries} exhausted"
                    ))
                    .into());
                }
                self.state.send_replace(SocketState::Reconnecting);
                let delay = ((initial_delay as f64 * 1.5_f64.powi(attempts.min(32) as i32))
                    .min(max_delay as f64)) as u64;
                tracing::warn!(%reason, attempts, max_retries, delay_ms = delay, "signaling socket retry scheduled; media is preserved");
                let wait = tokio::time::sleep(Duration::from_millis(delay));
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
            let endpoint = &self.endpoints[0].url;
            let request = signaling_request(
                endpoint,
                self.room.token(),
                self.controlling,
                self.reconnect_key.as_deref(),
            )?;
            tracing::debug!(%endpoint, attempts, controlling = self.controlling, "starting signaling socket connection");
            let connecting = connect_socket(request, tls.clone());
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
                    retry_reason = Some(format!("{error:#}"));
                    continue;
                }
            };
            // C05BB0: the transport retry count resets on WS open, not on a
            // later namespace ACK (whose endpoint counters are separate).
            attempts = 0;
            match self.connected(socket).await? {
                SocketEnd::Retry(reason) if self.room.is_local() => {
                    return Err(SignalFailure::Socket(reason).into());
                }
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
            let _ = writer_result_tx.send(write_packets(sink, writer_rx).await);
        });
        let result = self
            .read_connected(&mut reader, &writer_tx, &mut writer_result)
            .await;
        if matches!(result, Ok(SocketEnd::Terminal(_) | SocketEnd::Stopped)) {
            // A leave event can itself request an ACK. Do not discard that
            // queued response by aborting the writer in the same poll.
            drop(writer_tx);
            if timeout(CLOSE_TIMEOUT, &mut writer).await.is_ok() {
                return result;
            }
        }
        // No writer, queue, TLS stream, or pending write crosses a physical
        // socket generation. Join the aborted task before the next attempt.
        writer.abort();
        let _ = writer.await;
        result
    }

    async fn read_connected(
        &mut self,
        reader: &mut Reader,
        writer: &mpsc::UnboundedSender<Vec<Message>>,
        writer_result: &mut oneshot::Receiver<Result<()>>,
    ) -> Result<SocketEnd> {
        writer
            .send(vec![Message::Text("40".into())])
            .context("start signaling namespace")?;
        let namespace_deadline = Instant::now() + NAMESPACE_TIMEOUT;
        let mut namespace_connected = false;
        let mut heartbeat = None::<(Duration, Instant)>;
        let mut binary = None::<PendingBinaryEvent>;
        loop {
            if self.stop {
                self.close_socket(reader, writer, namespace_connected).await;
                return Ok(SocketEnd::Stopped);
            }
            if namespace_connected {
                while let Some(messages) = self.queued.pop_front() {
                    if writer.send(messages).is_err() {
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
                _ = tokio::time::sleep_until(namespace_deadline), if !namespace_connected => {
                    return Ok(SocketEnd::Terminal(SignalFailure::NamespaceClosed));
                }
                _ = async {
                    if let Some((_, deadline)) = heartbeat { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                } => {
                    self.protocol_close(reader, writer, "Ping timeout").await;
                    return Ok(SocketEnd::Retry("Engine.IO ping timeout (1008)".into()));
                }
                message = reader.next() => {
                    let message = match message {
                        Some(Ok(message)) => message,
                        Some(Err(error)) => return Ok(SocketEnd::Retry(error.to_string())),
                        None => return Ok(SocketEnd::Retry("signaling WebSocket EOF".into())),
                    };
                    match message {
                        Message::Close(frame) => {
                            let normal = frame.as_ref().is_some_and(|frame| frame.code == CloseCode::Normal);
                            let reason = format!("WebSocket closed: {frame:?}");
                            return Ok(if normal { SocketEnd::Terminal(SignalFailure::Socket(reason)) } else { SocketEnd::Retry(reason) });
                        }
                        Message::Ping(payload) => { let _ = writer.send(vec![Message::Pong(payload)]); }
                        Message::Pong(_) | Message::Frame(_) => {}
                        Message::Text(text) => {
                            let packet = match decode(&text) {
                                Ok(packet) => packet,
                                Err(error) => { tracing::warn!(%error, "discarded invalid signaling frame"); continue; }
                            };
                            match packet {
                                EnginePacket::Open(ref open) => {
                                    if open.get("sid").and_then(serde_json::Value::as_str).is_none() {
                                        self.protocol_close(reader, writer, "Handshake error").await;
                                        return Ok(SocketEnd::Retry("invalid Engine.IO open (1008)".into()));
                                    }
                                    let interval = open.get("pingInterval").and_then(serde_json::Value::as_i64).map_or(25_000, |n| n as u32);
                                    let timeout = open.get("pingTimeout").and_then(serde_json::Value::as_i64).map_or(60_000, |n| n as u32);
                                    let duration = Duration::from_millis(interval.wrapping_add(timeout) as u64);
                                    heartbeat = Some((duration, Instant::now() + duration));
                                    tracing::debug!(interval_ms = interval, timeout_ms = timeout, "Engine.IO heartbeat installed");
                                }
                                EnginePacket::Ping(_) => {
                                    let _ = writer.send(vec![Message::Text("3".into())]);
                                    if let Some((duration, deadline)) = heartbeat.as_mut() { *deadline = Instant::now() + *duration; }
                                }
                                EnginePacket::Close => return Ok(SocketEnd::Retry("Engine.IO ended by server (1006)".into())),
                                EnginePacket::Message(SocketPacket::Connect { ref namespace, .. }) if namespace == "/" => {
                                    namespace_connected = true;
                                    for endpoint in &mut self.endpoints { endpoint.uses = 0; }
                                    self.state.send_replace(SocketState::Connected);
                                    tracing::info!("signaling namespace connected; existing media session retained");
                                    let _ = self.packets.send(Ok(packet));
                                }
                                EnginePacket::Message(SocketPacket::Disconnect { ref namespace }) if namespace == "/" => return Ok(SocketEnd::Terminal(SignalFailure::NamespaceClosed)),
                                EnginePacket::Message(SocketPacket::ConnectError { data, .. }) => return Ok(SocketEnd::Terminal(SignalFailure::Rejected(data.to_string()))),
                                EnginePacket::Message(SocketPacket::BinaryEvent { namespace, id, event, args, attachments }) => {
                                    if attachments == 0 {
                                        let packet = resolve_binary_event(namespace, id, event, args, &[])?;
                                        if let Some(failure) = self.dispatch_event(packet, writer) { return Ok(SocketEnd::Terminal(failure)); }
                                    } else {
                                        binary = Some(PendingBinaryEvent { namespace, id, event, args, expected_attachments: attachments, attachments: Vec::new() });
                                    }
                                }
                                EnginePacket::Message(SocketPacket::Ack { id, .. }) => {
                                    tracing::trace!(id, "received signaling ACK");
                                    self.pending_acks.remove(&id);
                                    let _ = self.packets.send(Ok(packet));
                                }
                                EnginePacket::Message(SocketPacket::Event { .. }) => {
                                    if let Some(failure) = self.dispatch_event(packet, writer) { return Ok(SocketEnd::Terminal(failure)); }
                                }
                                _ => { let _ = self.packets.send(Ok(packet)); }
                            }
                        }
                        Message::Binary(bytes) => {
                            if let Some(pending) = binary.as_mut() {
                                pending.attachments.push(bytes.to_vec());
                                if pending.attachments.len() == pending.expected_attachments {
                                    let pending = binary.take().expect("completed binary event");
                                    match resolve_binary_event(pending.namespace, pending.id, pending.event, pending.args, &pending.attachments) {
                                        Ok(packet) => {
                                            if let Some(failure) = self.dispatch_event(packet, writer) { return Ok(SocketEnd::Terminal(failure)); }
                                        }
                                        Err(error) => tracing::warn!(%error, "discarded invalid signaling binary event"),
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn dispatch_event(
        &self,
        packet: EnginePacket,
        writer: &mpsc::UnboundedSender<Vec<Message>>,
    ) -> Option<SignalFailure> {
        // Event payloads and ACKs can contain credentials; record metadata only.
        if let EnginePacket::Message(SocketPacket::Event {
            namespace,
            id,
            event,
            args,
            ..
        }) = &packet
        {
            tracing::trace!(
                event,
                ?id,
                argument_count = args.len(),
                "received signaling event"
            );
            if namespace != "/" {
                return None;
            }
            // B5E050 installs bmsg_push (literal F3C118), vtable F3C6C0
            // -> B60D00 -> B5F880 -> B5FEC0 -> ControlledCallback+40.
            // streamer_push is a different, higher-level session event.
            if matches!(event.as_str(), "bmsg_push" | "streamer_push") {
                tracing::debug!(?self.role, event, ?id, argument_count = args.len(), "received signaling push event");
            }
            if self.role == SignalRole::Host
                && event == "bmsg_push"
                && let Some(observer) = &self.push
            {
                if let Some(push) = args.first().filter(|value| value.is_object()) {
                    observer(push);
                } else {
                    tracing::warn!("discarded bmsg_push without an object argument");
                }
            }
            // BC3110 invokes the namespace listener, then sends its ACK list.
            // Observers queue domain work without filling this ACK list; business
            // responses are sent separately by their owning feature.
            if let Some(id) = id {
                let _ = writer.send(vec![Message::Text(format!("43{id}[]").into())]);
                tracing::debug!(id, event, "queued server-event empty ACK");
            }
            match event.as_str() {
                "leave" => return Some(SignalFailure::Kicked),
                "publisher_disconnect" => return Some(SignalFailure::PublisherDisconnected),
                _ => {}
            }
        }
        let _ = self.packets.send(Ok(packet));
        None
    }

    async fn protocol_close(
        &mut self,
        reader: &mut Reader,
        writer: &mpsc::UnboundedSender<Vec<Message>>,
        reason: &'static str,
    ) {
        let _ = writer.send(vec![Message::Close(Some(CloseFrame {
            code: CloseCode::Policy,
            reason: reason.into(),
        }))]);
        let _ = timeout(IO_STAGE_TIMEOUT, async {
            loop {
                tokio::select! {
                    message = reader.next() => if matches!(message, Some(Ok(Message::Close(_))) | Some(Err(_)) | None) { break; },
                    command = self.commands.recv() => self.command(command, false),
                }
                if self.stop { break; }
            }
        }).await;
    }

    async fn close_socket(
        &self,
        reader: &mut Reader,
        writer: &mpsc::UnboundedSender<Vec<Message>>,
        namespace_connected: bool,
    ) {
        if namespace_connected {
            let leave = encode_event("leave", &[], None).expect("fixed leave event");
            let _ = writer.send(vec![Message::Text(leave.into())]);
            if self.role == SignalRole::Host {
                // 919FD0 (host) awaits its leave/close notification for up to
                // one second. Controller 91AA20 proceeds directly.
                let _ = timeout(Duration::from_secs(1), async {
                    while let Some(Ok(message)) = reader.next().await {
                        if matches!(message, Message::Close(_)) { break; }
                        if let Message::Text(text) = message
                            && matches!(decode(&text), Ok(EnginePacket::Message(SocketPacket::Event { event, .. })) if event == "leave") { break; }
                    }
                }).await;
            }
        }
        let _ = writer.send(vec![
            Message::Text("41".into()),
            Message::Close(Some(CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            })),
        ]);
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

async fn write_packets(
    mut sink: SplitSink<Socket, Message>,
    mut packets: mpsc::UnboundedReceiver<Vec<Message>>,
) -> Result<()> {
    while let Some(messages) = packets.recv().await {
        for message in messages {
            sink.feed(message).await.context("queue signaling frame")?;
        }
        sink.flush().await.context("flush signaling packet")?;
    }
    Ok(())
}

fn endpoint_uses_tls(endpoint: &str) -> bool {
    if endpoint.contains("://") {
        url::Url::parse(endpoint)
            .is_ok_and(|url| url.scheme().eq_ignore_ascii_case("wss"))
    } else {
        true
    }
}

async fn connect_socket(
    request: Request<()>,
    tls: Option<Arc<rustls::ClientConfig>>,
) -> Result<Socket> {
    // Read the actual request header; never log authentication/reconnect tokens.
    let streamer_version = request
        .headers()
        .get(super::STREAMER_VERSION_HEADER)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("missing")
        .to_owned();
    let controlling = request
        .headers()
        .get("x-nrd-controlling")
        .is_some_and(|value| value == "1");
    let host = request
        .uri()
        .host()
        .context("signaling URL has no host")?
        .trim_matches(['[', ']'])
        .to_owned();
    let scheme = request.uri().scheme_str().context("signaling URL has no scheme")?;
    let port = request.uri().port_u16().unwrap_or(if scheme == "ws" { 80 } else { 443 });
    let addresses = timeout(IO_STAGE_TIMEOUT, lookup_host((host.as_str(), port)))
        .await
        .context("signaling DNS timed out")??
        .collect::<Vec<_>>();
    let tcp = timeout(IO_STAGE_TIMEOUT, TcpStream::connect(addresses.as_slice()))
        .await
        .context("signaling TCP connect timed out")??;
    let stream = match scheme {
        "ws" if super::is_loopback_host(&host) => SignalStream::Plain(tcp),
        "wss" => {
            let name = rustls::pki_types::ServerName::try_from(host)
                .context("invalid signaling TLS server name")?;
            let connector = tokio_rustls::TlsConnector::from(
                tls.context("signaling TLS roots were not loaded")?,
            );
            let stream = timeout(IO_STAGE_TIMEOUT, connector.connect(name, tcp))
                .await
                .context("signaling TLS handshake timed out")??;
            SignalStream::Tls(stream)
        }
        _ => bail!("unsupported signaling WebSocket scheme"),
    };
    let (socket, response) = timeout(IO_STAGE_TIMEOUT, client_async(request, stream))
        .await
        .context("signaling WebSocket opening handshake timed out")??;
    tracing::info!(
        %streamer_version,
        controlling,
        http_status = response.status().as_u16(),
        "signaling WebSocket handshake completed"
    );
    Ok(socket)
}
