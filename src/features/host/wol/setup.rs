//! On-demand local setup. Cloud permission and relay permission are independent.
use crate::{
    platform::windows::{components, wol as network},
    session::host_client::HostClient,
};
use anyhow::{Context, Result, ensure};
pub(crate) use network::setup::{Adapter, AdapterKey};
use serde::{Deserialize, Serialize};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Status {
    pub busy: bool,
    pub checked: bool,
    pub enabled: Option<bool>,
    pub adapters: Vec<Adapter>,
    pub default_index: Option<u32>,
    pub unattended: bool,
    pub message: String,
    pub error: Option<String>,
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) enum Action {
    Check,
    Configure(AdapterKey),
    Enable {
        adapter: AdapterKey,
        confirmed: bool,
    },
    Disable,
}
#[derive(Default)]
struct Control {
    sender: Mutex<Option<tokio::sync::mpsc::Sender<Action>>>,
    busy: AtomicBool,
    enabled: AtomicBool,
}
#[derive(Clone, Default)]
pub(crate) struct Handle(Arc<Control>);
impl Handle {
    pub fn enabled(&self) -> bool {
        self.0.enabled.load(Ordering::Acquire)
    }
    pub fn act(&self, action: Action) -> Result<()> {
        if let Action::Configure(key) | Action::Enable { adapter: key, .. } = &action {
            key.validate()?;
        }
        ensure!(
            !self.0.busy.swap(true, Ordering::AcqRel),
            "远程开机设置正在处理，请等待完成"
        );
        let result = super::super::lock(&self.0.sender)
            .as_ref()
            .context("本机账号后台尚未就绪")
            .and_then(|tx| {
                tx.try_send(action)
                    .map_err(|_| anyhow::anyhow!("远程开机设置任务不可用"))
            });
        if result.is_err() {
            self.0.busy.store(false, Ordering::Release);
        }
        result
    }
}
pub(crate) struct Running {
    task: tokio::task::JoinHandle<()>,
    stop: CancellationToken,
    handle: Handle,
}
impl Running {
    pub fn start(client: HostClient) -> Option<Self> {
        if client.is_guest() {
            return None;
        }
        let handle = client.host.wol_setup.clone();
        let (tx, mut rx) = tokio::sync::mpsc::channel(1);
        *super::super::lock(&handle.0.sender) = Some(tx);
        let stop = client.ended().child_token();
        let token = stop.clone();
        let worker = handle.clone();
        let task = tokio::spawn(async move {
            let mut status = Status::default();
            match client.wol_enabled().await {
                Ok(enabled) => {
                    status.enabled = Some(enabled);
                    worker.0.enabled.store(enabled, Ordering::Release);
                    client.host.wol.change();
                }
                Err(e) => {
                    status.error = Some(format!("读取远程开机状态失败：{e}"));
                }
            }
            client.host.wol_setup_status(status.clone());
            loop {
                let action = tokio::select! {biased;_=token.cancelled()=>break,v=rx.recv()=>match v {Some(v)=>v,None=>break}};
                status.busy = true;
                status.error = None;
                status.message = "正在处理远程开机设置…".into();
                client.host.wol_setup_status(status.clone());
                let result = perform(&client, action, &mut status, &token).await;
                let enabled = status.enabled == Some(true);
                if worker.0.enabled.swap(enabled, Ordering::AcqRel) != enabled {
                    client.host.wol.change();
                }
                status.busy = false;
                if let Err(e) = result {
                    status.error = Some(format!("{e:#}"));
                    status.message = "操作未完成，请检查当前状态".into();
                }
                client.host.wol_setup_status(status.clone());
                worker.0.busy.store(false, Ordering::Release);
            }
        });
        Some(Self { task, stop, handle })
    }
}
impl Drop for Running {
    fn drop(&mut self) {
        self.stop.cancel();
        self.task.abort();
        super::super::lock(&self.handle.0.sender).take();
        self.handle.0.busy.store(false, Ordering::Release);
        self.handle.0.enabled.store(false, Ordering::Release);
    }
}
async fn hardware(
    key: Option<AdapterKey>,
    configure: bool,
    token: CancellationToken,
) -> Result<Vec<Adapter>> {
    tokio::task::spawn_blocking(move || {
        if configure {
            network::setup::configure(key.as_ref().context("未选择网卡")?, &token)
        } else {
            network::setup::inspect(key.as_ref(), false, &token)
        }
    })
    .await
    .context("网卡设置任务中断")?
}
async fn inventory() -> Result<Vec<network::Interface>> {
    tokio::task::spawn_blocking(network::inventory).await?
}
fn active(client: &HostClient, stop: &CancellationToken) -> Result<()> {
    ensure!(
        !stop.is_cancelled() && client.is_active() && !client.is_guest(),
        "账号已结束"
    );
    Ok(())
}
async fn perform(
    client: &HostClient,
    action: Action,
    status: &mut Status,
    stop: &CancellationToken,
) -> Result<()> {
    active(client, stop)?;
    match action {
        Action::Check => {
            // Independent results: a cloud error must not hide usable hardware diagnostics.
            let (hardware, cloud, network) = tokio::join!(
                hardware(None, false, stop.clone()),
                client.wol_enabled(),
                inventory()
            );
            active(client, stop)?;
            status.adapters = hardware?;
            status.default_index = network.ok().and_then(|rows| {
                rows.into_iter()
                    .find(|r| r.default_metric.is_some())
                    .map(|r| r.index)
            });
            status.unattended =
                components::status(components::Kind::HostService).is_ok_and(|s| s.ready);
            status.checked = true;
            status.enabled = None;
            status.enabled = Some(cloud?);
            status.message = "检查完成".into();
        }
        Action::Configure(key) => {
            ensure!(
                status.checked && status.adapters.iter().any(|a| a.key == key),
                "请先检查并选择网卡"
            );
            let rows = hardware(Some(key.clone()), true, stop.clone()).await?;
            active(client, stop)?;
            let adapter = rows.into_iter().next().context("目标网卡已移除")?;
            let partial = !adapter.errors.is_empty();
            status.adapters.retain(|a| a.key != key);
            status.adapters.push(adapter);
            status.message = if partial {
                "部分网卡设置未完成；请查看各项结果"
            } else {
                "已配置并复查；部分驱动设置需下次重启生效，未重启网卡"
            }
            .into();
        }
        Action::Enable {
            adapter: key,
            confirmed,
        } => {
            ensure!(confirmed, "请先确认网卡与 BIOS 已配置");
            ensure!(client.host.allowed(), "请先开启允许被控");
            ensure!(
                status.checked && status.adapters.iter().any(|a| a.key == key),
                "请先检查并选择网卡"
            );
            // Revalidate physical identity and current network just before any cloud write.
            let rows = hardware(Some(key.clone()), false, stop.clone()).await?;
            let adapter = rows.into_iter().next().context("目标网卡已移除")?;
            ensure!(adapter.connected, "选中的有线网卡尚未连接");
            let info = inventory()
                .await?
                .into_iter()
                .find(|i| {
                    i.index == adapter.index
                        && i.mac
                            .iter()
                            .map(|v| format!("{v:02X}"))
                            .collect::<String>()
                            .eq_ignore_ascii_case(&key.mac)
                })
                .context("网卡没有可登记的IPv4网络")?;
            // Relay registration also uses the primary route; avoid two independent owners
            // repeatedly replacing a device's LAN identity with different adapters.
            ensure!(
                info.default_metric.is_some(),
                "请选择当前默认网络的有线网卡"
            );
            ensure!(
                inventory()
                    .await?
                    .first()
                    .is_some_and(|i| i.index == info.index),
                "默认网络已变化，请重新检查"
            );
            active(client, stop)?;
            client
                .wol_info(info.registration())
                .await
                .context("网络登记失败，未开启远程开机")?;
            active(client, stop)?;
            ensure!(client.host.allowed(), "允许被控已关闭");
            status.enabled = None;
            let saved = client.set_wol_enabled(true).await;
            let readback = client.wol_enabled().await;
            status.enabled = readback.as_ref().ok().copied();
            saved.context("开启请求未确认；不会自动重发，请重新检查云端状态")?;
            ensure!(
                readback.context("服务器已接受设置，但状态回读失败；请重新检查")?,
                "服务器已接受设置，但回读仍为关闭；未确认生效"
            );
            active(client, stop)?;
            // Only the validated portable account owner installs login startup,
            // after cloud confirmation. A rejected/stale GUI request has no side effects.
            if !crate::platform::windows::host_service::resident::is_owner() {
                crate::platform::windows::host_service::startup::set_image(
                    true,
                    &std::env::current_exe()?,
                )
                .context("云端开机许可已开启，但登录自启动设置失败")?;
            }
            client.host.wol.refresh();
            status.message = "已开启本设备远程开机；硬件实际唤醒仍取决于网卡和 BIOS".into();
        }
        Action::Disable => {
            status.enabled = None;
            let saved = client.set_wol_enabled(false).await;
            let readback = client.wol_enabled().await;
            status.enabled = readback.as_ref().ok().copied();
            saved.context("关闭请求未确认；不会自动重发，请重新检查云端状态")?;
            ensure!(
                !readback.context("服务器已接受设置，但状态回读失败；请重新检查")?,
                "服务器已接受设置，但回读仍为开启；未确认生效"
            );
            client.host.wol.refresh();
            status.message = "已关闭本设备远程开机；网卡配置保持不变".into();
        }
    }
    Ok(())
}
