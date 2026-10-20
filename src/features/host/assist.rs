//! Who may connect to this host without being one of its account's devices
//! or a friend (`docs/access-model.md` §2.2–§2.3): devices it trusts, and
//! visitors with the access code or the local user's confirmation.
//!
//! Codes never leave this device: a controller proves knowledge of the code
//! with SPAKE2 relayed by the server (`docs/protocol/v2-design.md` §2) or
//! over the direct TLS channel. A trusted device is admitted by its key.

use crate::session::host_client::HostClient;
use anyhow::{Context, Result, ensure};
use farwing_protocol::api::{AssistDecision, AssistPakeReply, AssistRefusal};
use farwing_protocol::crypto;
use rand::Rng;
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Mode {
    #[default]
    Password,
    Confirmation,
    PasswordConfirmation,
}
impl Mode {
    pub const ALL: [Self; 3] = [
        Self::Password,
        Self::Confirmation,
        Self::PasswordConfirmation,
    ];
    pub fn label(self) -> &'static str {
        match self {
            Self::Password => "访问码",
            Self::Confirmation => "仅本机确认",
            Self::PasswordConfirmation => "访问码 + 本机确认",
        }
    }
    pub fn wire(self) -> crate::account::api::AssistMode {
        match self {
            Self::Password => crate::account::api::AssistMode::Password,
            Self::Confirmation => crate::account::api::AssistMode::Confirmation,
            Self::PasswordConfirmation => crate::account::api::AssistMode::PasswordConfirmation,
        }
    }
    pub fn needs_password(self) -> bool {
        self != Self::Confirmation
    }
    pub fn needs_confirmation(self) -> bool {
        self != Self::Password
    }
}

// Deliberately no Debug: this configuration contains the local access code.
#[derive(Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Passwords {
    #[default]
    Random,
    Custom,
    Both,
}
impl Passwords {
    pub const ALL: [Self; 3] = [Self::Random, Self::Custom, Self::Both];
    pub fn label(self) -> &'static str {
        match self {
            Self::Random => "临时访问码",
            Self::Custom => "固定访问码",
            Self::Both => "两种都接受",
        }
    }
    pub fn random(self) -> bool {
        self != Self::Custom
    }
    pub fn custom(self) -> bool {
        self != Self::Random
    }
}

#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Settings {
    pub enabled: bool,
    pub mode: Mode,
    pub code: String,
    #[serde(default)]
    pub passwords: Passwords,
    #[serde(default)]
    pub custom_code: String,
    /// Capabilities an assisting participant gets; `None` is the default
    /// template (`farwing_protocol::caps::DEFAULT_ASSIST`).
    #[serde(default)]
    pub grants: Option<Vec<String>>,
}
impl Settings {
    /// Capabilities that may be offered to assistance. Power, ports and
    /// diagnostics stay with account devices and friends.
    pub const OFFERED: &[&str] = &[
        farwing_protocol::caps::CONTROL,
        farwing_protocol::caps::CLIPBOARD,
        farwing_protocol::caps::FILES,
        farwing_protocol::caps::AUDIO,
        farwing_protocol::caps::MICROPHONE,
        farwing_protocol::caps::ANNOTATION,
        farwing_protocol::caps::DISPLAY,
    ];
    /// The capabilities of an assisting participant; viewing is always
    /// included.
    pub fn grants(&self) -> Vec<String> {
        let mut grants: Vec<String> = match &self.grants {
            Some(grants) => grants
                .iter()
                .filter(|cap| Self::OFFERED.contains(&cap.as_str()))
                .cloned()
                .collect(),
            None => farwing_protocol::caps::DEFAULT_ASSIST
                .iter()
                .map(|cap| (*cap).to_owned())
                .collect(),
        };
        grants.push(farwing_protocol::caps::VIEW.to_owned());
        farwing_protocol::caps::normalize(&grants)
    }
    pub fn valid_custom_code(code: &str) -> bool {
        (8..=16).contains(&code.len())
            && code.bytes().all(|b| b.is_ascii_alphanumeric())
            && code.bytes().any(|b| b.is_ascii_alphabetic())
            && code.bytes().any(|b| b.is_ascii_digit())
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.code.is_empty()
                || ((8..=32).contains(&self.code.len())
                    && self.code.bytes().all(|b| b.is_ascii_alphanumeric())
                    && self.code.bytes().any(|b| b.is_ascii_alphabetic())
                    && self.code.bytes().any(|b| b.is_ascii_digit())),
            "临时访问码须为8至32位，且同时包含字母和数字"
        );
        ensure!(
            self.custom_code.is_empty() || Self::valid_custom_code(&self.custom_code),
            "固定访问码须为8至16位字母和数字，且同时包含两者"
        );
        ensure!(
            self.grants.as_ref().is_none_or(|grants| grants
                .iter()
                .all(|cap| cap == farwing_protocol::caps::VIEW || Self::OFFERED.contains(&cap.as_str()))),
            "访客权限无效"
        );
        if self.enabled && self.mode.needs_password() {
            ensure!(
                !self.passwords.random() || !self.code.is_empty(),
                "请先生成临时访问码"
            );
            ensure!(
                !self.passwords.custom() || !self.custom_code.is_empty(),
                "请先设置固定访问码"
            );
        }
        Ok(())
    }
    /// The codes a controller may use, one per PAKE slot: the random code
    /// in slot 0 and the custom password in slot 1. Unused slots are `None`.
    pub(crate) fn slot_codes(&self) -> [Option<&str>; crypto::ASSIST_PAKE_SLOTS] {
        [
            (self.passwords.random() && !self.code.is_empty()).then_some(self.code.as_str()),
            (self.passwords.custom() && !self.custom_code.is_empty())
                .then_some(self.custom_code.as_str()),
        ]
    }
    pub fn refresh_code(&mut self) {
        const LETTERS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ";
        const DIGITS: &[u8] = b"23456789";
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut rng = rand::rng();
        let mut code = [0u8; 8];
        code[0] = LETTERS[rng.random_range(0..LETTERS.len())];
        code[1] = DIGITS[rng.random_range(0..DIGITS.len())];
        for byte in &mut code[2..] {
            *byte = ALPHABET[rng.random_range(0..ALPHABET.len())];
        }
        code.shuffle(&mut rng);
        self.code = code.into_iter().map(char::from).collect();
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Confirmation {
    pub id: String,
    pub token: String,
    pub name: String,
    pub expires_at: i64,
    pub responding: bool,
    /// The asking device's ID and key, for showing it and for trusting it.
    #[serde(default)]
    pub device_id: String,
    #[serde(default)]
    pub public_key: String,
    /// The server's limit on the asking controller (browsers): it gets no
    /// more than this, whatever visitors may do.
    #[serde(default)]
    pub caps_limit: Option<Vec<String>>,
    /// A pairing request: the six digits both screens show.
    #[serde(default)]
    pub pairing: Option<String>,
}

/// The local user's answer to a direct request.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Reject,
    /// This connection only.
    Allow,
    /// Allowed, and the device is trusted from now on (always so for a
    /// pairing request).
    Trust,
    /// Nobody can answer here: the reason.
    Unavailable(&'static str),
}
#[derive(Clone, Serialize, Deserialize)]
pub(crate) struct Attempt {
    pub id: String,
    pub verifying: bool,
    pub expires_at: i64,
}
#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct Snapshot {
    pub settings: Settings,
    pub connect_id: String,
    pub ready: bool,
    pub error: Option<String>,
    pub pending: Option<Confirmation>,
    pub attempt: Option<Attempt>,
}

