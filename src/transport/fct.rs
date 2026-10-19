//! Farwing Control Transport (FCT) v1: KCP multiplexed with SCTP.
//!
//! The transport is multiplexed on decrypted DTLS application data. Ordinary
//! data channels stay on SCTP; frames of the control channel use this KCP
//! instance after both SDP descriptions negotiate `a=x-farwing-fct:1`.
//! Records start with `"FCT"` and the transport version; KCP segments carry a
//! Reed–Solomon (CM256) repair layer whose redundancy follows
//! `transport::fec_policy`.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, ensure};
use bytes::Bytes;
use kcp::Kcp;
use tokio::sync::{mpsc, oneshot};
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};
use webrtc::mux::endpoint::Endpoint;
use webrtc::sctp_transport::RTCSctpTransport;
use webrtc::util::Conn;

use crate::transport::rsfec::{cm256_decode_originals, cm256_encode_repairs};

/// KCP conversation ID (`'FWCT'`).
const KCP_CONVERSATION: u32 = 0x4657_4354;
const KCP_WIRE_MTU: usize = 1_191;
const KCP_STANDARD_MTU: usize = KCP_WIRE_MTU - 4;
const KCP_HEADER: usize = 24;
const FCT_KCP_HEADER: usize = 28;
const FEC_HEADER: usize = 32;
const RECOVERY_INFO_SIZE: usize = 37;
const MAX_CONTROL_MESSAGE: usize = 0x40080;
mod streams;
const MAX_FEC_ORIGINALS: usize = 20;
const MAX_RECENT_PACKETS: usize = 4_096;
const FEC_RETENTION: Duration = Duration::from_millis(1_000);
const RECOVERY_INFO_INTERVAL: Duration = Duration::from_millis(500);
const FEC_NETWORK_UPDATE_INTERVAL: Duration = Duration::from_millis(1_000);
const BINARY_MESSAGE: u16 = 1;
const CONTROL_SEND_WINDOW: u16 = 256;
const CONTROL_RECEIVE_WINDOW: u16 = 256;

// Input must remain replaceable locally instead of filling a reliable stream.
// One CONTROL MTU of application payload bounds committed input independently
// of the protocol's 256-segment window. A final event may cross this watermark;
// finite release obligations bypass it. Both carriers use the same byte budget.
pub(crate) const INPUT_SEND_BUDGET_BYTES: usize = KCP_STANDARD_MTU - KCP_HEADER - 4;

struct InputWindow {
    active: AtomicBool,
    pending: AtomicUsize,
    changed: tokio::sync::Notify,
}

impl InputWindow {
    fn publish(&self, pending: usize) {
        if self.pending.swap(pending, Ordering::AcqRel) != pending {
            self.changed.notify_waiters();
        }
    }
    fn close(&self) {
        self.active.store(false, Ordering::Release);
        self.changed.notify_waiters();
    }
}

const CMD_PUSH: u8 = 81;
const CMD_ACK: u8 = 82;
const CMD_RESEND_PUSH: u8 = 85;
const CMD_FEC: u8 = 86;
const CMD_RECOVERY_INFO: u8 = 87;
const CMD_FEC_DUPLICATE: u8 = 88;

#[derive(Clone, Default)]
pub(crate) struct FctControl {
    state: Arc<StdMutex<ControlState>>,
    control_streams: Arc<StdMutex<streams::Streams>>,
    send_failure: Arc<StdMutex<Option<(Instant, String)>>>,
    progress: Arc<AtomicU8>,
}

#[repr(u8)]
enum Progress {
    Inactive,
    Idle,
    Window,
    Processing,
    Writing,
    Delivering,
}

#[derive(Default)]
struct ControlState {
    version: u8,
    generation: u64,
    sender: Option<mpsc::UnboundedSender<WorkerCommand>>,
    cancel: Option<oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
    input_window: Option<Arc<InputWindow>>,
}

type SendGuard = Arc<dyn Fn() -> bool + Send + Sync>;
pub(crate) type ControlReceiver = Arc<dyn Fn(u16, &[u8]) -> Result<()> + Send + Sync>;

enum WorkerCommand {
    Drain {
        result: oneshot::Sender<()>,
    },
    Send {
        stream_id: u16,
        payload: Vec<u8>,
        guard: Option<SendGuard>,
        release: bool,
        result: oneshot::Sender<std::result::Result<usize, String>>,
    },
}

