//! Native UU SMS login; only invoked by explicit user actions.
use super::{LoginProgress, PreparedLogin};
use crate::account::api::{ApiFailure, NrdApi};
use crate::account::auth::{KeyringSessionStore, LoginSession, SessionStore};
use crate::session::device_session::DeviceHandle;
use anyhow::{Result, bail};
use std::{
    fmt,
    time::{Duration, Instant},
};

pub(crate) const RESEND_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const TERMS_URL: &str =
    "https://adl.netease.com/d/g/uuremote/c/licenseandservice?type=pc&direct=1";
pub(crate) const PRIVACY_URL: &str =
    "https://adl.netease.com/d/g/uuremote/c/privacy?type=pc&direct=1";

#[derive(Clone, Eq, PartialEq)]
pub(crate) struct PhoneNumber {
    country: String,
    mobile: String,
}
impl fmt::Debug for PhoneNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("PhoneNumber(REDACTED)")
    }
}
impl PhoneNumber {
    pub(crate) fn parse(country: &str, mobile: &str) -> Result<Self> {
        let country = country.trim().strip_prefix('+').unwrap_or(country.trim());
        let mobile = mobile.trim();
        if country.is_empty() || !country.bytes().all(|b| b.is_ascii_digit()) {
            bail!("请输入有效区号");
        }
        if mobile.is_empty() || !mobile.bytes().all(|b| b.is_ascii_digit()) {
            bail!("请输入手机号，仅限数字");
        }
        if country == "86" && mobile.len() != 11 {
            bail!("请输入11位手机号");
        }
        Ok(Self {
            country: country.into(),
            mobile: mobile.into(),
        })
    }
}

pub(crate) fn validate_code(code: &str) -> Result<()> {
    if code.len() != 6 || !code.bytes().all(|b| b.is_ascii_digit()) {
        bail!("请输入6位短信验证码");
    }
    Ok(())
}

/// Worker-owned contact binding and cooldown; cancellation does not undo a
/// possible SMS delivery or permit another immediate send.
#[derive(Default)]
pub(crate) struct SmsGate {
    bound: Option<PhoneNumber>,
    resend_at: Option<Instant>,
}
impl SmsGate {
    pub(crate) fn begin_request(&mut self, now: Instant, agreed: bool) -> Result<Instant> {
        if !agreed {
            bail!("请先阅读并同意用户协议和隐私政策");
        }
        if self.resend_at.is_some_and(|at| at > now) {
            bail!("请等待倒计时结束后重试");
        }
        self.bound = None;
        let deadline = now + RESEND_INTERVAL;
        self.resend_at = Some(deadline);
        Ok(deadline)
    }
    pub(crate) fn dispatched(&mut self, phone: PhoneNumber) {
        self.bound = Some(phone);
    }
    pub(crate) fn validate_submission(
        &self,
        phone: &PhoneNumber,
        code: &str,
        agreed: bool,
    ) -> Result<()> {
        if !agreed {
            bail!("请先阅读并同意用户协议和隐私政策");
        }
        validate_code(code)?;
        if self.bound.as_ref() != Some(phone) {
            bail!("请先为当前手机号获取验证码");
        }
        Ok(())
    }
    pub(crate) fn cancel(&mut self) {
        self.bound = None;
    }
    pub(crate) fn resend_at(&self) -> Option<Instant> {
        self.resend_at
    }
}

pub(crate) struct CodeOutcome {
    pub dispatched: bool,
    pub result: Result<()>,
}

pub(crate) async fn request_code(device: DeviceHandle, phone: PhoneNumber) -> CodeOutcome {
    let prepared = async { NrdApi::new(device.ensure(false).await?.client_identity()?) }.await;
    match prepared {
        Ok(api) => CodeOutcome {
            dispatched: true,
            result: api.request_sms_code(&phone.country, &phone.mobile).await,
        },
        Err(error) => CodeOutcome {
            dispatched: false,
            result: Err(error),
        },
    }
}

pub(crate) async fn prepare_login(
    device: DeviceHandle,
    gate: super::LoginCommitGate,
    phone: PhoneNumber,
    code: String,
    mut progress: impl FnMut(LoginProgress),
) -> Result<PreparedLogin> {
    validate_code(&code)?;
    let store = KeyringSessionStore::new()?;
    progress(LoginProgress::RegisteringDevice);
    let permit = gate.enter().await?;
    // Server_HandleManualLogin_Sms forces initialization; ensure handles its
    // intermediate UUID-reset outcome rather than treating it as completion.
    let identity = device.ensure(true).await?;
    // Initialization may retire the previous account on UUID reset.
    // Guard subsequent changes, not that completed initialization transition.
    let expected = store.load()?;
    let api = NrdApi::new(identity.client_identity()?)?;
    progress(LoginProgress::SubmittingSms);
    let response = api
        .login_by_mobile(&phone.country, &phone.mobile, &code)
        .await?;
    if response.code == 1120
        && let Some(previous) = &expected
    {
        store.clear_if_matches(previous)?;
    }
    let login = response.into_data()?;
    let session = LoginSession::new(login.token, login.user_id, login.nickname)?;
    Ok(PreparedLogin::ConditionalNew {
        store,
        session,
        expected,
        permit,
    })
}

pub(crate) fn error_message(
    error: &anyhow::Error,
    phone: Option<&PhoneNumber>,
    code: Option<&str>,
) -> String {
    if let Some(failure) = error.downcast_ref::<ApiFailure>() {
        let mut message = failure.message.clone();
        if let Some(phone) = phone {
            message = message.replace(&phone.mobile, "***");
        }
        if let Some(code) = code.filter(|c| !c.is_empty()) {
            message = message.replace(code, "***");
        }
        let message: String = message
            .chars()
            .filter(|c| !c.is_control())
            .take(160)
            .collect();
        if message.is_empty() {
            format!("请求被拒绝（{}）", failure.code)
        } else {
            format!("{message}（{}）", failure.code)
        }
    } else if error
        .chain()
        .any(|e| e.downcast_ref::<reqwest::Error>().is_some())
    {
        "网络请求未完成，请稍后重试".into()
    } else {
        // In particular, don't echo deserializer errors containing a response
        // token, nor an unexpected credential-store value.
        "登录请求未完成，请重试或使用扫码登录".into()
    }
}