enum Push {
    /// `assist.join`: a controller asks; `pake` is empty without a code.
    Join {
        id: String,
        name: String,
        device_id: String,
        public_key: String,
        pake: Vec<String>,
        /// The server's limit on the controller (browsers).
        caps_limit: Option<Vec<String>>,
    },
    /// `assist.accept`: the controller's key confirmation.
    Accept {
        id: String,
        key_confirm: String,
    },
    Cancel(String),
    Answer {
        id: String,
        token: String,
        allow: bool,
        trust: bool,
    },
    Refresh,
    Reject(String),
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Action {
    Refresh,
    Answer {
        id: String,
        token: String,
        allow: bool,
        /// Also trust the asking device from now on.
        #[serde(default)]
        trust: bool,
    },
}

/// The trusted device holding `public_key`, if it is the device `device_id`.
fn trusted_device(device_id: &str, public_key: &str) -> Option<crate::transport::direct::store::TrustedDevice> {
    crate::transport::direct::store::find_trusted(public_key).filter(|d| d.device_id == device_id)
}

/// Trust the device of an allowed request, with the visitor permissions
/// (within the server's limit on a browser).
fn trust(name: &str, device_id: &str, public_key: &str, mut grants: Vec<String>, caps_limit: Option<Vec<String>>) {
    use crate::transport::direct::store;
    if store::device_id_of(public_key).as_deref() != Some(device_id) {
        return;
    }
    if let Some(limit) = caps_limit {
        grants.retain(|cap| limit.contains(cap));
    }
    let now = chrono::Utc::now().timestamp();
    match store::save_trusted(store::TrustedDevice {
        device_id: device_id.to_owned(),
        public_key: public_key.to_owned(),
        name: sanitize(name),
        grants,
        trusted_at: now,
        last_seen: now,
    }) {
        Ok(()) => tracing::info!(device = %device_id, "device trusted after confirmation"),
        Err(error) => tracing::warn!(error = %format!("{error:#}"), "device could not be trusted"),
    }
}

struct State {
    snapshot: Snapshot,
    requests: Option<mpsc::Sender<Push>>,
    /// PAKE keys of granted requests by controller device, for binding the
    /// assistance session's DTLS fingerprints.
    keys: HashMap<String, ([u8; 32], Instant)>,
    /// Wrong codes (assistance and pairing, server-relayed and direct)
    /// within [`FAILURE_WINDOW`], and the lockout they caused.
    failures: VecDeque<Instant>,
    locked_until: Option<Instant>,
    /// Confirmations of direct assistance and pairing requests waiting for
    /// the user.
    direct: HashMap<String, tokio::sync::oneshot::Sender<Decision>>,
    revision: u64,
    operations: CancellationToken,
    admission: CancellationToken,
    pending_deadline: Option<Instant>,
    attempt_deadline: Option<Instant>,
    ui_seen: Option<Instant>,
}
impl Default for State {
    fn default() -> Self {
        Self {
            snapshot: Snapshot::default(),
            requests: None,
            keys: HashMap::new(),
            failures: VecDeque::new(),
            locked_until: None,
            direct: HashMap::new(),
            revision: 0,
            operations: CancellationToken::new(),
            admission: CancellationToken::new(),
            pending_deadline: None,
            attempt_deadline: None,
            ui_seen: None,
        }
    }
}
/// Count a wrong code; lock assistance and pairing after too many.
fn note_failure(state: &mut State) {
    state.failures.push_back(Instant::now());
    check_lock(state);
}

/// Whether wrong codes currently lock assistance and pairing.
fn check_lock(state: &mut State) -> bool {
    state.failures.retain(|at| at.elapsed() < FAILURE_WINDOW);
    if state.failures.len() >= FAILURE_LIMIT && state.locked_until.is_none() {
        state.locked_until = Some(Instant::now() + LOCKOUT);
        state.snapshot.error = Some(LOCKED_MESSAGE.into());
        tracing::warn!("assistance and pairing locked after repeated wrong codes");
    }
    let locked = state.locked_until.is_some_and(|until| Instant::now() < until);
    if !locked && state.locked_until.take().is_some() {
        state.failures.clear();
        if state.snapshot.error.as_deref() == Some(LOCKED_MESSAGE) {
            state.snapshot.error = None;
        }
    }
    locked
}

#[derive(Clone, Default)]
pub(crate) struct Handle(Arc<Mutex<State>>);
impl Handle {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|e| e.into_inner())
    }
    /// Count a wrong assistance or pairing code (`docs/protocol/v2-design.md`
    /// §2, §12.2).
    pub(crate) fn record_failure(&self) {
        note_failure(&mut self.lock());
    }
    /// Whether too many wrong codes lock assistance and pairing right now.
    pub(crate) fn locked(&self) -> bool {
        check_lock(&mut self.lock())
    }
    /// The host half of a direct assistance PAKE: messages, key
    /// confirmations bound to the TLS channel, and the keys per slot.
    pub(crate) fn direct_pake(
        &self,
        ekm: &[u8],
        pake: &[String],
    ) -> (Vec<String>, Vec<String>, Vec<Option<[u8; 32]>>) {
        let settings = self.settings();
        answer_slots(&settings, pake, |key| crypto::direct_confirm(key, "host", ekm))
    }
    /// Ask the local user to allow a direct assistance request.
    pub(crate) async fn confirm_direct(&self, name: String, device_id: String, public_key: String) -> Decision {
        self.ask_direct(name, device_id, public_key, None).await
    }

    /// Ask the local user to pair with a device that shows `code`: trust
    /// each other with full control.
    pub(crate) async fn confirm_pairing(
        &self,
        name: String,
        device_id: String,
        public_key: String,
        code: String,
    ) -> Decision {
        self.ask_direct(name, device_id, public_key, Some(code)).await
    }

    async fn ask_direct(
        &self,
        name: String,
        device_id: String,
        public_key: String,
        pairing: Option<String>,
    ) -> Decision {
        let (sender, answer) = tokio::sync::oneshot::channel();
        let id = uuid::Uuid::new_v4().to_string();
        {
            let mut state = self.lock();
            if state.snapshot.pending.is_some() {
                return Decision::Unavailable("对方正在处理另一个连接请求，请稍后再试");
            }
            if crate::platform::capture::session_locked() != Some(false) {
                return Decision::Unavailable("对方的电脑已锁定，没有人能确认");
            }
            if !state
                .ui_seen
                .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
            {
                return Decision::Unavailable("对方没有打开 Farwing 设备中心，无法在那里确认");
            }
            state.snapshot.pending = Some(Confirmation {
                id: id.clone(),
                token: uuid::Uuid::new_v4().simple().to_string(),
                name: sanitize(&name),
                expires_at: chrono::Utc::now().timestamp() + 60,
                responding: false,
                device_id,
                public_key,
                caps_limit: None,
                pairing,
            });
            state.direct.insert(id.clone(), sender);
        }
        // Ends the request when the asking side gives up (this future is
        // dropped) as well as when it is answered or expires.
        struct Ending<'a>(&'a Handle, String);
        impl Drop for Ending<'_> {
            fn drop(&mut self) {
                let mut state = self.0.lock();
                state.direct.remove(&self.1);
                if state.snapshot.pending.as_ref().is_some_and(|p| p.id == self.1) {
                    state.snapshot.pending = None;
                }
            }
        }
        let _ending = Ending(self, id);
        tokio::time::timeout(Duration::from_secs(60), answer)
            .await
            .ok()
            .and_then(Result::ok)
            .unwrap_or(Decision::Reject)
    }
    pub fn settings(&self) -> Settings {
        self.lock().snapshot.settings.clone()
    }
    pub fn snapshot(&self) -> Snapshot {
        self.lock().snapshot.clone()
    }
    pub fn replace(&self, snapshot: Snapshot) {
        self.lock().snapshot = snapshot;
    }
    pub fn configure(&self, settings: Settings) -> Result<bool> {
        settings.validate()?;
        let mut state = self.lock();
        if settings == state.snapshot.settings {
            return Ok(false);
        }
        state.operations.cancel();
        state.operations = CancellationToken::new();
        if !settings.enabled || settings.mode != state.snapshot.settings.mode {
            state.admission.cancel();
            state.admission = CancellationToken::new();
        }
        state.revision = state.revision.wrapping_add(1);
        state.snapshot.settings = settings;
        if let Some(pending) = state.snapshot.pending.take()
            && let Some(requests) = &state.requests
        {
            let _ = requests.try_send(Push::Reject(pending.id));
        }
        state.pending_deadline = None;
        state.snapshot.attempt = None;
        state.attempt_deadline = None;
        state.snapshot.error = None;
        Ok(true)
    }
    pub fn unavailable(&self) {
        let mut state = self.lock();
        state.operations.cancel();
        state.operations = CancellationToken::new();
        state.admission.cancel();
        state.admission = CancellationToken::new();
        state.snapshot.ready = false;
        state.snapshot.pending = None;
        state.pending_deadline = None;
        state.snapshot.attempt = None;
        state.attempt_deadline = None;
    }
    pub fn admission(&self) -> Result<CancellationToken> {
        let state = self.lock();
        ensure!(
            state.snapshot.settings.enabled && state.snapshot.ready,
            "本机没有开启访问码"
        );
        Ok(state.admission.clone())
    }
    pub fn touch_ui(&self) {
        self.lock().ui_seen = Some(Instant::now());
    }
    pub fn ui_present(&self) -> bool {
        self.lock()
            .ui_seen
            .is_some_and(|at| at.elapsed() < Duration::from_secs(3))
    }
    pub fn act(&self, action: Action) -> Result<()> {
        match action {
            Action::Refresh => self.refresh(),
            Action::Answer { id, token, allow, trust } => self.answer(id, token, allow, trust),
        }
    }
    pub fn answer(&self, id: String, token: String, allow: bool, trust: bool) -> Result<()> {
        let mut state = self.lock();
        if state.direct.contains_key(&id) {
            ensure!(
                state
                    .snapshot
                    .pending
                    .as_ref()
                    .is_some_and(|p| p.id == id && p.token == token),
                "连接请求已结束"
            );
            let pending = state.snapshot.pending.take();
            let pairing = pending.as_ref().is_some_and(|p| p.pairing.is_some());
            let trust = allow && (trust || pairing);
            let decision = match (allow, trust) {
                (false, _) => Decision::Reject,
                (true, false) => Decision::Allow,
                (true, true) => Decision::Trust,
            };
            if let Some(sender) = state.direct.remove(&id)
                && sender.send(decision).is_ok()
                && trust
                && let Some(pending) = pending
            {
                // A paired device gets full control, as the other side
                // gives this one.
                let grants = if pairing {
                    farwing_protocol::caps::all()
                } else {
                    state.snapshot.settings.grants()
                };
                drop(state);
                self::trust(&pending.name, &pending.device_id, &pending.public_key, grants, pending.caps_limit);
            }
            return Ok(());
        }
        ensure!(
            state
                .snapshot
                .pending
                .as_ref()
                .is_some_and(|p| p.id == id && p.token == token && !p.responding),
            "连接请求已结束"
        );
        state
            .requests
            .as_ref()
            .context("访问码后台未就绪")?
            .try_send(Push::Answer { id, token, allow, trust })
            .map_err(|_| anyhow::anyhow!("访问码后台忙，请稍后重试"))
    }
    pub fn refresh(&self) -> Result<()> {
        self.lock()
            .requests
            .as_ref()
            .context("访问码后台未就绪")?
            .try_send(Push::Refresh)
            .map_err(|_| anyhow::anyhow!("访问码后台忙，请稍后重试"))
    }
    /// The PAKE key of the assistance request granted to `controller_device`
    /// (at most once, within two minutes of the grant).
    pub fn take_pake_key(&self, controller_device: &str) -> Option<[u8; 32]> {
        let mut state = self.lock();
        state.keys.retain(|_, (_, at)| at.elapsed() < KEY_LIFETIME);
        state.keys.remove(controller_device).map(|(key, _)| key)
    }
    pub fn push(&self, value: &farwing_protocol::realtime::Envelope) -> bool {
        use farwing_protocol::realtime::{
            AssistAcceptPush, AssistCancelled, AssistJoinPush, types,
        };
        let event = match value.kind.as_str() {
            types::ASSIST_JOIN => {
                let Ok(push) = serde_json::from_value::<AssistJoinPush>(value.data.clone()) else {
                    return true;
                };
                let alias = sanitize(&push.controller.alias);
                let user = sanitize(push.controller.display_name());
                Push::Join {
                    id: push.request_id,
                    name: match (user.is_empty(), alias.is_empty()) {
                        (false, false) => format!("{user} · {alias}"),
                        (false, true) => user,
                        (true, false) => alias,
                        (true, true) => "远端设备".into(),
                    },
                    caps_limit: push.controller.effective_caps_limit(),
                    device_id: push.controller.device_id,
                    public_key: push.controller.public_key,
                    pake: push.pake,
                }
            }
            types::ASSIST_ACCEPT => {
                let Ok(push) = serde_json::from_value::<AssistAcceptPush>(value.data.clone())
                else {
                    return true;
                };
                Push::Accept {
                    id: push.request_id,
                    key_confirm: push.key_confirm,
                }
            }
            types::ASSIST_CANCELLED => {
                let Ok(push) = serde_json::from_value::<AssistCancelled>(value.data.clone()) else {
                    return true;
                };
                Push::Cancel(push.request_id)
            }
            _ => return false,
        };
        let id = match &event {
            Push::Join { id, .. } | Push::Accept { id, .. } | Push::Cancel(id) => id,
            _ => return true,
        };
        if !farwing_protocol::ids::is_uuid(id) {
            return true;
        }
        if let Some(requests) = &self.lock().requests {
            let _ = requests.try_send(event);
        }
        true
    }
}

