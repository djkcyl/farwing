//! Registered-device publication owned by the active account, never a viewer.
use super::*;
use crate::{
    account::{auth::SecretEntry, reporting},
    platform::{device_profile, host_service::resident},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tokio::task::JoinHandle;

#[derive(Default)]
pub(super) struct Sync {
    identity: Option<String>,
    pub(super) task: Option<JoinHandle<()>>,
}
impl Drop for Sync {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
#[derive(Serialize, Deserialize)]
struct Receipt {
    image_sha256: String,
    url_sha256: String,
}
fn digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

impl AuthenticatedClient {
    pub(super) fn schedule_publication(&self) {
        if resident::managed() {
            return;
        }
        let Ok(identity) = self.device.identity().client_identity() else {
            return;
        };
        if identity.device_id.is_empty() || !self.is_active() {
            return;
        }
        let key = digest(
            serde_json::to_string(&[
                self.session.user_id(),
                &identity.client_id,
                &identity.system_id,
                &identity.device_id,
            ])
            .expect("identity strings serialize")
            .as_bytes(),
        );
        let mut sync = self.publication.lock().unwrap_or_else(|e| e.into_inner());
        if sync.identity.as_ref() == Some(&key) {
            return;
        }
        let Some(api) = self.api.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        if let Some(task) = sync.task.take() {
            task.abort();
        }
        sync.identity = Some(key.clone());
        let device = self.device.clone();
        let session = self.session.clone();
        let ended = self.ended.clone();
        sync.task = Some(tokio::spawn(async move {
            let job = async {
                let mut source_seen = None;
                let mut source_error = None;
                let mut attempted_digest = String::new();
                let mut explicit = false;
                let mut readback_needed = true;
                loop {
                    verify_owner(&device, &identity, &session).await?;
                    // Native collection has no network traffic. Only changed
                    // facts or an explicit refresh submit another init request.
                    let hardware =
                        tokio::task::spawn_blocking(device_profile::Hardware::read).await?;
                    match hardware {
                        Ok(hardware)
                            if explicit
                                || reporting::snapshot().hardware.as_ref() != Some(&hardware) =>
                        {
                            readback_needed = true;
                            if let Err(error) = device.ensure(true).await {
                                reporting::update(|s| {
                                    s.registration = format!("上报失败：{error:#}")
                                });
                            }
                        }
                        Err(error) => reporting::update(|s| {
                            s.registration = format!("信息读取失败：{error:#}")
                        }),
                        _ => {}
                    }
                    if readback_needed {
                        readback_needed = false;
                        verify_owner(&device, &identity, &session).await?;
                        match api
                            .device_detail(&identity.device_id)
                            .await
                            .and_then(|v| v.into_data())
                        {
                            Ok(detail) => reporting::update(|s| {
                                s.server_details = detail.details;
                                s.readback = "已读取服务端资料".into();
                                s.readback_at = Some(chrono::Utc::now().timestamp());
                            }),
                            Err(error) => {
                                reporting::update(|s| s.readback = format!("回读失败：{error:#}"))
                            }
                        }
                    }
                    let source =
                        tokio::task::spawn_blocking(device_profile::wallpaper_source).await?;
                    if source.is_ok() {
                        source_error = None;
                    }
                    match source {
                        Ok(source) if explicit || source_seen.as_ref() != Some(&source) => {
                            tracing::info!(
                                cached = source.cached(),
                                "device wallpaper source resolved"
                            );
                            let capture = source.clone();
                            let result = async {
                                reporting::update(|s| {
                                    s.wallpaper_file = source.path.display().to_string()
                                });
                                let image = tokio::task::spawn_blocking(move || {
                                    device_profile::wallpaper_image(&capture)
                                })
                                .await??;
                                let hash = digest(&image);
                                if !explicit && attempted_digest == hash {
                                    return Ok(());
                                }
                                attempted_digest = hash.clone();
                                reporting::update(|s| {
                                    s.wallpaper_digest = hash;
                                    s.wallpaper = "正在核对壁纸上报".into();
                                });
                                let current = api.list_devices().await?.into_data()?;
                                anyhow::ensure!(
                                    current.current_device.device_id == identity.device_id,
                                    "当前注册设备与上报目标不一致"
                                );
                                synchronize(
                                    api.clone(),
                                    device.clone(),
                                    identity.clone(),
                                    session.clone(),
                                    key.clone(),
                                    current.current_device.wallpaper_url,
                                    bytes::Bytes::from(image),
                                )
                                .await?;
                                anyhow::Ok(())
                            }
                            .await;
                            source_seen = Some(source);
                            if let Err(error) = result {
                                reporting::update(|s| {
                                    s.wallpaper = format!("上报未确认：{error:#}")
                                });
                                tracing::warn!(%error,"device wallpaper publication not confirmed");
                            }
                        }
                        Err(error) => {
                            source_seen = None;
                            let error = format!("{error:#}");
                            if source_error.as_ref() != Some(&error) {
                                tracing::warn!(%error, "device wallpaper source unavailable");
                                source_error = Some(error.clone());
                            }
                            reporting::update(|s| s.wallpaper = format!("不可用：{error}"));
                        }
                        _ => {}
                    }
                    explicit = tokio::select! {
                        _=reporting::REFRESH.notified()=>true,
                        _=tokio::time::sleep(std::time::Duration::from_secs(30))=>false,
                    };
                }
                #[allow(unreachable_code)]
                anyhow::Ok(())
            };
            tokio::select! {
                biased;
                _=ended.cancelled()=>{},
                result=job=>if let Err(error)=result {reporting::update(|s|s.wallpaper=format!("上报任务已停止：{error:#}"));},
            }
        }));
    }
}
async fn verify_owner(
    device: &DeviceHandle,
    expected: &crate::account::api::ClientIdentity,
    session: &LoginSession,
) -> Result<()> {
    let live = device.identity().client_identity()?;
    if live.device_id != expected.device_id
        || live.client_id != expected.client_id
        || live.system_id != expected.system_id
    {
        bail!("wallpaper identity changed");
    }
    let expected = expected.clone();
    let session = session.clone();
    tokio::task::spawn_blocking(move || {
        let saved = KeyringIdentityStore::new()?
            .load_existing()?
            .context("wallpaper identity no longer exists")?
            .client_identity()?;
        let account = KeyringSessionStore::new()?
            .load()?
            .context("wallpaper account ended")?;
        if saved.device_id != expected.device_id
            || saved.client_id != expected.client_id
            || saved.system_id != expected.system_id
            || account.user_id() != session.user_id()
            || account.token() != session.token()
        {
            bail!("wallpaper owner changed");
        }
        Ok(())
    })
    .await
    .context("wallpaper identity check interrupted")?
}

async fn synchronize(
    api: NrdApi,
    device: DeviceHandle,
    identity: crate::account::api::ClientIdentity,
    session: LoginSession,
    key: String,
    current_url: String,
    image: bytes::Bytes,
) -> Result<bool> {
    verify_owner(&device, &identity, &session).await?;
    let image_sha256 = digest(&image);
    let entry = Arc::new(
        SecretEntry::new("com.openuuyc.wallpaper", &key)
            .map_err(|_| anyhow::anyhow!("wallpaper receipt store unavailable"))?,
    );
    let reader = Arc::clone(&entry);
    let receipt = tokio::task::spawn_blocking(move || -> Result<Option<Receipt>> {
        match reader.get_secret() {
            Ok(bytes) => Ok(serde_json::from_slice(&bytes).ok()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(_) => bail!("wallpaper receipt could not be read"),
        }
    })
    .await
    .context("wallpaper receipt read interrupted")??;
    if !current_url.is_empty()
        && receipt.as_ref().is_some_and(|r| {
            r.image_sha256 == image_sha256 && r.url_sha256 == digest(current_url.as_bytes())
        })
    {
        reporting::update(|s| {
            s.wallpaper = "已同步".into();
            s.wallpaper_url = current_url;
            s.wallpaper_at = Some(chrono::Utc::now().timestamp());
        });
        return Ok(false);
    }

    // The account snapshot and verify_owner bind this job to this application's
    // current registered identity. Hardware descriptions are not identity proofs.
    let grant = api
        .wallpaper_grant()
        .await
        .map_err(|_| anyhow::anyhow!("wallpaper upload grant request failed"))?;
    verify_owner(&device, &identity, &session).await?;
    reporting::update(|s| s.wallpaper = "正在上传并校验图片".into());
    let uploaded = crate::account::api::wallpaper::upload_wallpaper(grant, image).await?;
    verify_owner(&device, &identity, &session).await?;

    // This records the desired uploaded image, not a claim that binding succeeded.
    // On restart it counts as success only if the server returns the same URL.
    let record = serde_json::to_vec(&Receipt {
        image_sha256,
        url_sha256: digest(uploaded.url.as_bytes()),
    })?;
    tokio::task::spawn_blocking(move || {
        entry
            .set_secret(&record)
            .map_err(|_| anyhow::anyhow!("wallpaper upload receipt could not be saved"))
    })
    .await
    .context("wallpaper receipt write interrupted")??;
    verify_owner(&device, &identity, &session).await?;
    let binding = api.bind_wallpaper(&uploaded.url).await;
    verify_owner(&device, &identity, &session).await?;
    let list = api
        .list_devices()
        .await
        .map_err(|_| anyhow::anyhow!("wallpaper readback failed; binding not replayed"))?
        .into_data()
        .map_err(|_| anyhow::anyhow!("wallpaper readback rejected; binding not replayed"))?;
    if list.current_device.device_id == identity.device_id
        && list.current_device.wallpaper_url == uploaded.url
    {
        reporting::update(|s| {
            s.wallpaper = "已上传并确认".into();
            s.wallpaper_url = uploaded.url;
            s.wallpaper_at = Some(chrono::Utc::now().timestamp());
        });
        return Ok(true);
    }
    if binding.is_err() {
        bail!("wallpaper binding failed or unconfirmed; binding not replayed");
    }
    bail!("wallpaper binding accepted but readback differs; binding not replayed")
}
