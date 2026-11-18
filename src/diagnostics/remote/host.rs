use super::*;
use crate::features::host::Lease;
use sha2::{Digest, Sha256};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

pub(crate) fn bind(
    channel: Arc<RTCDataChannel>,
    lease: Lease,
    connected: Arc<AtomicBool>,
    stop: CancellationToken,
) -> CancellationToken {
    let cancel = stop.child_token();
    let (tx, mut rx) = mpsc::channel(8);
    let input_cancel = cancel.clone();
    channel.on_message(Box::new(move |message| {
        if let Ok(packet) = decode(&message.data) {
            if tx.try_send(packet).is_err() {
                input_cancel.cancel();
            }
        } else {
            input_cancel.cancel();
        }
        Box::pin(async {})
    }));
    let close = cancel.clone();
    channel.on_close(Box::new(move || {
        close.cancel();
        Box::pin(async {})
    }));
    let weak = Arc::downgrade(&channel);
    let worker_stop = cancel.clone();
    let activity = crate::platform::host_service::activity::Work::new();
    tokio::spawn(async move {
        let _activity = activity;
        let cancel = worker_stop;
        let mut last_allowed = None;
        let mut policy_tick = tokio::time::interval(std::time::Duration::from_millis(200));
        loop {
            let packet = tokio::select! {
                _=cancel.cancelled()=>break,
                _=policy_tick.tick()=>{
                    let allowed=allowed(&lease,&connected);
                    if let Some(channel)=weak.upgrade() {
                        if channel.ready_state()==webrtc::data_channel::data_channel_state::RTCDataChannelState::Open && last_allowed!=Some(allowed) {
                            if send(&channel,[0;16],Message::Hello{allowed}).await.is_err(){break}
                            last_allowed=Some(allowed);
                        }
                    } else {break}
                    continue;
                },
                packet=rx.recv()=>match packet {Some(p)=>p,None=>break}
            };
            let Packet::Control(id, Message::Request) = packet else {
                continue;
            };
            let Some(channel) = weak.upgrade() else { break };
            if !allowed(&lease, &connected) {
                let _ = send(
                    &channel,
                    id,
                    Message::Error {
                        message: "远端未允许文件与诊断包访问".into(),
                    },
                )
                .await;
                continue;
            }
            let result = {
                let work = transfer(&channel, id, &lease, &connected, &cancel, &mut rx);
                tokio::pin!(work);
                loop {
                    tokio::select! {
                        _=cancel.cancelled()=>break Err(anyhow::anyhow!("诊断连接已关闭")),
                        _=policy_tick.tick()=>if !allowed(&lease,&connected) {break Err(anyhow::anyhow!("诊断访问权限已撤销或连接已中断"))},
                        result=&mut work=>break result,
                    }
                }
            };
            if let Err(error) = result {
                tracing::debug!(%error,"remote diagnostic export ended");
                if !cancel.is_cancelled() {
                    let _ = send(
                        &channel,
                        id,
                        Message::Error {
                            message: format!("{error:#}"),
                        },
                    )
                    .await;
                }
            }
        }
    });
    cancel
}
fn allowed(lease: &Lease, connected: &AtomicBool) -> bool {
    lease.file_access() && connected.load(Ordering::Acquire)
}
fn check(lease: &Lease, connected: &AtomicBool, cancel: &CancellationToken) -> Result<()> {
    ensure!(!cancel.is_cancelled(), "诊断连接已关闭");
    ensure!(allowed(lease, connected), "诊断访问权限已撤销或连接已中断");
    Ok(())
}
async fn transfer(
    channel: &RTCDataChannel,
    id: Id,
    lease: &Lease,
    connected: &AtomicBool,
    cancel: &CancellationToken,
    rx: &mut mpsc::Receiver<Packet>,
) -> Result<()> {
    check(lease, connected, cancel)?;
    let task = bundle::Task::remote(lease.diagnostic_input())?;
    let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
    let exported = loop {
        check(lease, connected, cancel)?;
        match task.receiver.try_recv() {
            Ok(result) => break result.map_err(anyhow::Error::msg)?,
            Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                anyhow::bail!("远端诊断导出任务已中断")
            }
            Err(_) => {}
        }
        tokio::select! {
            _=cancel.cancelled()=>anyhow::bail!("诊断连接已关闭"),
            _=tick.tick()=>send(channel,id,Message::Progress(task.progress())).await?,
            packet=rx.recv()=>match packet {
                Some(Packet::Control(other,Message::Cancel)) if other==id=>anyhow::bail!("已取消获取远端诊断包"),
                Some(_)=>anyhow::bail!("诊断请求顺序错误"),
                None=>anyhow::bail!("诊断通道已关闭"),
            }
        }
    };
    // Exported owns this temporary archive and deletes it on every exit, including
    // disconnect, revoked permissions, checksum/read errors and successful delivery.
    check(lease, connected, cancel)?;
    let mut file = tokio::fs::File::open(&exported.path).await?;
    let size = file.metadata().await?.len();
    ensure!(size > 0 && size <= bundle::ARCHIVE_LIMIT, "诊断包大小无效");
    let mut digest = Sha256::new();
    let mut buffer = vec![0; CHUNK];
    loop {
        check(lease, connected, cancel)?;
        let count = file.read(&mut buffer).await?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    use tokio::io::AsyncSeekExt;
    file.seek(std::io::SeekFrom::Start(0)).await?;
    send(
        channel,
        id,
        Message::Begin {
            size,
            sha256: format!("{:x}", digest.finalize()),
            logs: exported.logs,
            warnings: exported.warnings,
        },
    )
    .await?;
    let mut offset = 0;
    loop {
        // Stop-and-wait bounds both peers' memory and applies disk backpressure.
        // The ACK is sent only after the receiver has written the exact offset.
        let deadline = tokio::time::Instant::now() + TIMEOUT;
        let ack = loop {
            check(lease, connected, cancel)?;
            tokio::select! {
                _=tokio::time::sleep_until(deadline)=>anyhow::bail!("诊断传输确认超时"),
                p=next(rx,cancel)=>break p?,
                _=tokio::time::sleep(std::time::Duration::from_millis(200))=>{},
            }
        };
        match ack {
            Packet::Control(other, Message::Ack { offset: got })
                if other == id && got == offset => {}
            Packet::Control(other, Message::Cancel) if other == id => {
                anyhow::bail!("已取消获取远端诊断包")
            }
            _ => anyhow::bail!("诊断传输确认无效"),
        }
        check(lease, connected, cancel)?;
        if offset == size {
            send(channel, id, Message::Finish).await?;
            return Ok(());
        }
        let count = file.read(&mut buffer).await?;
        ensure!(
            count > 0 && offset + count as u64 <= size,
            "诊断包读取长度不一致"
        );
        send_data(channel, id, offset, &buffer[..count]).await?;
        offset += count as u64;
    }
}
