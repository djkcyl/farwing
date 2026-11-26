//! Current controller TEXT/FILE binding; bulk file traffic has a separate writer.
use super::*;
use std::sync::Weak;
use webrtc::data_channel::{RTCDataChannel, data_channel_state::RTCDataChannelState};
#[derive(Clone, Default)]
pub(crate) struct Controller(Arc<Mutex<Binding>>, Arc<AtomicU64>, Arc<AtomicU64>);
#[derive(Default)]
struct Binding {
    text: Weak<RTCDataChannel>,
    file: Weak<RTCDataChannel>,
    endpoint: Option<Endpoint>,
    reverse: Vec<(u64, Arc<Ticket>)>,
    active: Weak<Ticket>,
    reverse_owner: Option<u64>,
    input_flags: Option<(Arc<AtomicU64>, Arc<AtomicU64>)>,
    workers: Vec<tokio::task::JoinHandle<()>>,
    enabled: bool,
    seen_native: bool,
    error: Option<String>,
}
impl Binding {
    fn stop(&mut self) {
        self.reverse_owner = None;
        for (_, ticket) in self.reverse.drain(..) {
            ticket.cancel();
        }
        if let Some(endpoint) = self.endpoint.take() {
            endpoint.close();
        }
        if let Some((pointer, held)) = &self.input_flags {
            pointer.store(0, Ordering::Release);
            held.store(0, Ordering::Release);
        }
        for worker in self.workers.drain(..) {
            worker.abort();
        }
    }
}
impl Drop for Binding {
    fn drop(&mut self) {
        self.stop();
    }
}
impl Controller {
    pub fn bind(&self, channel: &Arc<RTCDataChannel>) {
        let mut binding = lock(&self.0);
        binding.input_flags = Some((self.1.clone(), self.2.clone()));
        let slot = match channel.label() {
            "TEXT_DATA_CHANNEL" => &mut binding.text,
            "FILE_DATA_CHANNEL" => &mut binding.file,
            _ => return,
        };
        let replaced = slot.upgrade().is_some() && !slot.ptr_eq(&Arc::downgrade(channel));
        *slot = Arc::downgrade(channel);
        if replaced {
            binding.stop();
            binding.seen_native = false;
            binding.error = None;
        }
        if binding.endpoint.is_some() {
            return;
        }
        let (Some(text), Some(file)) = (binding.text.upgrade(), binding.file.upgrade()) else {
            return;
        };
        let (endpoint, mut output) =
            Endpoint::with_input_flags(Role::Controller, self.1.clone(), self.2.clone());
        endpoint.enable(binding.enabled);
        let (control_tx, control_rx) = mpsc::channel(16);
        let (bulk_tx, bulk_rx) = mpsc::channel(8);
        let state = Arc::downgrade(&self.0);
        let active = endpoint.clone();
        binding.workers.push(tokio::spawn(async move {
            while let Some(packet) = output.recv().await {
                let sender = if packet.bulk { &bulk_tx } else { &control_tx };
                if sender.try_send(packet.data).is_err() {
                    fail(&state, &active, "拖放发送队列已满".into());
                    break;
                }
            }
        }));
        binding.workers.push(tokio::spawn(write(
            text,
            control_rx,
            false,
            Arc::downgrade(&self.0),
            endpoint.clone(),
        )));
        binding.workers.push(tokio::spawn(write(
            file,
            bulk_rx,
            true,
            Arc::downgrade(&self.0),
            endpoint.clone(),
        )));
        binding.endpoint = Some(endpoint);
    }
    pub fn enable(&self, enabled: bool) {
        let mut binding = lock(&self.0);
        binding.enabled = enabled;
        if let Some(endpoint) = &binding.endpoint {
            endpoint.enable(enabled && binding.error.is_none());
        }
    }
    pub fn close(&self, channel: &Arc<RTCDataChannel>) {
        let mut binding = lock(&self.0);
        if binding.text.ptr_eq(&Arc::downgrade(channel))
            || binding.file.ptr_eq(&Arc::downgrade(channel))
        {
            binding.stop();
            binding.text = Weak::new();
            binding.file = Weak::new();
            binding.seen_native = false;
            binding.error = None;
        }
    }
    pub fn receive(&self, channel: &Arc<RTCDataChannel>, bytes: &[u8], bulk: bool) -> Result<()> {
        let packet = wire::decode(bytes)?.ok_or_else(|| anyhow::anyhow!("拖放消息标记缺失"))?;
        ensure!(
            !bulk || matches!(&packet.payload, Some(Payload::Data(_))),
            "FILE通道不能承载拖放控制"
        );
        let mut binding = lock(&self.0);
        let current = if bulk { &binding.file } else { &binding.text };
        if !current.ptr_eq(&Arc::downgrade(channel)) {
            return Ok(());
        }
        if matches!(&packet.payload, Some(Payload::Hello(1))) {
            if !binding.seen_native {
                tracing::info!("native file drag capability negotiated");
            }
            binding.seen_native = true;
        }
        if let Some(endpoint) = &binding.endpoint {
            endpoint.receive(bytes)?;
        }
        Ok(())
    }
    pub fn handoff_pending(&self) -> bool {
        self.2.load(Ordering::Acquire) != 0
    }
    pub fn probe(
        &self,
        owner: u64,
        point: Point,
        input: crate::features::remote_input::RemoteInput,
    ) -> Result<()> {
        ensure!(self.available(), "当前连接没有原生拖出能力");
        let mut binding = lock(&self.0);
        ensure!(binding.reverse.len() < 4, "正在处理的拖出过多");
        let endpoint = binding
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("拖放连接已关闭"))?;
        let hold = if endpoint.shared.return_capable.load(Ordering::Acquire) {
            Some(input.hold_drag(owner)?)
        } else {
            None
        };
        let current = hold
            .clone()
            .map(|hold| Arc::new(move || hold.current()) as Arc<dyn Fn() -> bool + Send + Sync>);
        let resume = hold.clone().map(|hold| {
            Arc::new(move |point: Point, left| hold.resume(point.screen, point.x, point.y, left))
                as Arc<dyn Fn(Point, bool) -> bool + Send + Sync>
        });
        let wake_input = input.clone();
        let release = Arc::new(move || {
            if let Some(hold) = &hold {
                hold.cancel();
            } else {
                input.pause_owner(owner);
                input.repaint();
            }
        });
        let ticket = endpoint.probe(
            point,
            ReleaseSource {
                release: Some(release),
                resume,
                current,
                wake: Some(Arc::new(move || {
                    native::wake_viewer(owner);
                    wake_input.repaint();
                })),
            },
        )?;
        binding.reverse_owner = Some(owner);
        binding.active = Arc::downgrade(&ticket);
        binding.reverse.push((owner, ticket));
        Ok(())
    }
    pub fn take_reverse(&self, owner: u64) -> Vec<Arc<Ticket>> {
        let mut binding = lock(&self.0);
        let mut result = Vec::new();
        binding.reverse.retain(|(window, ticket)| {
            if *window == owner {
                result.push(ticket.clone());
                false
            } else {
                true
            }
        });
        result
    }
    pub fn interactive(&self) -> bool {
        self.1.load(Ordering::Acquire) != 0
    }
    #[cfg_attr(
        not(windows),
        allow(dead_code, reason = "Only the Windows viewer offers the drag return.")
    )]
    pub fn returning(
        &self,
        identity: native::appearance::Identity,
        owner: u64,
    ) -> Option<Arc<Ticket>> {
        let binding = lock(&self.0);
        let endpoint = binding.endpoint.as_ref()?;
        if binding.reverse_owner != Some(owner) {
            return None;
        }
        if !endpoint.available() || !endpoint.shared.return_capable.load(Ordering::Acquire) {
            return None;
        }
        binding.active.upgrade().filter(|ticket| {
            ticket.token == identity.token
                && ticket.id == identity.drag
                && ticket.id == self.1.load(Ordering::Acquire)
                && ticket.snapshot().stage == Stage::Dragging
        })
    }
    pub fn cancel_gesture(&self, preparing_only: bool) {
        let binding = lock(&self.0);
        if let Some(ticket) = binding.active.upgrade()
            && ticket.id == self.1.load(Ordering::Acquire)
            && (!preparing_only || ticket.snapshot().stage == Stage::Preparing)
        {
            ticket.cancel();
        }
    }
    pub fn is_native(&self) -> bool {
        lock(&self.0).seen_native
    }
    pub fn available(&self) -> bool {
        let binding = lock(&self.0);
        binding.error.is_none()
            && binding.endpoint.as_ref().is_some_and(Endpoint::available)
            && [&binding.text, &binding.file].iter().all(|c| {
                c.upgrade()
                    .is_some_and(|c| c.ready_state() == RTCDataChannelState::Open)
            })
    }
    pub fn begin(&self, paths: Vec<PathBuf>, point: Point) -> Result<Arc<Ticket>> {
        ensure!(self.available(), "原生拖放尚未就绪");
        let mut binding = lock(&self.0);
        let ticket = binding
            .endpoint
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("拖放连接已关闭"))?
            .begin(paths, point)?;
        binding.reverse_owner = None;
        binding.active = Arc::downgrade(&ticket);
        Ok(ticket)
    }
}
fn fail(state: &Weak<Mutex<Binding>>, endpoint: &Endpoint, error: String) {
    endpoint.enable(false);
    if let Some(state) = state.upgrade() {
        lock(&state).error = Some(error);
    }
}
async fn write(
    channel: Arc<RTCDataChannel>,
    mut input: mpsc::Receiver<Vec<u8>>,
    bulk: bool,
    state: Weak<Mutex<Binding>>,
    endpoint: Endpoint,
) {
    let stop = endpoint.stop_token();
    loop {
        let data = tokio::select! { _=stop.cancelled()=>break, v=input.recv()=>match v {Some(v)=>v,None=>break} };
        let sending = tokio::time::timeout(Duration::from_secs(30), async {
            while channel.buffered_amount().await + data.len() > 2 * 1024 * 1024 {
                tokio::select! { _=stop.cancelled()=>anyhow::bail!("拖放已结束"), _=tokio::time::sleep(Duration::from_millis(5))=>{} }
            }
            ensure!(
                channel.ready_state() == RTCDataChannelState::Open,
                "拖放数据通道已关闭"
            );
            if bulk {
                channel.send(&bytes::Bytes::from(data)).await?;
            } else {
                channel.send_text_bytes(&bytes::Bytes::from(data)).await?;
            }
            Ok::<_, anyhow::Error>(())
        });
        // Closing this endpoint also withdraws a send waiting for SCTP capacity.
        // Already admitted messages remain whole and are not replayed.
        let result = tokio::select! { biased; _=stop.cancelled()=>break, result=sending=>result };
        if !matches!(result, Ok(Ok(()))) {
            fail(&state, &endpoint, "拖放发送失败，未自动重试".into());
            break;
        }
    }
}