pub(crate) struct Running {
    cancel: CancellationToken,
    task: JoinHandle<()>,
    handle: Handle,
}
impl Drop for Running {
    fn drop(&mut self) {
        self.cancel.cancel();
        self.task.abort();
        self.handle.unavailable();
        self.handle.lock().requests = None;
    }
}
impl Running {
    pub fn start(client: HostClient) -> Self {
        let handle = client.host.assistance.clone();
        let (tx, rx) = mpsc::channel(32);
        handle.lock().requests = Some(tx);
        let cancel = CancellationToken::new();
        let stop = cancel.clone();
        let task = tokio::spawn(async move {
            run(client, rx, stop).await;
        });
        Self {
            cancel,
            task,
            handle,
        }
    }
}

fn sanitize(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect::<String>()
        .trim()
        .to_owned()
}

/// How long a granted request's PAKE key waits for its session.
const KEY_LIFETIME: Duration = Duration::from_secs(120);
/// Local brute-force guard: this many wrong codes within a minute block
/// assistance for five minutes.
const FAILURE_LIMIT: usize = 10;
const FAILURE_WINDOW: Duration = Duration::from_secs(60);
const LOCKOUT: Duration = Duration::from_secs(300);

/// One request between `assist.join` and the decision.
struct Request {
    name: String,
    device_id: String,
    public_key: String,
    /// A device this host trusts: admitted by its key.
    trusted: bool,
    mode: Mode,
    /// Shared keys per PAKE slot; `None` for slots without a configured code.
    keys: Vec<Option<[u8; 32]>>,
    /// Key that matched the controller's confirmation.
    granted_key: Option<[u8; 32]>,
    created: Instant,
    caps_limit: Option<Vec<String>>,
}

