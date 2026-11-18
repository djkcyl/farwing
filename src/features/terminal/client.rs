//! 主控端到对端终端通道的连接。
//!
//! 一条 `kind = 8` 的会话只承载终端。被控端把 `BINARY_DATA_CHANNEL` 交给
//! TERM 帧处理器，所以这里也直接接管同一条数据通道：会话列表、开 shell、
//! 标准输入输出都在这条通道上排队。

use super::protocol::{Frame, FrameType, MAGIC};
use crate::account::api::DeviceInfo;
use crate::account::client::AuthenticatedClient;
use crate::media::ConnectionMediaOptions;
use crate::session::controller::ControllerConnection;
use anyhow::{Context, Result, bail, ensure};
use bytes::Bytes;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;
use webrtc::data_channel::data_channel_state::RTCDataChannelState;

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

/// 终端连接的数据通道与帧队列。
///
/// `NativePeer` 在创建本地数据通道时把 BINARY 交过来；这里只接收以 `TERM`
/// 开头的消息，其他 protobuf 回复（例如环境检查）与本功能无关。
#[derive(Default)]
pub(crate) struct Transport {
    channel: Mutex<Option<Weak<RTCDataChannel>>>,
    sender: Arc<Mutex<Option<mpsc::UnboundedSender<Frame>>>>,
    receiver: Mutex<Option<mpsc::UnboundedReceiver<Frame>>>,
}

impl Transport {
    pub(crate) fn bind(self: &Arc<Self>, channel: &Arc<RTCDataChannel>) {
        *lock(&self.channel) = Some(Arc::downgrade(channel));
        let (sender, receiver) = mpsc::unbounded_channel();
        *lock(&self.sender) = Some(sender.clone());
        *lock(&self.receiver) = Some(receiver);
        let sender = Arc::clone(&self.sender);
        channel.on_message(Box::new(move |message| {
            if message.data.starts_with(MAGIC) {
                if let Some(frame) = Frame::parse(&message.data) {
                    let _ = lock(&sender).as_ref().map(|sender| sender.send(frame));
                } else {
                    tracing::warn!(bytes = message.data.len(), "terminal frame unparsed");
                }
            }
            Box::pin(async {})
        }));
        let sender = Arc::clone(&self.sender);
        channel.on_close(Box::new(move || {
            // 丢掉发送端让接收方看到 None，服务据此结束，而不是一直等。
            *lock(&sender) = None;
            Box::pin(async {})
        }));
    }

    fn take_frames(&self) -> Option<mpsc::UnboundedReceiver<Frame>> {
        lock(&self.receiver).take()
    }

    fn channel(&self) -> Option<Arc<RTCDataChannel>> {
        lock(&self.channel).as_ref().and_then(Weak::upgrade)
    }

    pub(crate) fn close(&self) {
        *lock(&self.sender) = None;
    }

    async fn wait_open(&self, cancel: &CancellationToken) -> Result<Arc<RTCDataChannel>> {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(20);
        loop {
            if let Some(channel) = self.channel()
                && channel.ready_state() == RTCDataChannelState::Open
            {
                return Ok(channel);
            }
            if cancel.is_cancelled() {
                bail!("终端连接已取消");
            }
            if tokio::time::Instant::now() >= deadline {
                bail!("终端数据通道未建立");
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
}

/// 一条已经握手完成的终端连接。
pub(crate) struct Connection {
    inner: ControllerConnection,
    transport: Arc<Transport>,
    frames: mpsc::UnboundedReceiver<Frame>,
}

impl Connection {
    pub(crate) async fn connect(
        client: &AuthenticatedClient,
        device: &DeviceInfo,
        options: ConnectionMediaOptions,
        cancel: &CancellationToken,
        takeover: Option<crate::session::controller::takeover::Approval>,
    ) -> Result<Self> {
        let current = validate_target(client, device).await?;
        let policy = client
            .feature_catalog()
            .policy(current.platform, &current.version_name);
        let inner = ControllerConnection::connect_terminal(
            client, &current, policy, options, cancel, takeover,
        )
        .await?;
        let transport = inner.terminal_transport();
        let frames = transport
            .take_frames()
            .context("终端通道已经绑定过另一条连接")?;
        transport.wait_open(cancel).await?;
        Ok(Self {
            inner,
            transport,
            frames,
        })
    }

    pub(crate) async fn send(&self, kind: FrameType, session: u32, payload: &[u8]) -> Result<()> {
        let channel = self.transport.channel().context("终端连接已断开")?;
        ensure!(
            channel.ready_state() == RTCDataChannelState::Open,
            "终端数据通道已关闭"
        );
        let bytes = Bytes::from(Frame::encode(kind, session, payload));
        tokio::time::timeout(Duration::from_secs(10), channel.send(&bytes))
            .await
            .context("发送终端消息超时")??;
        Ok(())
    }

    pub(crate) async fn next_frame(&mut self) -> Option<Frame> {
        self.frames.recv().await
    }

    pub(crate) async fn close(self) -> Result<()> {
        self.inner.close().await
    }
}

/// 从账号里取一台可连接的设备。`device_id` 与别名二选一；别名必须唯一，
/// 免得两台同名设备里选错。
pub(crate) async fn resolve_device(
    client: &AuthenticatedClient,
    device: Option<&str>,
    device_id: Option<&str>,
) -> Result<DeviceInfo> {
    ensure!(
        device.is_some() != device_id.is_some(),
        "请用设备名称或 --device-id 指定一台设备"
    );
    let list = client.list_devices().await?;
    if let Some(id) = device_id {
        ensure!(list.current_device.device_id != id, "不能连接本机虚拟设备");
    }
    let mut matches = list.my_binded_devices.iter().filter(|candidate| {
        device_id.map_or(candidate.alias == device.unwrap_or_default(), |id| {
            candidate.device_id == id
        })
    });
    let first = matches.next().context("账号里没有这台设备")?;
    ensure!(
        matches.next().is_none(),
        "设备名称不唯一，请改用 --device-id"
    );
    first.validated_device_id()?;
    ensure!(
        matches!(first.platform, 1 | 4)
            && first.is_connected()
            && first.controllable
            && first.controlled_support,
        "设备当前不可连接"
    );
    Ok(first.clone())
}

async fn validate_target(client: &AuthenticatedClient, device: &DeviceInfo) -> Result<DeviceInfo> {
    let list = client.list_devices().await?;
    let current = list
        .my_binded_devices
        .iter()
        .find(|candidate| candidate.device_id == device.device_id)
        .context("设备已不在当前账号中")?;
    current.validated_device_id()?;
    ensure!(
        matches!(current.platform, 1 | 4)
            && current.device_id != client.device_id()
            && current.is_connected()
            && current.controllable
            && current.controlled_support,
        "设备当前不可连接"
    );
    Ok(current.clone())
}
