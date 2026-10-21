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
use crate::i18n::t;

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
            "{}", t().features.wol_setup_in_progress
        );
        let result = super::super::lock(&self.0.sender)
            .as_ref()
            .context(t().features.wol_account_service_not_ready)
            .and_then(|tx| {
                tx.try_send(action)
                    .map_err(|_| anyhow::anyhow!(t().features.wol_setup_unavailable))
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
                    status.error = Some((t().features.wol_status_read_failed)(&e.to_string()));
                }
            }
            client.host.wol_setup_status(status.clone());
            loop {
                let action = tokio::select! {biased;_=token.cancelled()=>break,v=rx.recv()=>match v {Some(v)=>v,None=>break}};
                status.busy = true;
                status.error = None;
                status.message = t().features.wol_setup_working.into();
                client.host.wol_setup_status(status.clone());
                let result = perform(&client, action, &mut status, &token).await;
                let enabled = status.enabled == Some(true);
                if worker.0.enabled.swap(enabled, Ordering::AcqRel) != enabled {
                    client.host.wol.change();
                }
                status.busy = false;
                if let Err(e) = result {
                    status.error = Some(format!("{e:#}"));
                    status.message = t().features.wol_operation_incomplete.into();
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
            network::setup::configure(key.as_ref().context(t().features.wol_no_adapter_selected)?, &token)
        } else {
            network::setup::inspect(key.as_ref(), false, &token)
        }
    })
    .await
    .context(t().features.wol_adapter_setup_interrupted)?
}
async fn inventory() -> Result<Vec<network::Interface>> {
    tokio::task::spawn_blocking(network::inventory).await?
}
fn active(client: &HostClient, stop: &CancellationToken) -> Result<()> {
    ensure!(
        !stop.is_cancelled() && client.is_active() && !client.is_guest(),
        "{}", t().features.host_account_session_ended
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
            status.message = t().features.wol_check_done.into();
        }
        Action::Configure(key) => {
            ensure!(
                status.checked && status.adapters.iter().any(|a| a.key == key),
                "{}", t().features.wol_no_adapter_selected
            );
            let rows = hardware(Some(key.clone()), true, stop.clone()).await?;
            active(client, stop)?;
            let adapter = rows.into_iter().next().context(t().features.wol_adapter_removed)?;
            let partial = !adapter.errors.is_empty();
            status.adapters.retain(|a| a.key != key);
            status.adapters.push(adapter);
            status.message = if partial {
                t().features.wol_adapter_partial
            } else {
                t().features.wol_adapter_configured
            }
            .into();
        }
        Action::Enable {
            adapter: key,
            confirmed,
        } => {
            ensure!(confirmed, "{}", t().features.wol_not_confirmed);
            ensure!(client.host.allowed(), "{}", t().features.wol_connections_off);
            ensure!(
                status.checked && status.adapters.iter().any(|a| a.key == key),
                "{}", t().features.wol_no_adapter_selected
            );
            // Revalidate physical identity and current network just before any cloud write.
            let rows = hardware(Some(key.clone()), false, stop.clone()).await?;
            let adapter = rows.into_iter().next().context(t().features.wol_adapter_removed)?;
            ensure!(adapter.connected, "{}", t().features.wol_adapter_disconnected);
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
                .context(t().features.wol_adapter_no_ipv4)?;
            // Relay registration also uses the primary route; avoid two independent owners
            // repeatedly replacing a device's LAN identity with different adapters.
            ensure!(info.default_metric.is_some(), "{}", t().features.wol_adapter_not_default);
            ensure!(
                inventory()
                    .await?
                    .first()
                    .is_some_and(|i| i.index == info.index),
                "{}", t().features.wol_default_network_changed
            );
            active(client, stop)?;
            client
                .wol_info(info.registration())
                .await
                .context(t().features.wol_registration_failed)?;
            active(client, stop)?;
            ensure!(client.host.allowed(), "{}", t().features.wol_connections_turned_off);
            status.enabled = None;
            let saved = client.set_wol_enabled(true).await;
            let readback = client.wol_enabled().await;
            status.enabled = readback.as_ref().ok().copied();
            saved.context(t().features.wol_enable_unconfirmed)?;
            ensure!(
                readback.context(t().features.wol_readback_failed)?,
                "{}", t().features.wol_still_off
            );
            active(client, stop)?;
            // Only the validated portable account owner installs login startup,
            // after cloud confirmation. A rejected/stale GUI request has no side effects.
            if !crate::platform::windows::host_service::resident::is_owner() {
                crate::platform::windows::host_service::startup::set_image(
                    true,
                    &std::env::current_exe()?,
                )
                .context(t().features.wol_startup_failed)?;
            }
            client.host.wol.refresh();
            status.message = t().features.wol_turned_on.into();
        }
        Action::Disable => {
            status.enabled = None;
            let saved = client.set_wol_enabled(false).await;
            let readback = client.wol_enabled().await;
            status.enabled = readback.as_ref().ok().copied();
            saved.context(t().features.wol_disable_unconfirmed)?;
            ensure!(
                !readback.context(t().features.wol_readback_failed)?,
                "{}", t().features.wol_still_on
            );
            client.host.wol.refresh();
            status.message = t().features.wol_turned_off.into();
        }
    }
    Ok(())
}