impl Request {
    /// A code attempt whose key was never confirmed.
    fn unproven(&self) -> bool {
        !self.trusted
            && self.mode.needs_password()
            && !self.keys.is_empty()
            && self.granted_key.is_none()
    }
}

/// The host half of the PAKE: one SPAKE2 instance per controller slot.
fn answer_pake(
    settings: &Settings,
    id: &str,
    pake: &[String],
) -> (Vec<String>, Vec<String>, Vec<Option<[u8; 32]>>) {
    answer_slots(settings, pake, |key| crypto::key_confirm(key, "host", id))
}

fn answer_slots(
    settings: &Settings,
    pake: &[String],
    confirm: impl Fn(&[u8; 32]) -> String,
) -> (Vec<String>, Vec<String>, Vec<Option<[u8; 32]>>) {
    let codes = settings.slot_codes();
    let mut messages = Vec::new();
    let mut confirms = Vec::new();
    let mut keys = Vec::new();
    for (slot, controller_message) in pake.iter().take(crypto::ASSIST_PAKE_SLOTS).enumerate() {
        // A slot without a code still answers with a random one, so replies
        // do not reveal which codes this host has configured.
        let throwaway;
        let code = match codes[slot] {
            Some(code) => code,
            None => {
                throwaway = uuid::Uuid::new_v4().simple().to_string();
                throwaway.as_str()
            }
        };
        let (state, message) = crypto::pake_start(code);
        let key = state.finish(controller_message).ok();
        let confirm_key = key.unwrap_or_else(|| rand::rng().random());
        messages.push(message);
        confirms.push(confirm(&confirm_key));
        keys.push(key.filter(|_| codes[slot].is_some()));
    }
    (messages, confirms, keys)
}