impl FctControl {
    pub(crate) fn worker_stage(&self) -> &'static str {
        match self.progress.load(Ordering::Relaxed) {
            1 => "idle",
            2 => "waiting_for_kcp_window",
            3 => "processing_kcp",
            4 => "writing_dtls",
            5 => "delivering_peer_control",
            _ => "inactive",
        }
    }
    pub(crate) fn send_failure_reason(&self) -> Option<String> {
        lock(&self.send_failure)
            .as_ref()
            .filter(|(at, _)| at.elapsed() < Duration::from_secs(3))
            .map(|(_, error)| error.clone())
    }
    /// Both roles use the negotiated CONTROL carrier. A KCP failure is returned
    /// to its owner; it must not replay the message through SCTP.
    pub(crate) async fn send_control(
        &self,
        channel: &RTCDataChannel,
        payload: Vec<u8>,
    ) -> Result<usize> {
        ensure!(
            channel.ready_state() == RTCDataChannelState::Open,
            "control-channel data channel is not open"
        );
        if self.is_negotiated() {
            self.send(channel.id(), payload).await
        } else {
            Ok(channel.send(&Bytes::from(payload)).await?)
        }
    }

    pub(crate) fn is_negotiated(&self) -> bool {
        lock(&self.state).version != 0
    }

    pub(crate) fn set_control_stream(&self, stream_id: u16, open: bool) {
        lock(&self.control_streams).set_open(stream_id, open);
    }

    pub(crate) fn bind_stream(&self, stream_id: u16, control: bool) {
        lock(&self.control_streams).bind(stream_id, control);
    }

    pub(crate) fn start_receiver(
        &self,
        transport: Arc<RTCSctpTransport>,
        version: u8,
        stream_control: ControlReceiver,
    ) -> Result<()> {
        ensure!(version == FCT_VERSION, "unsupported FCT version {version}");
        let mut state = lock(&self.state);
        if state.version != 0 {
            ensure!(
                state.version == version,
                "FCT version changed within one peer connection"
            );
            return Ok(());
        }
        let (sender, receiver) = mpsc::unbounded_channel();
        let (cancel, mut canceled) = oneshot::channel();
        state.generation = state.generation.wrapping_add(1);
        let generation = state.generation;
        state.version = version;
        state.cancel = Some(cancel);
        let shared_state = Arc::clone(&self.state);
        let control_streams = Arc::clone(&self.control_streams);
        let send_failure = Arc::clone(&self.send_failure);
        let progress = Arc::clone(&self.progress);
        *lock(&send_failure) = None;
        state.task = Some(tokio::spawn(async move {
            // DcKcpTransport::Start configures the KCP object and its task,
            // without waiting for PacketTransport::writable. Do not block
            // the SOAC reader: DTLS needs candidates arriving on that reader.
            let endpoint = tokio::select! {
                biased;
                _ = &mut canceled => None,
                endpoint = transport.wait_new_data_endpoint(Box::new(move |packet| {
                    packet.len() >= 4 && packet[..3] == *b"FCT" && packet[3] == version
                })) => endpoint,
            };
            let result = if let Some(endpoint) = endpoint {
                let input_window = Arc::new(InputWindow {
                    active: AtomicBool::new(true),
                    pending: AtomicUsize::new(0),
                    changed: tokio::sync::Notify::new(),
                });
                let current = {
                    let mut state = lock(&shared_state);
                    let current = state.generation == generation && state.version == version;
                    if current {
                        state.sender = Some(sender);
                        state.input_window = Some(input_window.clone());
                    }
                    current
                };
                let result = if current {
                    tokio::select! {
                        biased;
                        _ = &mut canceled => Ok(()),
                        result = run_worker(Arc::clone(&endpoint), version, receiver, stream_control, control_streams, send_failure, progress.clone(), input_window.clone()) => result,
                    }
                } else {
                    Ok(())
                };
                input_window.close();
                // Unregister only our endpoint, including canceled startup.
                let _ = Endpoint::close(&endpoint).await;
                result
            } else {
                Ok(())
            };
            let mut state = lock(&shared_state);
            if state.generation == generation {
                // Worker failure does not renegotiate the wire protocol.
                state.sender = None;
                state.input_window = None;
                progress.store(Progress::Inactive as u8, Ordering::Relaxed);
            }
            drop(state);
            if let Err(error) = result {
                tracing::warn!(%error, version, "FCT worker stopped");
            }
        }));
        tracing::debug!(
            version,
            generation,
            "FCT negotiated; awaiting DTLS in transport task"
        );
        Ok(())
    }

    pub(crate) async fn send(&self, stream_id: u16, payload: Vec<u8>) -> Result<usize> {
        self.send_inner(stream_id, payload, None, false).await
    }

    pub(crate) async fn wait_input_capacity(&self) -> Result<()> {
        let window = lock(&self.state)
            .input_window
            .clone()
            .context("FCT is not active")?;
        loop {
            let changed = window.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            ensure!(
                window.active.load(Ordering::Acquire),
                "FCT worker is closed"
            );
            if window.pending.load(Ordering::Acquire) < INPUT_SEND_BUDGET_BYTES {
                return Ok(());
            }
            changed.await;
        }
    }
    /// Enqueued after the recovery release/probe pass. Completion means all
    /// earlier reliable segments were acknowledged, not just written to DTLS.
    pub(crate) async fn drain(&self) -> Result<()> {
        let sender = lock(&self.state)
            .sender
            .clone()
            .context("FCT is not active")?;
        let (result, received) = oneshot::channel();
        sender
            .send(WorkerCommand::Drain { result })
            .map_err(|_| anyhow!("FCT worker is closed"))?;
        received
            .await
            .context("FCT worker stopped before acknowledgement")
    }
    pub(crate) async fn send_input(
        &self,
        stream_id: u16,
        payload: Vec<u8>,
        guard: SendGuard,
        release: bool,
    ) -> Result<usize> {
        self.send_inner(stream_id, payload, Some(guard), release)
            .await
    }
    async fn send_inner(
        &self,
        stream_id: u16,
        payload: Vec<u8>,
        guard: Option<SendGuard>,
        release: bool,
    ) -> Result<usize> {
        ensure!(
            payload.len() <= MAX_CONTROL_MESSAGE,
            "control-channel message exceeds the FCT limit"
        );
        let sender = lock(&self.state)
            .sender
            .clone()
            .context("FCT is not active")?;
        let (result_tx, result_rx) = oneshot::channel();
        sender
            .send(WorkerCommand::Send {
                stream_id,
                payload,
                guard,
                release,
                result: result_tx,
            })
            .map_err(|_| anyhow!("FCT worker is closed"))?;
        result_rx
            .await
            .map_err(|_| anyhow!("FCT worker stopped before sending"))?
            .map_err(anyhow::Error::msg)
    }

    pub(crate) async fn close(&self) {
        let task = {
            let mut state = lock(&self.state);
            state.generation = state.generation.wrapping_add(1);
            state.version = 0;
            state.sender.take();
            if let Some(window) = state.input_window.take() {
                window.close();
            }
            if let Some(cancel) = state.cancel.take() {
                let _ = cancel.send(());
            }
            state.task.take()
        };
        if let Some(task) = task {
            let _ = task.await;
        }
        lock(&self.control_streams).clear();
    }

    // Also cancel from owners' Drop paths, where awaiting the task is impossible.
    pub(crate) fn stop(&self) {
        let mut state = lock(&self.state);
        state.generation = state.generation.wrapping_add(1);
        state.version = 0;
        state.sender.take();
        if let Some(window) = state.input_window.take() {
            window.close();
        }
        if let Some(cancel) = state.cancel.take() {
            let _ = cancel.send(());
        }
        lock(&self.control_streams).clear();
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[derive(Clone)]
struct KcpOutput {
    packets: Arc<StdMutex<VecDeque<Vec<u8>>>>,
}

impl Write for KcpOutput {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        lock(&self.packets).push_back(buffer.to_vec());
        Ok(buffer.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

struct Worker {
    version: u8,
    epoch: Instant,
    kcp: Kcp<KcpOutput>,
    output_packets: Arc<StdMutex<VecDeque<Vec<u8>>>>,
    wire_packets: VecDeque<WirePacket>,
    sent_sequences: HashSet<u32>,
    sent_order: VecDeque<u32>,
    fec_generator: FecGenerator,
    fec_receiver: FecReceiver,
    recovery: RecoveryState,
    last_recovery_info: Instant,
    last_fec_network_update: Instant,
    last_remote_header: Option<RemoteHeader>,
    control_streams: Arc<StdMutex<streams::Streams>>,
    send_failure: Arc<StdMutex<Option<(Instant, String)>>>,
    last_send_warning: Option<Instant>,
    progress: Arc<AtomicU8>,
    last_peer_input: Option<Instant>,
    input_credits: VecDeque<(u32, usize)>,
    input_pending_bytes: usize,
}

struct WirePacket {
    data: Vec<u8>,
    fec_original_sequence: Option<u32>,
}

#[derive(Clone, Copy)]
struct RemoteHeader {
    window: u16,
    timestamp: u32,
    una: u32,
}

async fn run_worker(
    endpoint: Arc<Endpoint>,
    version: u8,
    mut commands: mpsc::UnboundedReceiver<WorkerCommand>,
    stream_control: ControlReceiver,
    control_streams: Arc<StdMutex<streams::Streams>>,
    send_failure: Arc<StdMutex<Option<(Instant, String)>>>,
    progress: Arc<AtomicU8>,
    input_window: Arc<InputWindow>,
) -> Result<()> {
    let output_packets = Arc::new(StdMutex::new(VecDeque::new()));
    let mut kcp = Kcp::new(
        KCP_CONVERSATION,
        KcpOutput {
            packets: Arc::clone(&output_packets),
        },
    );
    kcp.set_mtu(KCP_STANDARD_MTU)
        .context("configure FCT MTU")?;
    kcp.set_wndsize(CONTROL_SEND_WINDOW, CONTROL_RECEIVE_WINDOW);
    // configures nodelay(1, 5, 2, 1). The local KCP fork preserves its
    // two-millisecond lower clamp instead of upstream KCP's ten milliseconds.
    kcp.set_nodelay(true, 5, 2, true);
    kcp.set_rx_minrto(10);
    // DcKcpTransport overwrites the nodelay helper's fast-resend value with 1
    // after construction and does not apply an xmit fast-limit.
    kcp.set_fast_resend(1);
    kcp.set_fast_resend_limit(0);

    let now = Instant::now();
    let mut worker = Worker {
        version,
        epoch: now,
        kcp,
        output_packets,
        wire_packets: VecDeque::new(),
        sent_sequences: HashSet::new(),
        sent_order: VecDeque::new(),
        fec_generator: FecGenerator::new(now),
        fec_receiver: FecReceiver::default(),
        recovery: RecoveryState::default(),
        last_recovery_info: now,
        last_fec_network_update: now,
        last_remote_header: None,
        control_streams,
        send_failure,
        last_send_warning: None,
        progress,
        last_peer_input: None,
        input_credits: VecDeque::new(),
        input_pending_bytes: 0,
    };
    worker
        .kcp
        .update(worker.now_ms())
        .context("start FCT clock")?;

    tracing::info!(
        version,
        conversation = KCP_CONVERSATION,
        wire_mtu = KCP_WIRE_MTU,
        send_window = CONTROL_SEND_WINDOW,
        receive_window = CONTROL_RECEIVE_WINDOW,
        "FCT control transport started"
    );

    let mut tick = tokio::time::interval(Duration::from_millis(10));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut receive_buffer = vec![0_u8; 65_536];
    let mut pending = None;
    let mut pending_since = Instant::now();
    let mut wait_reported = false;
    let mut drains: Vec<oneshot::Sender<()>> = Vec::new();
    loop {
        input_window.publish(worker.pending_input_bytes());
        drains.retain(|waiter| !waiter.is_closed());
        if worker.kcp.wait_snd() == 0 {
            for waiter in drains.drain(..) {
                let _ = waiter.send(());
            }
        }
        worker
            .progress
            .store(Progress::Idle as u8, Ordering::Relaxed);
        if let Some(WorkerCommand::Send {
            stream_id,
            payload,
            guard,
            release,
            result,
        }) = pending.take()
        {
            // Check cancellation before assigning reliable sequence numbers.
            // The input-only byte watermark applies before assigning reliable
            // sequence numbers. General CONTROL RPC and finite releases retain
            // their existing semantics; the negotiated window stays unchanged.
            if result.is_closed() || guard.as_ref().is_some_and(|valid: &SendGuard| !valid()) {
                let _ = result.send(Err("control request cancelled before transmission".into()));
            } else if guard.is_some()
                && !release
                && worker.input_pending_bytes >= INPUT_SEND_BUDGET_BYTES
            {
                worker
                    .progress
                    .store(Progress::Window as u8, Ordering::Relaxed);
                if !wait_reported && pending_since.elapsed() >= Duration::from_millis(250) {
                    tracing::warn!(
                        stream_id, waiting_ms = pending_since.elapsed().as_millis() as u64,
                        outstanding = worker.kcp.wait_snd(), remote_window = worker.kcp.rmt_wnd(),
                        pending_bytes = worker.input_pending_bytes, input_budget = INPUT_SEND_BUDGET_BYTES,
                        kcp_srtt_ms = worker.kcp.rx_srtt(),
                        last_peer_input_ms = ?worker.last_peer_input.map(|at| at.elapsed().as_millis() as u64),
                        "input blocked waiting for FCT send window"
                    );
                    wait_reported = true;
                }
                pending = Some(WorkerCommand::Send {
                    stream_id,
                    payload,
                    guard,
                    release,
                    result,
                });
            } else {
                worker
                    .progress
                    .store(Progress::Processing as u8, Ordering::Relaxed);
                let outcome = match worker.send_message(stream_id, &payload) {
                    Ok(bytes) => worker.flush_output(&endpoint).await.map(|()| bytes),
                    Err(error) => Err(error),
                };
                input_window.publish(worker.pending_input_bytes());
                let _ = result.send(outcome.map_err(|error| error.to_string()));
            }
        }
        tokio::select! {
            command = commands.recv(), if pending.is_none() => {
                let Some(command) = command else { break; };
                match command {
                    WorkerCommand::Drain { result } => drains.push(result),
                    command => pending = Some(command),
                }
                pending_since = Instant::now();
                wait_reported = false;
            }
            received = endpoint.recv(&mut receive_buffer) => {
                let size = received.context("receive FCT datagram")?;
                worker.progress.store(Progress::Processing as u8, Ordering::Relaxed);
                if let Err(error) = worker.receive_datagram(&receive_buffer[..size], &stream_control) {
                    tracing::warn!(%error, bytes = size, "discarding invalid FCT datagram");
                }
                worker.flush_output(&endpoint).await?;
            }
            _ = tick.tick() => {
                worker.progress.store(Progress::Processing as u8, Ordering::Relaxed);
                worker.on_tick()?;
                worker.deliver_messages(&stream_control);
                worker.flush_output(&endpoint).await?;
            }
        }
    }
    Ok(())
}

impl Worker {
    fn pending_input_bytes(&mut self) -> usize {
        let prefix = self.kcp.send_unacknowledged();
        while self
            .input_credits
            .front()
            .is_some_and(|(end, _)| prefix.wrapping_sub(*end) as i32 >= 0)
        {
            let (_, bytes) = self.input_credits.pop_front().unwrap();
            self.input_pending_bytes -= bytes;
        }
        self.input_pending_bytes
    }

    fn now_ms(&self) -> u32 {
        self.epoch.elapsed().as_millis() as u32
    }

    fn send_message(&mut self, stream_id: u16, payload: &[u8]) -> Result<usize> {
        let mut message = Vec::with_capacity(payload.len() + 4);
        message.extend_from_slice(payload);
        message.extend_from_slice(&stream_id.to_le_bytes());
        message.extend_from_slice(&BINARY_MESSAGE.to_le_bytes());
        let bytes = self
            .kcp
            .send(&message)
            .context("queue control-channel message in FCT")?;
        // Retain credit until the ordered ACK prefix crosses the message, not
        // just until a later segment is selectively acknowledged out of order.
        self.input_credits
            .push_back((self.kcp.send_sequence_end(), bytes));
        self.input_pending_bytes += bytes;
        let now = self.now_ms();
        self.kcp.update(now).context("update FCT")?;
        self.kcp.flush().context("flush FCT message")?;
        Ok(bytes.saturating_sub(4))
    }

    fn on_tick(&mut self) -> Result<()> {
        let now_ms = self.now_ms();
        self.kcp.update(now_ms).context("update FCT")?;
        let now = Instant::now();
        if now.duration_since(self.last_recovery_info) > RECOVERY_INFO_INTERVAL {
            self.last_recovery_info = now;
            let recovery_info = self.recovery.encode_info(self.version, now_ms);
            self.queue_wire_packet(recovery_info);
        }
        if now.duration_since(self.last_fec_network_update) > FEC_NETWORK_UPDATE_INTERVAL {
            self.last_fec_network_update = now;
            if let Some(loss) = self.recovery.remote_loss_ratio
                && let Some(decrease_fast_ack_after) =
                    self.fec_generator
                        .update_network(loss, self.kcp.rx_srtt(), now)
            {
                self.kcp
                    .set_decrease_fast_ack_after(decrease_fast_ack_after);
            }
        }
        Ok(())
    }

    async fn flush_output(&mut self, endpoint: &Endpoint) -> Result<()> {
        self.transform_kcp_output()?;
        for packet in self.fec_generator.poll(Instant::now())? {
            self.queue_wire_packet(packet);
        }
        while let Some(packet) = self.wire_packets.pop_front() {
            tracing::trace!(
                command = packet.data.get(8).copied().unwrap_or_default(),
                bytes = packet.data.len(),
                wire = %hex_prefix(&packet.data, 96),
                "sending FCT datagram"
            );
            self.progress
                .store(Progress::Writing as u8, Ordering::Relaxed);
            let sent = endpoint.send(&packet.data).await;
            self.progress
                .store(Progress::Processing as u8, Ordering::Relaxed);
            if let Err(error) = sent {
                // DcKcpTransport::SendPacket keeps KCP alive across temporary
                // transport failures; its send buffer owns PUSH retransmission.
                let now = Instant::now();
                *lock(&self.send_failure) = Some((now, error.to_string()));
                if self
                    .last_send_warning
                    .is_none_or(|at| now.duration_since(at) >= Duration::from_secs(1))
                {
                    self.last_send_warning = Some(now);
                    tracing::debug!(%error, "FCT datagram not sent on DTLS transport");
                }
                continue;
            }
            *lock(&self.send_failure) = None;
            if let Some(sequence) = packet.fec_original_sequence {
                // Feed the FEC worker only after the original datagram has
                // actually been accepted by DTLS.
                self.fec_generator.push(
                    canonical_fec_packet(packet.data)?,
                    sequence,
                    Instant::now(),
                );
                for repair in self.fec_generator.poll(Instant::now())? {
                    self.queue_wire_packet(repair);
                }
            }
        }
        Ok(())
    }

    fn transform_kcp_output(&mut self) -> Result<()> {
        let standard_datagrams = {
            let mut packets = lock(&self.output_packets);
            packets.drain(..).collect::<Vec<_>>()
        };
        for datagram in standard_datagrams {
            let mut cursor = 0;
            while cursor < datagram.len() {
                ensure!(
                    datagram.len() - cursor >= KCP_HEADER,
                    "short standard KCP output segment"
                );
                let payload_size = read_u32(&datagram, cursor + 20)? as usize;
                let end = cursor
                    .checked_add(KCP_HEADER + payload_size)
                    .context("KCP output segment length overflow")?;
                ensure!(
                    end <= datagram.len(),
                    "truncated standard KCP output segment"
                );
                let standard = &datagram[cursor..end];
                ensure!(
                    read_u32(standard, 0)? == KCP_CONVERSATION,
                    "KCP conversation mismatch"
                );
                let command = standard[4];
                let sequence = read_u32(standard, 12)?;
                let first_transmission =
                    command == CMD_PUSH && self.sent_sequences.insert(sequence);

                let mut wire = Vec::with_capacity(standard.len() + 4);
                wire.extend_from_slice(&magic(self.version));
                wire.extend_from_slice(standard);
                if command == CMD_PUSH && !first_transmission {
                    wire[8] = CMD_RESEND_PUSH;
                }
                self.wire_packets.push_back(WirePacket {
                    data: wire,
                    fec_original_sequence: first_transmission.then_some(sequence),
                });
                if first_transmission {
                    self.sent_order.push_back(sequence);
                    while self.sent_order.len() > MAX_RECENT_PACKETS {
                        if let Some(expired) = self.sent_order.pop_front() {
                            self.sent_sequences.remove(&expired);
                        }
                    }
                }
                cursor = end;
            }
        }
        Ok(())
    }

    fn queue_wire_packet(&mut self, packet: Vec<u8>) {
        self.wire_packets.push_back(WirePacket {
            data: packet,
            fec_original_sequence: None,
        });
    }

    fn receive_datagram(
        &mut self,
        datagram: &[u8],
        stream_control: &ControlReceiver,
    ) -> Result<()> {
        tracing::trace!(
            command = datagram.get(8).copied().unwrap_or_default(),
            bytes = datagram.len(),
            wire = %hex_prefix(datagram, 96),
            "received FCT datagram"
        );
        let mut cursor = 0;
        while cursor < datagram.len() {
            ensure!(datagram.len() - cursor >= 9, "short FCT segment");
            ensure!(
                datagram[cursor..cursor + 4] == magic(self.version),
                "FCT magic/version mismatch"
            );
            ensure!(
                read_u32(datagram, cursor + 4)? == KCP_CONVERSATION,
                "FCT conversation mismatch"
            );
            let command = datagram[cursor + 8];
            let segment_size = match command {
                CMD_FEC => {
                    ensure!(
                        datagram.len() - cursor >= FEC_HEADER,
                        "short FCT FEC header"
                    );
                    FEC_HEADER + read_u32(datagram, cursor + 25)? as usize
                }
                CMD_RECOVERY_INFO => RECOVERY_INFO_SIZE,
                _ => {
                    ensure!(
                        datagram.len() - cursor >= FCT_KCP_HEADER,
                        "short FCT data header"
                    );
                    FCT_KCP_HEADER + read_u32(datagram, cursor + 24)? as usize
                }
            };
            let end = cursor
                .checked_add(segment_size)
                .context("FCT segment length overflow")?;
            ensure!(end <= datagram.len(), "truncated FCT segment");
            let segment = &datagram[cursor..end];
            match command {
                CMD_FEC => {
                    let recovered = self.fec_receiver.receive_repair(segment, Instant::now())?;
                    for packet in recovered {
                        self.input_data_segment(&packet, true, stream_control)?;
                    }
                }
                CMD_RECOVERY_INFO => {
                    self.recovery.receive_info(segment)?;
                }
                _ => {
                    self.input_data_segment(segment, false, stream_control)?;
                    if matches!(command, CMD_PUSH | CMD_RESEND_PUSH) {
                        let sequence = read_u32(segment, 16)?;
                        let recovered = self.fec_receiver.remember_original(
                            sequence,
                            canonical_fec_packet(segment.to_vec())?,
                            Instant::now(),
                        )?;
                        for packet in recovered {
                            self.input_data_segment(&packet, true, stream_control)?;
                        }
                    }
                }
            }
            cursor = end;
        }
        Ok(())
    }

    fn input_data_segment(
        &mut self,
        wire: &[u8],
        recovered_by_fec: bool,
        stream_control: &ControlReceiver,
    ) -> Result<()> {
        ensure!(
            wire.len() >= FCT_KCP_HEADER,
            "short FCT data segment"
        );
        let wire_command = wire[8];
        ensure!(
            matches!(
                wire_command,
                CMD_PUSH | CMD_ACK | 83 | 84 | CMD_RESEND_PUSH | CMD_FEC_DUPLICATE
            ),
            "unsupported FCT command {wire_command}"
        );
        let mut standard = wire[4..].to_vec();
        if matches!(wire_command, CMD_RESEND_PUSH | CMD_FEC_DUPLICATE) {
            standard[4] = CMD_PUSH;
        }
        if recovered_by_fec || wire_command == CMD_FEC_DUPLICATE {
            // Repairs contain normalized (zeroed) flow-control metadata. Restore
            // from the real peer header BEFORE input; CMD88 must never overwrite
            // that cache with its own zeros and close an otherwise open window.
            let header = self.last_remote_header.unwrap_or(RemoteHeader {
                window: 256,
                timestamp: self.now_ms(),
                una: 0,
            });
            standard[6..8].copy_from_slice(&header.window.to_le_bytes());
            standard[8..12].copy_from_slice(&header.timestamp.to_le_bytes());
            standard[16..20].copy_from_slice(&header.una.to_le_bytes());
        }
        let accepted = self.kcp.accepted_segments();
        let receive_next = self.kcp.receive_next();
        self.kcp
            .input(&standard)
            .context("input FCT segment")?;
        // Input remembers headers only for eligible PUSH/retransmits.
        // A rejected/out-of-window packet or a synthesized repair is not a new
        // peer advertisement, even if its payload is a duplicate.
        if !recovered_by_fec
            && matches!(wire_command, CMD_PUSH | CMD_RESEND_PUSH)
            && (read_u32(wire, 16)?.wrapping_sub(receive_next) as i32)
                < i32::from(CONTROL_RECEIVE_WINDOW)
        {
            self.last_remote_header = Some(RemoteHeader {
                window: read_u16(wire, 10)?,
                timestamp: read_u32(wire, 12)?,
                una: read_u32(wire, 20)?,
            });
        }
        if !recovered_by_fec {
            self.last_peer_input = Some(Instant::now());
        }
        if self.kcp.accepted_segments() != accepted {
            self.recovery
                .record_received(read_u32(wire, 16)?, wire_command, recovered_by_fec);
        }
        let now = self.now_ms();
        self.kcp.update(now).context("flush FCT ACK")?;
        self.kcp.flush().context("flush FCT input")?;
        let result = self.drain_messages(stream_control);
        self.fec_receiver.receive_next = self.kcp.receive_next();
        self.fec_receiver.prune(Instant::now());
        result
    }

    fn drain_messages(&mut self, stream_control: &ControlReceiver) -> Result<()> {
        self.deliver_messages(stream_control);
        while let Ok(size) = self.kcp.peeksize() {
            let mut message = vec![0_u8; size];
            let received = self
                .kcp
                .recv(&mut message)
                .context("receive reassembled FCT message")?;
            ensure!(received == size, "FCT returned a partial message");
            ensure!(
                message.len() >= 4,
                "FCT message omitted its stream trailer"
            );
            let trailer = message.split_off(message.len() - 4);
            let stream_id = u16::from_le_bytes([trailer[0], trailer[1]]);
            let message_type = u16::from_le_bytes([trailer[2], trailer[3]]);
            if !lock(&self.control_streams)
                .push(stream_id, message)
                .map_err(|error| anyhow!(error))?
            {
                tracing::debug!(
                    stream_id,
                    message_type,
                    "ignoring non-CONTROL FCT stream"
                );
                continue;
            }
            self.deliver_messages(stream_control);
        }
        self.deliver_messages(stream_control);
        Ok(())
    }

    fn deliver_messages(&mut self, stream_control: &ControlReceiver) {
        loop {
            // Do not hold the registry lock while calling a business consumer.
            let pending = lock(&self.control_streams).pop_ready();
            let Some((stream_id, message)) = pending else {
                break;
            };
            self.progress
                .store(Progress::Delivering as u8, Ordering::Relaxed);
            let delivered = stream_control(stream_id, &message);
            self.progress
                .store(Progress::Processing as u8, Ordering::Relaxed);
            if let Err(error) = delivered {
                // Discard an invalid application message without
                // interrupting delivery of subsequent transport messages.
                tracing::warn!(%error, stream_id, bytes = message.len(), "invalid control-channel protobuf from FCT");
            }
        }
    }
}

/// FCT record version negotiated by `a=x-farwing-fct`.
pub(crate) const FCT_VERSION: u8 = 1;

fn magic(version: u8) -> [u8; 4] {
    [b'F', b'C', b'T', version]
}

fn hex_prefix(bytes: &[u8], limit: usize) -> String {
    use std::fmt::Write as _;
    let mut output = String::with_capacity(bytes.len().min(limit) * 2);
    for byte in bytes.iter().take(limit) {
        let _ = write!(output, "{byte:02X}");
    }
    output
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16> {
    let value = bytes
        .get(offset..offset + 2)
        .context("truncated little-endian u16")?;
    Ok(u16::from_le_bytes(
        value.try_into().expect("two-byte slice"),
    ))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32> {
    let value = bytes
        .get(offset..offset + 4)
        .context("truncated little-endian u32")?;
    Ok(u32::from_le_bytes(
        value.try_into().expect("four-byte slice"),
    ))
}

fn canonical_fec_packet(mut packet: Vec<u8>) -> Result<Vec<u8>> {
    ensure!(
        packet.len() >= FCT_KCP_HEADER,
        "short KCP packet for FEC normalization"
    );
    packet[8] = CMD_PUSH;
    packet[10..12].fill(0);
    packet[12..16].fill(0);
    packet[20..24].fill(0);
    Ok(packet)
}

struct FecSource {
    sequence: u32,
    packet: Vec<u8>,
    received_at: Instant,
}

struct FecGenerator {
    queue: VecDeque<FecSource>,
    state: u8,
    high_rtt: bool,
    low_loss_state: bool,
    high_loss_state: bool,
    loss_ratio: f64,
    interval: Duration,
    forced_group_size: usize,
    last_flush: Instant,
    last_state_update: Option<Instant>,
    group_id: u32,
}

impl FecGenerator {
    fn new(now: Instant) -> Self {
        Self {
            queue: VecDeque::new(),
            state: 0,
            high_rtt: false,
            low_loss_state: false,
            high_loss_state: false,
            loss_ratio: 0.0,
            interval: Duration::from_millis(30),
            forced_group_size: 0,
            last_flush: now,
            last_state_update: None,
            group_id: 0,
        }
    }

    fn push(&mut self, packet: Vec<u8>, sequence: u32, now: Instant) {
        let expiry = self.interval.saturating_mul(2);
        while self
            .queue
            .front()
            .is_some_and(|packet| now.duration_since(packet.received_at) >= expiry)
        {
            self.queue.pop_front();
        }
        if self
            .queue
            .back()
            .is_some_and(|packet| sequence.wrapping_sub(packet.sequence) as i32 <= 0)
        {
            return;
        }
        self.queue.push_back(FecSource {
            sequence,
            packet,
            received_at: now,
        });
    }

    fn update_network(&mut self, loss_ratio: f64, rtt_ms: u32, now: Instant) -> Option<bool> {
        if !(0.0..=1.0).contains(&loss_ratio)
            || rtt_ms > 9_999
            || self
                .last_state_update
                .is_some_and(|previous| now.duration_since(previous) < Duration::from_millis(100))
        {
            return None;
        }
        self.last_state_update = Some(now);
        self.loss_ratio = if loss_ratio < 0.01 { 0.0 } else { loss_ratio };

        if self.high_rtt {
            if rtt_ms <= 40 {
                self.high_rtt = false;
            }
        } else if rtt_ms >= 50 {
            self.high_rtt = true;
        }

        let previous_state = self.state;
        self.state = if self.high_rtt {
            self.low_loss_state = false;
            if self.high_loss_state {
                if self.loss_ratio <= 0.11 {
                    self.high_loss_state = false;
                }
            } else if self.loss_ratio >= 0.15 {
                self.high_loss_state = true;
            }
            if self.high_loss_state { 3 } else { 2 }
        } else {
            self.high_loss_state = false;
            if self.low_loss_state {
                if self.loss_ratio <= 0.07 {
                    self.low_loss_state = false;
                }
            } else if self.loss_ratio >= 0.10 {
                self.low_loss_state = true;
            }
            if self.low_loss_state { 1 } else { 0 }
        };

        self.forced_group_size = 0;
        match self.state {
            0 => {
                self.interval = Duration::from_millis(30);
                self.loss_ratio = 0.0;
            }
            1 => {
                self.interval = Duration::from_millis(u64::from((rtt_ms / 2).max(10)));
                self.loss_ratio *= self.loss_ratio;
            }
            2 => {
                self.interval = Duration::from_millis(u64::from((rtt_ms / 2).max(30)));
                self.loss_ratio = self.loss_ratio.max(0.02);
            }
            3 => {
                self.interval = Duration::from_millis(u64::from(((3 * rtt_ms) / 4).max(30)));
                let percent = (self.loss_ratio.clamp(0.0, 1.0) * 100.0) as u8;
                self.forced_group_size = match percent {
                    0..=20 => 6,
                    21..=25 => 8,
                    26..=30 => 12,
                    31..=35 => 16,
                    _ => 0,
                };
            }
            _ => unreachable!(),
        }
        (self.state != previous_state).then_some(self.state == 3)
    }

    fn poll(&mut self, now: Instant) -> Result<Vec<Vec<u8>>> {
        if self.queue.is_empty()
            || (now.duration_since(self.last_flush) < self.interval
                && (self.forced_group_size == 0 || self.queue.len() < self.forced_group_size))
        {
            return Ok(Vec::new());
        }
        self.last_flush = now;
        let original_count = self.queue.len().min(MAX_FEC_ORIGINALS);
        if self.loss_ratio.abs() < f64::EPSILON {
            self.queue.drain(..original_count);
            return Ok(Vec::new());
        }
        let repair_count = fec_repair_count(original_count, self.loss_ratio);
        if original_count + repair_count > 256 {
            self.queue.drain(..original_count);
            return Ok(Vec::new());
        }
        let sources = self.queue.drain(..original_count).collect::<Vec<_>>();
        if sources.is_empty() || sources.iter().any(|source| source.sequence == u32::MAX) {
            return Ok(Vec::new());
        }
        let base_sequence = sources[0].sequence;
        let mut mask = 0_u64;
        for source in &sources {
            let difference = source.sequence.wrapping_sub(base_sequence);
            if difference >= 64 {
                return Ok(Vec::new());
            }
            mask |= 1_u64 << difference;
        }
        if original_count == 1 && repair_count <= 1 {
            let mut duplicate = sources[0].packet.clone();
            duplicate[8] = CMD_FEC_DUPLICATE;
            return Ok(vec![duplicate]);
        }

        let shard_size = sources
            .iter()
            .map(|source| source.packet.len())
            .max()
            .unwrap_or_default();
        ensure!(
            shard_size <= KCP_WIRE_MTU,
            "FCT FEC source exceeds wire MTU"
        );
        let originals = sources
            .iter()
            .map(|source| {
                let mut shard = vec![0_u8; shard_size];
                shard[..source.packet.len()].copy_from_slice(&source.packet);
                shard
            })
            .collect::<Vec<_>>();
        let repairs = cm256_encode_repairs(&originals, repair_count as u8, shard_size)?;
        let group_id = self.group_id;
        self.group_id = self.group_id.wrapping_add(1);
        let mut packets = Vec::with_capacity(repairs.len());
        for (index, repair) in repairs.into_iter().enumerate() {
            let mut packet = Vec::with_capacity(FEC_HEADER + repair.len());
            packet.extend_from_slice(&magic(FCT_VERSION));
            packet.extend_from_slice(&KCP_CONVERSATION.to_le_bytes());
            packet.push(CMD_FEC);
            packet.extend_from_slice(&group_id.to_le_bytes());
            packet.extend_from_slice(&base_sequence.to_le_bytes());
            packet.extend_from_slice(&mask.to_le_bytes());
            packet.extend_from_slice(&(shard_size as u32).to_le_bytes());
            packet.push(original_count as u8);
            packet.push(repair_count as u8);
            packet.push(index as u8);
            packet.extend_from_slice(&repair);
            packets.push(packet);
        }
        Ok(packets)
    }
}

fn fec_repair_count(original_count: usize, loss_ratio: f64) -> usize {
    if original_count <= 1 {
        return 1;
    }
    let k = original_count.min(MAX_FEC_ORIGINALS);
    crate::transport::fec_policy::repair_count(
        k,
        loss_ratio,
        crate::transport::fec_policy::CONTROL_TARGET,
        1,
        k,
    )
}

#[derive(Default)]
struct FecReceiver {
    receive_next: u32,
    originals: HashMap<u32, RecentOriginal>,
    original_order: VecDeque<u32>,
    groups: HashMap<u32, FecGroup>,
    group_order: VecDeque<u32>,
    completed: HashSet<u32>,
    completed_order: VecDeque<u32>,
}

struct RecentOriginal {
    packet: Vec<u8>,
    received_at: Instant,
}

struct FecGroup {
    base_sequence: u32,
    sequences: Vec<u32>,
    original_count: u8,
    repair_count: u8,
    shard_size: usize,
    shards: BTreeMap<u8, Vec<u8>>,
}

impl FecReceiver {
    fn remember_original(
        &mut self,
        sequence: u32,
        packet: Vec<u8>,
        now: Instant,
    ) -> Result<Vec<Vec<u8>>> {
        if let std::collections::hash_map::Entry::Vacant(entry) = self.originals.entry(sequence) {
            entry.insert(RecentOriginal {
                packet: packet.clone(),
                received_at: now,
            });
            self.original_order.push_back(sequence);
        }
        let matching = self
            .groups
            .iter()
            .filter_map(|(group_id, group)| {
                group.sequences.contains(&sequence).then_some(*group_id)
            })
            .collect::<Vec<_>>();
        let mut recovered = Vec::new();
        for group_id in matching {
            if let Some(group) = self.groups.get_mut(&group_id)
                && let Some(index) = group.sequences.iter().position(|value| *value == sequence)
            {
                insert_fec_original(group, index as u8, &packet);
            }
            recovered.extend(self.attempt(group_id)?);
        }
        self.prune(now);
        Ok(recovered)
    }

    fn receive_repair(&mut self, packet: &[u8], now: Instant) -> Result<Vec<Vec<u8>>> {
        ensure!(packet.len() >= FEC_HEADER, "short FCT FEC packet");
        ensure!(packet[..4] == magic(FCT_VERSION), "FCT FEC version mismatch");
        ensure!(
            read_u32(packet, 4)? == KCP_CONVERSATION,
            "FCT FEC conversation mismatch"
        );
        ensure!(packet[8] == CMD_FEC, "FCT FEC command mismatch");
        let group_id = read_u32(packet, 9)?;
        let base_sequence = read_u32(packet, 13)?;
        let mask = u64::from_le_bytes(packet[17..25].try_into().expect("eight-byte FEC mask"));
        let shard_size = read_u32(packet, 25)? as usize;
        let original_count = packet[29];
        let repair_count = packet[30];
        let repair_index = packet[31];
        ensure!(
            (1..=20).contains(&original_count),
            "invalid FCT FEC original count"
        );
        ensure!(repair_count != 0, "invalid FCT FEC repair count");
        ensure!(
            u16::from(original_count) + u16::from(repair_count) <= 256,
            "invalid FCT FEC shard count"
        );
        ensure!(
            repair_index < repair_count,
            "FCT FEC index is outside repair count"
        );
        ensure!(
            (FCT_KCP_HEADER..=KCP_WIRE_MTU).contains(&shard_size),
            "invalid FCT FEC shard size"
        );
        ensure!(
            packet.len() >= FEC_HEADER + shard_size,
            "truncated FCT FEC repair shard"
        );
        let sequences = (0..64)
            .filter(|bit| mask & (1_u64 << bit) != 0)
            .map(|bit| base_sequence.wrapping_add(bit))
            .collect::<Vec<_>>();
        ensure!(
            sequences.len() == usize::from(original_count),
            "FCT FEC mask popcount mismatch"
        );
        if self.completed.contains(&group_id) {
            return Ok(Vec::new());
        }
        if sequences
            .iter()
            .all(|sn| (sn.wrapping_sub(self.receive_next) as i32) < 0)
        {
            self.complete(group_id);
            return Ok(Vec::new());
        }
        self.prune(now);

        let group = self.groups.entry(group_id).or_insert_with(|| {
            let mut group = FecGroup {
                base_sequence,
                sequences: sequences.clone(),
                original_count,
                repair_count,
                shard_size,
                shards: BTreeMap::new(),
            };
            for (index, sequence) in sequences.iter().enumerate() {
                if let Some(original) = self.originals.get(sequence) {
                    insert_fec_original(&mut group, index as u8, &original.packet);
                }
            }
            self.group_order.push_back(group_id);
            group
        });
        ensure!(
            group.base_sequence == base_sequence
                && group.sequences == sequences
                && group.original_count == original_count
                && group.repair_count == repair_count
                && group.shard_size == shard_size,
            "inconsistent FCT FEC group"
        );
        group
            .shards
            .entry(original_count + repair_index)
            .or_insert_with(|| packet[FEC_HEADER..FEC_HEADER + shard_size].to_vec());
        let recovered = self.attempt(group_id)?;
        self.prune(now);
        Ok(recovered)
    }

    fn attempt(&mut self, group_id: u32) -> Result<Vec<Vec<u8>>> {
        let Some(group) = self.groups.get(&group_id) else {
            return Ok(Vec::new());
        };
        let source_count = (0..group.original_count)
            .filter(|index| group.shards.contains_key(index))
            .count();
        if source_count == usize::from(group.original_count) {
            self.complete(group_id);
            return Ok(Vec::new());
        }
        if group.shards.len() < usize::from(group.original_count) {
            return Ok(Vec::new());
        }
        let original_count = group.original_count;
        let repair_count = group.repair_count;
        let shard_size = group.shard_size;
        let sequences = group.sequences.clone();
        let existing = (0..original_count)
            .filter(|index| group.shards.contains_key(index))
            .collect::<HashSet<_>>();
        let mut selected = group
            .shards
            .iter()
            .take(usize::from(original_count))
            .map(|(index, shard)| (*index, shard.clone()))
            .collect::<Vec<_>>();
        self.complete(group_id);
        let originals =
            cm256_decode_originals(original_count, repair_count, shard_size, &mut selected)?;
        let mut recovered = Vec::new();
        for (index, sequence) in sequences.into_iter().enumerate() {
            if existing.contains(&(index as u8)) {
                continue;
            }
            let mut packet = originals[index].clone();
            ensure!(
                packet.len() >= FCT_KCP_HEADER,
                "recovered FCT shard is short"
            );
            ensure!(packet[..4] == magic(FCT_VERSION), "recovered FCT magic mismatch");
            ensure!(
                read_u32(&packet, 4)? == KCP_CONVERSATION,
                "recovered KCP conversation mismatch"
            );
            ensure!(
                packet[8] == CMD_PUSH,
                "recovered FCT command is not PUSH"
            );
            ensure!(
                read_u32(&packet, 16)? == sequence,
                "recovered FCT sequence mismatch"
            );
            let size = FCT_KCP_HEADER + read_u32(&packet, 24)? as usize;
            ensure!(
                size <= packet.len(),
                "recovered FCT payload is truncated"
            );
            packet.truncate(size);
            recovered.push(packet);
        }
        Ok(recovered)
    }

    fn complete(&mut self, group_id: u32) {
        self.groups.remove(&group_id);
        self.group_order.retain(|value| *value != group_id);
        if self.completed.insert(group_id) {
            self.completed_order.push_back(group_id);
        }
        while self.completed_order.len() > MAX_RECENT_PACKETS {
            if let Some(expired) = self.completed_order.pop_front() {
                self.completed.remove(&expired);
            }
        }
    }

    fn prune(&mut self, now: Instant) {
        while self.original_order.len() > MAX_RECENT_PACKETS
            || self.original_order.front().is_some_and(|sequence| {
                self.originals
                    .get(sequence)
                    .is_some_and(|packet| now.duration_since(packet.received_at) > FEC_RETENTION)
            })
        {
            if let Some(expired) = self.original_order.pop_front() {
                self.originals.remove(&expired);
            }
        }
        // Repair-group usefulness follows reliable receive progress, not
        // the independent one-second cache lifetime of original packets.
        let expired_groups = self
            .groups
            .iter()
            .filter_map(|(id, group)| {
                group
                    .sequences
                    .iter()
                    .all(|sn| (sn.wrapping_sub(self.receive_next) as i32) < 0)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for group_id in expired_groups {
            self.complete(group_id);
        }
        while self.group_order.len() > MAX_RECENT_PACKETS {
            if let Some(expired) = self.group_order.pop_front() {
                self.groups.remove(&expired);
            }
        }
    }
}

fn insert_fec_original(group: &mut FecGroup, index: u8, packet: &[u8]) {
    if packet.len() > group.shard_size {
        return;
    }
    group.shards.entry(index).or_insert_with(|| {
        let mut shard = vec![0_u8; group.shard_size];
        shard[..packet.len()].copy_from_slice(packet);
        shard
    });
}

#[derive(Default)]
struct RecoveryState {
    received_total: u32,
    received_resend: u32,
    received_fec: u32,
    received_fec_duplicate: u32,
    highest_normal_sequence: u32,
    missing_normal_sequences: u32,
    last_sent_highest: u32,
    last_sent_missing: u32,
    remote_timestamp: Option<u32>,
    remote_total: u32,
    remote_resend: u32,
    remote_fec: u32,
    remote_fec_duplicate: u32,
    remote_loss_ratio: Option<f64>,
}

impl RecoveryState {
    fn record_received(&mut self, sequence: u32, command: u8, recovered_by_fec: bool) {
        self.received_total = self.received_total.wrapping_add(1);
        if recovered_by_fec {
            self.received_fec = self.received_fec.wrapping_add(1);
        } else {
            match command {
                CMD_RESEND_PUSH => self.received_resend = self.received_resend.wrapping_add(1),
                CMD_FEC_DUPLICATE => {
                    self.received_fec_duplicate = self.received_fec_duplicate.wrapping_add(1)
                }
                CMD_PUSH => {
                    if sequence.wrapping_sub(self.highest_normal_sequence) as i32 > 0 {
                        self.missing_normal_sequences = self.missing_normal_sequences.wrapping_add(
                            sequence
                                .wrapping_sub(self.highest_normal_sequence)
                                .wrapping_sub(1),
                        );
                        self.highest_normal_sequence = sequence;
                    } else if self.missing_normal_sequences != 0 {
                        self.missing_normal_sequences -= 1;
                    }
                }
                _ => {}
            }
        }
    }

    fn encode_info(&mut self, version: u8, now_ms: u32) -> Vec<u8> {
        let highest_delta = self
            .highest_normal_sequence
            .wrapping_sub(self.last_sent_highest);
        let missing_delta = self
            .missing_normal_sequences
            .wrapping_sub(self.last_sent_missing);
        self.last_sent_highest = self.highest_normal_sequence;
        self.last_sent_missing = self.missing_normal_sequences;
        let mut packet = Vec::with_capacity(RECOVERY_INFO_SIZE);
        packet.extend_from_slice(&magic(version));
        packet.extend_from_slice(&KCP_CONVERSATION.to_le_bytes());
        packet.push(CMD_RECOVERY_INFO);
        for value in [
            now_ms,
            self.received_total,
            self.received_resend,
            self.received_fec,
            self.received_fec_duplicate,
            highest_delta,
            missing_delta,
        ] {
            packet.extend_from_slice(&value.to_le_bytes());
        }
        packet
    }

    fn receive_info(&mut self, packet: &[u8]) -> Result<()> {
        ensure!(
            packet.len() >= RECOVERY_INFO_SIZE,
            "short FCT recovery-info packet"
        );
        let timestamp = read_u32(packet, 9)?;
        if self
            .remote_timestamp
            .is_some_and(|previous| timestamp.wrapping_sub(previous) as i32 <= 0)
        {
            return Ok(());
        }
        let total = read_u32(packet, 13)?;
        let resend = read_u32(packet, 17)?;
        let fec = read_u32(packet, 21)?;
        let fec_duplicate = read_u32(packet, 25)?;
        let total_delta = total.wrapping_sub(self.remote_total);
        if total_delta != 0 {
            let recovered_delta = resend
                .wrapping_sub(self.remote_resend)
                .wrapping_add(fec.wrapping_sub(self.remote_fec))
                .wrapping_add(fec_duplicate.wrapping_sub(self.remote_fec_duplicate));
            let sample = f64::from(recovered_delta) / f64::from(total_delta);
            if (0.0..=1.0).contains(&sample) {
                self.remote_loss_ratio = Some(
                    self.remote_loss_ratio
                        .map_or(sample * 0.25, |previous| (previous * 3.0 + sample) * 0.25),
                );
            }
        }
        self.remote_timestamp = Some(timestamp);
        self.remote_total = total;
        self.remote_resend = resend;
        self.remote_fec = fec;
        self.remote_fec_duplicate = fec_duplicate;
        Ok(())
    }
}
