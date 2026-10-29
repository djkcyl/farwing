//! One explicit diagnostic job per owned device. Uses a data-only shared lease.
use super::{Controller, Snapshot, lock};
use crate::{
    account::{api::DeviceInfo, client::AuthenticatedClient},
    media::ConnectionMediaOptions,
    session::controller::ControllerConnection,
};
use anyhow::{Context, Result, ensure};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex, OnceLock},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
struct Job {
    view: Mutex<Snapshot>,
    cancel: CancellationToken,
    done: CancellationToken,
}
fn jobs() -> &'static Mutex<HashMap<String, Arc<Job>>> {
    static JOBS: OnceLock<Mutex<HashMap<String, Arc<Job>>>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
fn key(controller: &str, target: &str) -> String {
    format!("{controller}:{target}")
}
pub(crate) fn snapshot(controller: &str, target: &str) -> Option<Snapshot> {
    lock(jobs())
        .get(&key(controller, target))
        .map(|job| lock(&job.view).clone())
}
pub(crate) fn cancel(controller: &str, target: &str) {
    if let Some(job) = lock(jobs()).get(&key(controller, target)) {
        job.cancel.cancel();
        lock(&job.view).progress.stage = crate::i18n::t().session.rdiag_cancelling.into();
    }
}
pub(crate) fn start(
    client: Arc<AuthenticatedClient>,
    device: DeviceInfo,
    options: ConnectionMediaOptions,
) {
    let key = key(&client.device_id(), &device.device_id);
    let mut jobs = lock(jobs());
    if jobs.get(&key).is_some_and(|job| !job.done.is_cancelled()) {
        return;
    }
    let job = Arc::new(Job {
        view: Mutex::new(Snapshot {
            busy: true,
            progress: super::bundle::Progress {
                stage: crate::i18n::t().session.rdiag_connecting.into(),
                step: super::bundle::Step::Other,
                completed: 0,
                total: 0,
            },
            ..Default::default()
        }),
        cancel: client.ended().child_token(),
        done: CancellationToken::new(),
    });
    jobs.insert(key, job.clone());
    tokio::spawn(async move {
        let _done = job.done.clone().drop_guard();
        let result = run(&client, &device, options, &job).await;
        let mut view = lock(&job.view);
        match result {
            Ok(value) => *view = value,
            Err(error) => {
                view.cancelled = job.cancel.is_cancelled();
                if view.cancelled {
                    view.error = None;
                    view.progress.stage = crate::i18n::t().session.rdiag_cancelled_short.into();
                } else {
                    // Keep the error chain in diagnostics, not in the product label.
                    tracing::warn!(error = %format!("{error:#}"), "remote diagnostic request failed");
                    view.error = Some(error.to_string());
                }
                view.path = None;
            }
        }
        view.busy = false;
    });
}
pub(crate) async fn shutdown_all() {
    let jobs = std::mem::take(&mut *lock(jobs()));
    for job in jobs.values() {
        job.cancel.cancel()
    }
    for job in jobs.values() {
        job.done.cancelled().await
    }
}
async fn run(
    client: &AuthenticatedClient,
    device: &DeviceInfo,
    options: ConnectionMediaOptions,
    job: &Job,
) -> Result<Snapshot> {
    let list = tokio::select! {_=job.cancel.cancelled()=>anyhow::bail!("{}", crate::i18n::t().session.rdiag_cancelled),list=client.list_devices()=>list.context(crate::i18n::t().session.rdiag_device_state_unreadable)?};
    let target = list.with_capability(&device.device_id, farwing_protocol::caps::DIAGNOSTICS)?;
    target
        .validated_device_id()
        .context(crate::i18n::t().session.rdiag_device_info_invalid)?;
    ensure!(
        target.platform == farwing_protocol::Platform::Windows
            && target.device_id != client.device_id()
            && target.is_connected()
            && target.controllable
            && target.controlled_support,
        "{}",
        crate::i18n::t().session.rdiag_device_unavailable
    );
    let policy = client.feature_catalog().policy_for(target);
    let connection =
        ControllerConnection::connect_diagnostics(client, target, policy, options, &job.cancel)
            .await
            .context(crate::i18n::t().session.rdiag_connect_failed)?;
    lock(&job.view).progress.stage = crate::i18n::t().session.rdiag_waiting_response.into();
    let wire = connection.stream_control_handle().diagnostics().clone();
    let mut started = false;
    let result = collect(&wire, job, &mut started).await;
    if result.is_err() && started {
        wire.cancel();
        // Let the per-request receiver delete its partial before releasing our
        // lease. This never closes another viewer or file-transfer attachment.
        while wire.snapshot().busy {
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    }
    let closed = connection.close().await;
    match result {
        Ok(snapshot) => {
            if let Err(error) = closed {
                tracing::debug!(%error,"diagnostic lease release after completed download");
            }
            Ok(snapshot)
        }
        Err(error) => Err(error),
    }
}
async fn collect(wire: &Controller, job: &Job, started: &mut bool) -> Result<Snapshot> {
    tokio::select! {
        _=job.cancel.cancelled()=>anyhow::bail!("{}", crate::i18n::t().session.rdiag_cancelled),
        ready=tokio::time::timeout(Duration::from_secs(15),async {
            loop {
                ensure!(!wire.is_closed(), "{}", crate::i18n::t().session.rdiag_connection_closed);
                let view=wire.snapshot();
                if view.supported {ensure!(view.allowed, "{}", crate::i18n::t().session.rdiag_files_not_allowed);return Ok::<_,anyhow::Error>(());}
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })=>ready.map_err(|_| anyhow::anyhow!("{}", crate::i18n::t().session.rdiag_no_response))??,
    }
    wire.start().context(crate::i18n::t().session.rdiag_start_failed)?;
    *started = true;
    loop {
        let snapshot = wire.snapshot();
        if !snapshot.busy {
            if let Some(error) = &snapshot.error {
                return Err(anyhow::anyhow!((crate::i18n::t().session.rdiag_fetch_failed)(error)));
            }
            ensure!(snapshot.path.is_some(), "{}", crate::i18n::t().session.rdiag_remote_incomplete);
            return Ok(snapshot);
        }
        *lock(&job.view) = snapshot;
        tokio::select! {_=job.cancel.cancelled()=>anyhow::bail!("{}", crate::i18n::t().session.rdiag_cancelled),_=tokio::time::sleep(Duration::from_millis(100))=>{}}
    }
}
