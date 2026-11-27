//! Interactive QR login orchestration.

pub(crate) mod sms;

use std::time::Duration;

use anyhow::{Context, Error, Result, bail};
use qrcode::{EcLevel, QrCode, render::unicode};
use tokio::time::Instant;

use crate::account::api::{LoginByQrRequest, LoginQrStatusRequest, NrdApi};
use crate::account::auth::{KeyringSessionStore, LoginSession, SessionStore};
use crate::account::session_restore::{self, RestoreTrigger};
use crate::session::device_session::{DeviceHandle, DeviceRuntime};

const QR_TIMEOUT: Duration = Duration::from_secs(5 * 60);
const STATUS_PENDING: i32 = 1;
const STATUS_SCANNED: i32 = 2;
const STATUS_CANCELED: i32 = 3;
const STATUS_CONFIRMED: i32 = 4;

#[derive(Debug)]
pub(crate) struct QrExpired;
impl std::fmt::Display for QrExpired {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("二维码已过期")
    }
}
impl std::error::Error for QrExpired {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LoginOutcome {
    Restored,
    New,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum LoginProgress {
    ValidatingSavedSession,
    RegisteringDevice,
    DeviceRegistered,
    QrReady(String),
    WaitingForScan,
    Scanned,
    CanceledRefreshing,
    Confirmed,
    SubmittingSms,
}

pub async fn interactive_login() -> Result<LoginOutcome> {
    login_with_progress(|progress| match progress {
        LoginProgress::ValidatingSavedSession => {
            println!("正在验证保存的登录态……");
        }
        LoginProgress::RegisteringDevice => {
            println!("正在初始化本机设备身份……");
        }
        LoginProgress::DeviceRegistered => {
            println!("本机设备已注册，并保存到系统凭据存储。");
        }
        LoginProgress::QrReady(content) => {
            if let Ok(code) = QrCode::with_error_correction_level(content.as_bytes(), EcLevel::L) {
                let image = code.render::<unicode::Dense1x2>().quiet_zone(true).build();
                println!("请使用 UU 手机端扫描以下二维码（二维码内容不会以明文写入日志）：\n");
                println!("{image}");
            }
        }
        LoginProgress::WaitingForScan => println!("状态：等待扫码"),
        LoginProgress::Scanned => println!("状态：已扫码，等待手机确认"),
        LoginProgress::CanceledRefreshing => println!("状态：手机端已取消，正在生成新的二维码"),
        LoginProgress::Confirmed => println!("状态：手机端已确认"),
        LoginProgress::SubmittingSms => println!("正在验证短信验证码……"),
    })
    .await
}

pub async fn login_with_progress<F>(report: F) -> Result<LoginOutcome>
where
    F: FnMut(LoginProgress),
{
    let device = DeviceRuntime::start()?;
    let result = prepare_login_with_progress(device.handle(), report)
        .await
        .and_then(PreparedLogin::commit);
    device.close().await;
    result
}

pub(crate) enum PreparedLogin {
    Restored,
    ConditionalNew {
        store: KeyringSessionStore,
        session: LoginSession,
        expected: Option<LoginSession>,
        permit: tokio::sync::OwnedMutexGuard<bool>,
    },
}

/// Both methods may wait independently. Only credential exchange/commit is
/// serialized, so two near-simultaneous confirmations cannot replace one another.
#[derive(Clone, Default)]
pub(crate) struct LoginCommitGate(std::sync::Arc<tokio::sync::Mutex<bool>>);

impl LoginCommitGate {
    pub(crate) async fn enter(&self) -> Result<tokio::sync::OwnedMutexGuard<bool>> {
        let permit = self.0.clone().lock_owned().await;
        if *permit {
            bail!("登录已完成");
        }
        Ok(permit)
    }
}

impl PreparedLogin {
    pub(crate) fn commit(self) -> Result<LoginOutcome> {
        match self {
            Self::Restored => Ok(LoginOutcome::Restored),
            Self::ConditionalNew {
                store,
                session,
                expected,
                mut permit,
            } => {
                if !store.save_if_matches(expected.as_ref(), &session)? {
                    bail!("登录状态已在其他进程中改变，请重新登录");
                }
                *permit = true;
                Ok(LoginOutcome::New)
            }
        }
    }
}

pub(crate) async fn prepare_login_with_progress<F>(
    device: DeviceHandle,
    report: F,
) -> Result<PreparedLogin>
where
    F: FnMut(LoginProgress),
{
    prepare_login_with_gate(device, LoginCommitGate::default(), report).await
}

pub(crate) async fn prepare_login_with_gate<F>(
    device: DeviceHandle,
    gate: LoginCommitGate,
    mut report: F,
) -> Result<PreparedLogin>
where
    F: FnMut(LoginProgress),
{
    let session_store = match device.self_hosted() {
        Some(settings) => settings.session_store()?,
        None => KeyringSessionStore::new()?,
    };
    report(LoginProgress::RegisteringDevice);
    let identity = device.ensure(true).await?.client_identity()?;
    let mut api = match device.self_hosted() {
        Some(settings) => NrdApi::self_hosted(identity, settings.clone())?,
        None => NrdApi::new(identity)?,
    };
    report(LoginProgress::DeviceRegistered);

    if let Some(session) = session_store.load()? {
        report(LoginProgress::ValidatingSavedSession);
        api.set_user_id(Some(session.user_id()))?;
        api.set_bearer_token(Some(session.token()))?;
        match session_restore::restore_user(&api, RestoreTrigger::ExplicitLogin).await {
            Ok(_) => {
                return Ok(PreparedLogin::Restored);
            }
            Err(failure) if failure.invalid_saved_credentials() => {
                session_store.clear_if_matches(&session)?;
                api.set_user_id(None)?;
                api.set_bearer_token(None)?;
            }
            Err(error) => {
                return Err(Error::new(error))
                    .context("could not validate the saved login session; it was kept unchanged");
            }
        }
    }

    let mut qr = api.generate_login_qr().await?.into_data()?;
    qr.validate()?;
    report(LoginProgress::QrReady(qr.qrcode_jump_url.clone()));

    let mut deadline = Instant::now() + QR_TIMEOUT;
    let mut state = LoginQrStatusRequest::initial(&qr);
    let mut announced_status = None;
    loop {
        if Instant::now() >= deadline {
            return Err(QrExpired.into());
        }
        let response = match api.get_login_qr_status(&state).await {
            Ok(response) => response,
            Err(error) if is_retryable_poll_error(&error) => continue,
            Err(error) => return Err(error),
        };
        let status = response.into_data()?.resolved_status()?;
        if announced_status != Some(status) {
            match status {
                STATUS_PENDING => report(LoginProgress::WaitingForScan),
                STATUS_SCANNED => report(LoginProgress::Scanned),
                STATUS_CANCELED => {}
                STATUS_CONFIRMED => report(LoginProgress::Confirmed),
                _ => {}
            }
            announced_status = Some(status);
        }
        match status {
            STATUS_PENDING | STATUS_SCANNED => state.login_status = status,
            STATUS_CANCELED => {
                // LoginPresenter refreshes its request/QR after phone cancel.
                // The old request is already complete, and this sequential
                // loop never reuses its token or last polled status.
                report(LoginProgress::CanceledRefreshing);
                qr = api.generate_login_qr().await?.into_data()?;
                qr.validate()?;
                state = LoginQrStatusRequest::initial(&qr);
                announced_status = None;
                deadline = Instant::now() + QR_TIMEOUT;
                report(LoginProgress::QrReady(qr.qrcode_jump_url.clone()));
            }
            STATUS_CONFIRMED => break,
            _ => bail!("NRD API returned an unknown QR status: {status}"),
        }
    }

    // 3B7170's QR exchange owner forces device initialization after phone
    // confirmation as well; a QR token is not itself a registered identity.
    let permit = gate.enter().await?;
    api.set_identity(device.ensure(true).await?.client_identity()?);
    let expected = session_store.load()?;
    let login = api
        .login_by_qr(&LoginByQrRequest {
            qrcode_jump_url: qr.qrcode_jump_url,
            token: qr.token,
        })
        .await?
        .into_data()?;
    let session = LoginSession::new(login.token, login.user_id, login.nickname)?;
    Ok(PreparedLogin::ConditionalNew {
        store: session_store,
        session,
        expected,
        permit,
    })
}

fn is_retryable_poll_error(error: &Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<reqwest::Error>()
            .is_some_and(|error| error.is_timeout() || error.is_connect() || error.is_request())
    })
}