enum Completion {
    Identity(Result<String>),
    Reply {
        revision: u64,
        request: Option<String>,
        result: Result<()>,
    },
}

fn decision(allowed: bool, refusal: Option<AssistRefusal>) -> AssistDecision {
    AssistDecision {
        allowed,
        refusal: if allowed { None } else { refusal },
        trusted: false,
    }
}

fn report_failure(state: &mut State, error: &anyhow::Error) {
    let code = error
        .downcast_ref::<crate::account::api::ApiFailure>()
        .map(|e| e.code);
    tracing::warn!(code = ?code, "host assistance request failed");
    state.snapshot.error = Some(match code {
        Some(code) => format!("连接请求失败（{code}），请重试"),
        None => "连接请求未完成，请检查网络后重试".into(),
    });
}

async fn run(client: HostClient, mut rx: mpsc::Receiver<Push>, stop: CancellationToken) {
    let handle = client.host.assistance.clone();
    let mut jobs = JoinSet::new();
    let mut identity_attempted = false;
    let mut identity_pending = false;
    let mut requested_revision = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut answered = VecDeque::<(String, Instant)>::new();
    let mut requests = HashMap::<String, Request>::new();
    let ended = client.ended();
    loop {
        let event = tokio::select! {
            biased;
            _=stop.cancelled()=>break,
            _=ended.cancelled()=>break,
            Some(result)=jobs.join_next(), if !jobs.is_empty()=>{
                match result {
                    Ok(Completion::Identity(result)) => {
                        identity_pending = false;
                        let mut state = handle.lock();
                        match result {
                            Ok(id) => state.snapshot.connect_id = id,
                            Err(error) => report_failure(&mut state, &error),
                        }
                    }
                    Ok(Completion::Reply { revision, request, result }) => {
                        let mut state = handle.lock();
                        if revision == state.revision {
                            if let Some(token) = request {
                                if state.snapshot.pending.as_ref().is_some_and(|p|p.token == token) {
                                    state.snapshot.pending = None;
                                    state.pending_deadline = None;
                                    state.snapshot.error = None;
                                }
                            }
                            if let Err(error) = result { report_failure(&mut state, &error); }
                        }
                    }
                    Err(_) => {
                        handle.unavailable();
                        handle.lock().snapshot.error = Some("访问码后台已停止，请重新打开程序".into());
                        break;
                    }
                }
                continue;
            }
            value=rx.recv()=>match value {Some(v)=>Some(v),None=>break},
            _=tick.tick()=>None,
        };
        let allowed = client.is_active() && client.host.requested() && client.host.status().ready;
        let (settings, guard, revision, ui_present) = {
            let mut state = handle.lock();
            let ready = allowed && state.snapshot.settings.enabled;
            if state.snapshot.ready && !ready {
                state.admission.cancel();
                state.admission = CancellationToken::new();
            }
            state.snapshot.ready = ready;
            (
                state.snapshot.settings.clone(),
                state.operations.clone(),
                state.revision,
                state
                    .ui_seen
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(3)),
            )
        };
        if requested_revision != revision {
            requested_revision = revision;
            identity_attempted = false;
        }
        if matches!(event, Some(Push::Refresh)) {
            identity_attempted = false;
            handle.lock().snapshot.error = None;
        }
        // The short number exists only on a server; without one, visitors
        // and trusted devices connect directly (§12.2).
        if allowed
            && !client.offline()
            && !identity_attempted
            && !identity_pending
            && jobs.len() < 8
            && handle.lock().snapshot.connect_id.is_empty()
        {
            identity_attempted = true;
            identity_pending = true;
            let current = client.clone();
            jobs.spawn(async move { Completion::Identity(current.host_assist_identity().await) });
        }
        while answered
            .front()
            .is_some_and(|(_, at)| at.elapsed() > Duration::from_secs(120))
        {
            answered.pop_front();
        }
        // A code attempt that is never proven counts as a wrong code: a
        // controller learns whether its guess matched from the PAKE reply
        // alone, so attempts must be limited here, not only on `accept`.
        requests.retain(|_, request| {
            let live = request.created.elapsed() < Duration::from_secs(120);
            if !live && request.unproven() {
                handle.record_failure();
            }
            live
        });
        let locked = handle.locked();
        let expired = {
            let mut state = handle.lock();
            if !allowed
                || state
                    .attempt_deadline
                    .is_some_and(|at| Instant::now() >= at)
            {
                state.snapshot.attempt = None;
                state.attempt_deadline = None;
            }
            if state
                .pending_deadline
                .is_some_and(|at| Instant::now() >= at)
                || (!allowed && state.snapshot.pending.is_some())
            {
                // A direct request times out on its own connection.
                state
                    .snapshot
                    .pending
                    .as_ref()
                    .filter(|p| !p.responding && !state.direct.contains_key(&p.id))
                    .map(|p| (p.id.clone(), p.token.clone()))
            } else {
                None
            }
        };
        let mut events = VecDeque::new();
        if let Some((id, token)) = expired {
            events.push_back(Push::Answer {
                id,
                token,
                allow: false,
                trust: false,
            });
        }
        if let Some(event) = event {
            events.push_back(event);
        }
        for event in events {
            let accept = allowed && settings.enabled && !locked;
            let mut outgoing = None;
            let mut answered_request = None;
            match event {
                Push::Refresh => (),
                Push::Reject(id) => {
                    requests.remove(&id);
                    outgoing = Some(Reply::Decision(
                        id.clone(),
                        decision(false, Some(AssistRefusal::Unavailable)),
                    ));
                    answered.push_back((id, Instant::now()));
                }
                Push::Cancel(id) => {
                    if requests.remove(&id).is_some_and(|request| request.unproven()) {
                        handle.record_failure();
                    }
                    let mut state = handle.lock();
                    if state.snapshot.pending.as_ref().is_some_and(|p| p.id == id) {
                        state.snapshot.pending = None;
                        state.pending_deadline = None;
                    }
                    if state.snapshot.attempt.as_ref().is_some_and(|p| p.id == id) {
                        state.snapshot.attempt = None;
                        state.attempt_deadline = None;
                    }
                    answered.push_back((id, Instant::now()));
                }
                Push::Join {
                    id,
                    name,
                    device_id,
                    public_key,
                    pake,
                    caps_limit,
                } => {
                    if answered.iter().any(|(known, _)| known == &id) || requests.contains_key(&id)
                    {
                        continue;
                    }
                    let usable = settings.validate().is_ok();
                    // A device this host trusts needs neither a code nor the
                    // user: the host only has to allow connections at all.
                    let trusted = pake.is_empty()
                        && allowed
                        && requests.len() < 16
                        && trusted_device(&device_id, &public_key).is_some();
                    if trusted {
                        requests.insert(
                            id.clone(),
                            Request {
                                name,
                                device_id,
                                public_key,
                                trusted: true,
                                mode: settings.mode,
                                keys: Vec::new(),
                                granted_key: None,
                                created: Instant::now(),
                                caps_limit,
                            },
                        );
                        outgoing = Some(Reply::Pake(
                            id,
                            AssistPakeReply {
                                allowed: true,
                                mode: settings.mode.wire(),
                                pake: Vec::new(),
                                key_confirm: Vec::new(),
                                trusted: true,
                            },
                        ));
                    } else if !accept || !usable || requests.len() >= 16 {
                        outgoing = Some(Reply::Pake(
                            id.clone(),
                            AssistPakeReply {
                                allowed: false,
                                mode: settings.mode.wire(),
                                pake: Vec::new(),
                                key_confirm: Vec::new(),
                                trusted: false,
                            },
                        ));
                        answered.push_back((id, Instant::now()));
                    } else {
                        let (messages, confirms, keys) = if settings.mode.needs_password() {
                            answer_pake(&settings, &id, &pake)
                        } else {
                            Default::default()
                        };
                        {
                            let mut state = handle.lock();
                            if state.snapshot.pending.is_none() {
                                state.snapshot.attempt = Some(Attempt {
                                    id: id.clone(),
                                    verifying: !pake.is_empty(),
                                    expires_at: chrono::Utc::now().timestamp() + 30,
                                });
                                state.attempt_deadline =
                                    Some(Instant::now() + Duration::from_secs(30));
                            }
                        }
                        requests.insert(
                            id.clone(),
                            Request {
                                name,
                                device_id,
                                public_key,
                                trusted: false,
                                mode: settings.mode,
                                keys,
                                granted_key: None,
                                created: Instant::now(),
                                caps_limit,
                            },
                        );
                        outgoing = Some(Reply::Pake(
                            id,
                            AssistPakeReply {
                                allowed: true,
                                mode: settings.mode.wire(),
                                pake: messages,
                                key_confirm: confirms,
                                trusted: false,
                            },
                        ));
                    }
                }
                Push::Accept { id, key_confirm } => {
                    if answered.iter().any(|(known, _)| known == &id) {
                        continue;
                    }
                    let mut state = handle.lock();
                    if state.snapshot.pending.as_ref().is_some_and(|p| p.id == id) {
                        continue;
                    }
                    if !requests.contains_key(&id) {
                        drop(state);
                        outgoing = Some(Reply::Decision(
                            id.clone(),
                            decision(false, Some(AssistRefusal::Unavailable)),
                        ));
                        answered.push_back((id, Instant::now()));
                    } else if requests.get(&id).is_some_and(|r| r.trusted) {
                        // Still trusted and still allowed: admitted by key.
                        let request = requests.remove(&id).expect("request checked above");
                        let admitted =
                            allowed && trusted_device(&request.device_id, &request.public_key).is_some();
                        outgoing = Some(Reply::Decision(
                            id.clone(),
                            AssistDecision {
                                allowed: admitted,
                                refusal: (!admitted).then_some(AssistRefusal::Unavailable),
                                trusted: admitted,
                            },
                        ));
                        answered.push_back((id, Instant::now()));
                    } else {
                        let request = requests.get_mut(&id).expect("request checked above");
                        if state.snapshot.attempt.as_ref().is_some_and(|p| p.id == id) {
                            state.snapshot.attempt = None;
                            state.attempt_deadline = None;
                        }
                        // The mode is the one this request was answered under; a
                        // settings change in between rejects it (Push::Reject).
                        let refusal = if !accept || request.mode != settings.mode {
                            Some(AssistRefusal::Unavailable)
                        } else if request.mode.needs_password() {
                            request.granted_key =
                                request.keys.iter().flatten().copied().find(|key| {
                                    crypto::key_confirm_matches(
                                        key,
                                        "controller",
                                        &id,
                                        &key_confirm,
                                    )
                                });
                            if request.granted_key.is_some() {
                                None
                            } else {
                                // Includes a request without any code.
                                note_failure(&mut state);
                                Some(AssistRefusal::CodeInvalid)
                            }
                        } else {
                            None
                        };
                        if refusal.is_none() && request.mode.needs_confirmation() {
                            // A confirmation can only be granted by a live user
                            // interface on an unlocked session.
                            if !ui_present
                                || state.snapshot.pending.is_some()
                                || crate::platform::capture::session_locked() != Some(false)
                            {
                                outgoing = Some(Reply::Decision(
                                    id.clone(),
                                    decision(false, Some(AssistRefusal::Rejected)),
                                ));
                                requests.remove(&id);
                                answered.push_back((id, Instant::now()));
                            } else {
                                state.snapshot.pending = Some(Confirmation {
                                    id: id.clone(),
                                    token: uuid::Uuid::new_v4().simple().to_string(),
                                    name: request.name.clone(),
                                    expires_at: chrono::Utc::now().timestamp() + 60,
                                    responding: false,
                                    device_id: request.device_id.clone(),
                                    public_key: request.public_key.clone(),
                                    caps_limit: request.caps_limit.clone(),
                                    pairing: None,
                                });
                                state.pending_deadline =
                                    Some(Instant::now() + Duration::from_secs(60));
                            }
                        } else {
                            if refusal.is_none()
                                && let Some(key) = request.granted_key
                            {
                                state
                                    .keys
                                    .insert(request.device_id.clone(), (key, Instant::now()));
                            }
                            outgoing = Some(Reply::Decision(
                                id.clone(),
                                decision(refusal.is_none(), refusal),
                            ));
                            requests.remove(&id);
                            answered.push_back((id, Instant::now()));
                        }
                    }
                }
                Push::Answer {
                    id,
                    token,
                    allow,
                    trust,
                } => {
                    let mut state = handle.lock();
                    if !state
                        .snapshot
                        .pending
                        .as_ref()
                        .is_some_and(|p| p.id == id && p.token == token && !p.responding)
                    {
                        continue;
                    }
                    let live = state.pending_deadline.is_some_and(|at| Instant::now() < at);
                    if let Some(pending) = &mut state.snapshot.pending {
                        pending.responding = true;
                    }
                    let request = requests.remove(&id);
                    let allowed = allow
                        && accept
                        && live
                        && ui_present
                        && request.is_some()
                        && crate::platform::capture::session_locked() == Some(false);
                    if allowed
                        && let Some(request) = &request
                        && let Some(key) = request.granted_key
                    {
                        state
                            .keys
                            .insert(request.device_id.clone(), (key, Instant::now()));
                    }
                    if allowed
                        && trust
                        && let Some(request) = &request
                    {
                        self::trust(
                            &request.name,
                            &request.device_id,
                            &request.public_key,
                            settings.grants(),
                            request.caps_limit.clone(),
                        );
                    }
                    answered_request = Some(token);
                    outgoing = Some(Reply::Decision(
                        id.clone(),
                        decision(allowed, Some(AssistRefusal::Rejected)),
                    ));
                    answered.push_back((id, Instant::now()));
                }
            }
            while answered.len() > 64 {
                answered.pop_front();
            }
            if let Some(reply) = outgoing {
                if jobs.len() >= 8 && answered_request.is_none() {
                    continue;
                }
                let current = client.clone();
                let guard = guard.clone();
                jobs.spawn(async move {
                    let result = tokio::select! {
                        biased;
                        _=guard.cancelled()=>Ok(()),
                        result=reply.send(&current)=>result,
                    };
                    Completion::Reply {
                        revision,
                        request: answered_request,
                        result,
                    }
                });
            }
        }
    }
    jobs.abort_all();
    while jobs.join_next().await.is_some() {}
}

const LOCKED_MESSAGE: &str = "多次收到错误的访问码，访问码已暂停 5 分钟";

enum Reply {
    Pake(String, AssistPakeReply),
    Decision(String, AssistDecision),
}
impl Reply {
    async fn send(self, client: &HostClient) -> Result<()> {
        match self {
            Self::Pake(id, reply) => client.host_assist_pake(&id, reply).await,
            Self::Decision(id, decision) => client.host_assist_decision(&id, decision).await,
        }
    }
}
