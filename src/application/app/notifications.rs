//! Remote-access notices share one owner, independent of delivery surface.

mod files;
mod worker;
use super::DeviceCenterApp;
use crate::features::host::assist;
use crate::ui::{App, controls, theme};
use std::collections::{HashMap, HashSet};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
    mpsc,
};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum Mode {
    #[default]
    Gui,
    Windows,
    Both,
}
impl Mode {
    pub const ALL: [Self; 3] = [Self::Gui, Self::Windows, Self::Both];
    pub fn label(self) -> &'static str {
        match self {
            Self::Gui => "程序浮窗",
            // The variant keeps its stored name; Linux delivers it through
            // the desktop's notification service.
            Self::Windows if cfg!(windows) => "Windows 通知",
            Self::Windows => "系统通知",
            Self::Both => "两者同时",
        }
    }
    fn gui(self) -> bool {
        self != Self::Windows
    }
    fn windows(self) -> bool {
        self != Self::Gui
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TransferCard {
    pub progress: Option<u16>,
    pub folder: Option<std::path::PathBuf>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Card {
    pub disconnect: Option<String>,
    pub transfer: Option<TransferCard>,
    pub ticket: String,
    pub title: String,
    pub body: String,
    pub detail: String,
    pub expires_at: i64,
    pub confirmation: bool,
    pub busy: bool,
    pub connected: bool,
    /// The connection is a terminal, not a screen; see `ConnectionInfo`.
    pub terminal: bool,
    /// That terminal is in a shell the local user opened on this machine.
    pub local_terminal: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Allow,
    Reject,
    Open,
    Dismiss,
    OpenFolder,
    Disconnect,
}
#[derive(Clone, Copy)]
pub(crate) enum ButtonKind {
    Normal,
    Primary,
    Danger,
}
pub(crate) struct CardAction {
    pub label: &'static str,
    pub verb: Verb,
    pub kind: ButtonKind,
    pub enabled: bool,
}
impl Card {
    pub fn actions(&self) -> Vec<CardAction> {
        if self.confirmation {
            let enabled = !self.busy && self.expires_at > chrono::Utc::now().timestamp();
            return vec![
                CardAction {
                    label: "拒绝",
                    verb: Verb::Reject,
                    kind: ButtonKind::Normal,
                    enabled,
                },
                CardAction {
                    label: if self.busy {
                        "正在提交"
                    } else {
                        "允许本次"
                    },
                    verb: Verb::Allow,
                    kind: ButtonKind::Primary,
                    enabled,
                },
            ];
        }
        let (label, verb) = if self.transfer.as_ref().is_some_and(|t| t.folder.is_some()) {
            ("打开文件夹", Verb::OpenFolder)
        } else {
            (
                if self.expires_at == 0 {
                    "控制中心"
                } else {
                    "查看"
                },
                Verb::Open,
            )
        };
        let mut actions = vec![CardAction {
            label,
            verb,
            kind: ButtonKind::Normal,
            enabled: true,
        }];
        if self.disconnect.is_some() {
            actions.push(CardAction {
                label: "断开连接",
                verb: Verb::Disconnect,
                kind: ButtonKind::Danger,
                enabled: true,
            });
        }
        actions
    }
}
/// 通知按钮回传的协议前缀，也是激活链接里区分「通知」与「协助」的那一段。
pub(crate) const SCHEME: &str = "openuuyc-notification://";
#[derive(Clone)]
pub(crate) struct Action {
    pub ticket: String,
    pub verb: Verb,
}
impl Action {
    pub(crate) fn parse(uri: &str) -> anyhow::Result<Self> {
        let s = uri
            .strip_prefix(SCHEME)
            .ok_or_else(|| anyhow::anyhow!("通知地址无效"))?;
        let (verb, ticket) = s
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!("通知参数无效"))?;
        anyhow::ensure!(
            ticket.len() == 32 && ticket.bytes().all(|c| c.is_ascii_hexdigit()),
            "通知标识无效"
        );
        let verb = match verb {
            "allow" => Verb::Allow,
            "reject" => Verb::Reject,
            "open" => Verb::Open,
            "folder" => Verb::OpenFolder,
            "disconnect" => Verb::Disconnect,
            _ => anyhow::bail!("不支持的通知操作"),
        };
        Ok(Self {
            ticket: ticket.into(),
            verb,
        })
    }
    pub(crate) fn uri(ticket: &str, verb: Verb) -> String {
        let verb = match verb {
            Verb::Allow => "allow",
            Verb::Reject => "reject",
            Verb::OpenFolder => "folder",
            Verb::Disconnect => "disconnect",
            _ => "open",
        };
        format!("{SCHEME}{verb}/{ticket}")
    }
}
struct Target {
    disconnect: Option<String>,
    folder: Option<std::path::PathBuf>,
    generation: u64,
    request: Option<assist::Confirmation>,
}
struct EndNotice {
    id: String,
    peer: String,
    card: Card,
}
#[derive(Default)]
struct EndNotices {
    initialized: bool,
    seen: Option<String>,
    current: Option<EndNotice>,
}
impl EndNotices {
    fn observe(&mut self, notice: Option<EndNotice>, now: i64) {
        if let Some(notice) = notice {
            if self.initialized
                && self.seen.as_ref() != Some(&notice.id)
                && notice.card.expires_at > now
            {
                self.seen = Some(notice.id.clone());
                self.current = Some(notice);
            } else {
                self.seen = Some(notice.id);
            }
        }
        self.initialized = true;
        if self
            .current
            .as_ref()
            .is_some_and(|n| n.card.expires_at <= now)
        {
            self.current = None;
        }
    }
}
pub(super) struct Center {
    pub mode: Mode,
    pub error: Option<String>,
    events: mpsc::Receiver<Action>,
    sender: mpsc::Sender<Action>,
    worker: worker::Worker,
    targets: HashMap<String, Target>,
    cards: Vec<Card>,
    keys: HashMap<String, String>,
    dismissed: HashSet<String>,
    view: Arc<Mutex<Vec<Card>>>,
    alive: Arc<AtomicBool>,
    opening: Arc<AtomicBool>,
    generation: u64,
    mode_revision: u64,
    claimed: HashSet<String>,
    preview: HashSet<String>,
    feedback: Option<(String, i64)>,
    assist_error: Option<String>,
    ended: EndNotices,
    last_source: Option<(String, String)>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|p| p.into_inner())
}
fn clean(s: &str) -> String {
    s.chars()
        .filter(|c| !c.is_control() && !matches!(c,'\u{202a}'..='\u{202e}'|'\u{2066}'..='\u{2069}'))
        .take(100)
        .collect()
}
impl Center {
    pub fn new(ctx: &egui::Context) -> Self {
        let (sender, events) = mpsc::channel();
        crate::platform::notifications::set_activation_sink(Some(sender.clone()));
        Self {
            mode: Mode::Gui,
            error: None,
            worker: worker::Worker::new(ctx.clone()),
            sender,
            events,
            targets: HashMap::new(),
            cards: Vec::new(),
            keys: HashMap::new(),
            dismissed: HashSet::new(),
            view: Default::default(),
            alive: Default::default(),
            opening: Default::default(),
            generation: 0,
            mode_revision: 0,
            claimed: HashSet::new(),
            preview: HashSet::new(),
            feedback: None,
            assist_error: None,
            ended: EndNotices::default(),
            last_source: None,
        }
    }
    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.error = None;
        self.preview.clear();
        self.mode_revision = self.mode_revision.wrapping_add(1);
        self.worker.set_mode(mode, self.mode_revision);
    }
    pub fn reopen(&mut self) {
        self.dismissed.clear();
        self.preview = self.cards.iter().map(|c| c.ticket.clone()).collect();
    }
    pub fn report(&mut self, error: String) {
        self.feedback = Some((error, chrono::Utc::now().timestamp() + 10));
    }
    pub fn available(&self) -> bool {
        !self.cards.is_empty()
    }
    fn claim(&mut self, action: &Action, generation: u64) -> Option<assist::Action> {
        if !matches!(action.verb, Verb::Allow | Verb::Reject)
            || self.claimed.contains(&action.ticket)
        {
            return None;
        }
        let target = self.targets.get(&action.ticket)?;
        if target.generation != generation {
            return None;
        }
        let p = target
            .request
            .as_ref()
            .filter(|p| !p.responding && p.expires_at > chrono::Utc::now().timestamp())?
            .clone();
        self.claimed.insert(action.ticket.clone());
        self.targets.remove(&action.ticket);
        Some(assist::Action::Answer {
            id: p.id,
            token: p.token,
            allow: action.verb == Verb::Allow,
        })
    }
    fn update(
        &mut self,
        generation: u64,
        host: Option<&crate::features::host::Handle>,
        status_snapshot: Option<crate::features::host::Status>,
        caption: Option<crate::ui::chrome::TitleBarAlert>,
        ended: Option<EndNotice>,
    ) {
        for event in self.worker.events.try_iter() {
            match event {
                worker::Event::Mode(mode, revision) => {
                    if revision == self.mode_revision {
                        self.mode = mode;
                    }
                }
                worker::Event::Error(error) => self.error = error,
            }
        }
        if generation != self.generation {
            self.generation = generation;
            self.keys.clear();
            self.dismissed.clear();
            self.claimed.clear();
            self.preview.clear();
            self.feedback = None;
            self.assist_error = None;
            self.targets.clear();
            self.ended = EndNotices::default();
            self.last_source = None;
        }
        if host.is_none() {
            self.feedback = None;
            self.assist_error = None;
            self.ended = EndNotices::default();
            self.last_source = None;
        }
        let now = chrono::Utc::now().timestamp();
        let disconnect = status_snapshot
            .as_ref()
            .and_then(|s| s.disconnect_target())
            .map(str::to_owned);
        let mut items = Vec::new();
        if let Some(host) = host {
            let ended = ended.map(|mut notice| {
                if let Some((peer, source)) = &self.last_source
                    && *peer == notice.peer
                {
                    notice.card.body = source.clone();
                }
                notice
            });
            self.ended.observe(ended, now);
            let snapshot = host.assistance.snapshot();
            if snapshot.error != self.assist_error {
                self.assist_error = snapshot.error.clone();
                if let Some(error) = snapshot.error {
                    self.report(error);
                }
            }
            if let Some(p) = snapshot.pending.filter(|p| p.expires_at > now) {
                let key = format!("confirm:{}", p.token);
                items.push((
                    key,
                    Card {
                        disconnect: None,
                        ticket: String::new(),
                        title: "远程协助请求".into(),
                        body: clean(&p.name),
                        detail: if p.responding {
                            "正在提交处理结果…".into()
                        } else {
                            "请求查看画面并控制键鼠，本次允许后免输验证码。".into()
                        },
                        expires_at: p.expires_at,
                        confirmation: true,
                        busy: p.responding,
                        connected: false,
                        terminal: false,
                        local_terminal: false,
                        transfer: None,
                    },
                    Some(p),
                ));
            } else if let Some(a) = snapshot.attempt.filter(|p| p.expires_at > now) {
                items.push((
                    format!("attempt:{}", a.id),
                    Card {
                        disconnect: None,
                        ticket: String::new(),
                        title: "收到远程连接尝试".into(),
                        body: "对方正在申请连接这台电脑。".into(),
                        detail: if a.verifying {
                            "正在校验验证码，尚未建立控制会话。".into()
                        } else {
                            "等待对方继续连接，当前尚未获得控制权限。".into()
                        },
                        expires_at: a.expires_at,
                        confirmation: false,
                        busy: false,
                        connected: false,
                        terminal: false,
                        local_terminal: false,
                        transfer: None,
                    },
                    None,
                ));
            }
            if let Some(caption) = caption {
                let status = status_snapshot.as_ref().expect("host snapshot");
                let key = status
                    .connection
                    .as_ref()
                    .map(|c| format!("active:{}:{}:{}", c.client_id, c.device_id, c.session_id))
                    .unwrap_or_else(|| "active".into());
                let terminal = status.connection.as_ref().is_some_and(|c| c.terminal);
                let local_terminal =
                    terminal && status.connection.as_ref().is_some_and(|c| c.local_terminal);
                let title = if status
                    .connection
                    .as_ref()
                    .is_some_and(|c| c.observation_lost)
                {
                    "正在确认被控状态"
                } else if status.connected && local_terminal {
                    "对方接入了你本机的终端会话"
                } else if status.connected && terminal {
                    "正在被远程访问终端"
                } else if status.connected {
                    "正在被远程访问"
                } else {
                    "远程连接暂时中断"
                };
                // A terminal is not a screen being watched: say what the
                // other side can do, next to how long it has been connected.
                let detail = if local_terminal {
                    format!("对方能看到并操作你在本机打开的终端 · {}", caption.duration)
                } else if terminal {
                    format!("终端访问，对方可在本机执行命令 · {}", caption.duration)
                } else {
                    caption.duration
                };
                if status.connected
                    && !status
                        .connection
                        .as_ref()
                        .is_some_and(|c| c.observation_lost)
                {
                    self.last_source = Some((key.clone(), clean(&caption.source)));
                }
                items.push((
                    key,
                    Card {
                        disconnect: disconnect.clone(),
                        ticket: String::new(),
                        title: title.into(),
                        body: clean(&caption.source),
                        detail,
                        expires_at: 0,
                        confirmation: false,
                        busy: false,
                        connected: status.connected,
                        terminal,
                        local_terminal,
                        transfer: None,
                    },
                    None,
                ));
            }
        }
        if let Some(snapshot) = status_snapshot {
            for (key, mut card) in files::cards(snapshot.file_transfers, now) {
                if card.transfer.as_ref().is_some_and(|t| t.progress.is_some()) {
                    card.disconnect = disconnect.clone();
                }
                items.push((key, card, None));
            }
        }
        if let Some(notice) = &self.ended.current {
            items.push((format!("ended:{}", notice.id), notice.card.clone(), None));
        }
        if let Some((error, until)) = self.feedback.as_ref().filter(|(_, until)| *until > now) {
            items.push((
                format!("feedback:{until}"),
                Card {
                    disconnect: None,
                    ticket: String::new(),
                    title: "远程协助提示".into(),
                    body: clean(error),
                    detail: "请查看当前请求状态。".into(),
                    expires_at: *until,
                    confirmation: false,
                    busy: false,
                    connected: false,
                    terminal: false,
                    local_terminal: false,
                    transfer: None,
                },
                None,
            ));
        }
        let valid: HashSet<_> = items.iter().map(|(key, _, _)| key.clone()).collect();
        self.keys.retain(|key, _| valid.contains(key));
        let mut cards = Vec::new();
        let mut targets = HashMap::new();
        for (key, mut card, request) in items {
            let ticket = self
                .keys
                .entry(key)
                .or_insert_with(|| uuid::Uuid::new_v4().simple().to_string())
                .clone();
            card.ticket = ticket.clone();
            card.busy |= self.claimed.contains(&ticket);
            if card.confirmation && card.busy {
                card.detail = "正在提交处理结果…".into();
            }
            targets.insert(
                ticket,
                Target {
                    disconnect: card.disconnect.clone(),
                    folder: card.transfer.as_ref().and_then(|t| t.folder.clone()),
                    generation,
                    request,
                },
            );
            cards.push(card);
        }
        self.dismissed.retain(|t| targets.contains_key(t));
        self.targets = targets;
        self.claimed.retain(|t| self.targets.contains_key(t));
        self.preview.retain(|t| self.targets.contains_key(t));
        self.cards = cards;
        let shown = self
            .cards
            .iter()
            .filter(|c| {
                (self.mode.gui() || self.preview.contains(&c.ticket))
                    && !self.dismissed.contains(&c.ticket)
            })
            .cloned()
            .collect();
        *lock(&self.view) = shown;
        self.worker.update(&self.cards);
        if !lock(&self.view).is_empty()
            && !self.alive.load(Ordering::Acquire)
            && !self.opening.swap(true, Ordering::AcqRel)
        {
            let opening = Opening(self.opening.clone());
            let view = self.view.clone();
            let alive = self.alive.clone();
            let sender = self.sender.clone();
            let result =
                crate::ui::window_manager::send(crate::ui::window_manager::Request::Open {
                    key: "remote-access-notifications".into(),
                    config: crate::ui::WindowConfig {
                        viewport: egui::ViewportBuilder::default()
                            .with_title("OpenUUYC · 远程访问")
                            .with_inner_size([theme::NOTIFICATION_WIDTH, 220.])
                            .with_resizable(false)
                            .with_active(false)
                            .with_visible(true),
                        centered: false,
                        notification: true,
                        floating: false,
                    },
                    factory: Box::new(move |ctx, _| {
                        theme::configure(ctx);
                        alive.store(true, Ordering::Release);
                        drop(opening);
                        Box::new(Popup {
                            view,
                            alive,
                            sender,
                        })
                    }),
                });
            if let Err(error) = result {
                self.error = Some(error.to_string());
            }
        }
    }
}
// Also resets when window creation fails before consuming the factory.
struct Opening(Arc<AtomicBool>);
impl Drop for Opening {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
impl Drop for Center {
    fn drop(&mut self) {
        lock(&self.view).clear();
        crate::platform::notifications::set_activation_sink(None);
    }
}
struct Popup {
    view: Arc<Mutex<Vec<Card>>>,
    alive: Arc<AtomicBool>,
    sender: mpsc::Sender<Action>,
}
impl Drop for Popup {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}
impl App for Popup {
    fn uses_tray(&self) -> bool {
        false
    }
    fn on_close_requested(&mut self) -> bool {
        for c in lock(&self.view).iter() {
            let _ = self.sender.send(Action {
                ticket: c.ticket.clone(),
                verb: Verb::Dismiss,
            });
        }
        true
    }
    fn ui(&mut self, ui: &mut egui::Ui) {
        let cards = lock(&self.view).clone();
        if cards.is_empty() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
            return;
        }
        // The card frames do not cover the complete native surface while its
        // size changes. Paint the window too, including any scroll-area gutter.
        ui.painter().rect_filled(ui.max_rect(), 0., theme::BG);
        ui.spacing_mut().item_spacing.y = 0.;
        let content = egui::ScrollArea::vertical()
            .auto_shrink([false, true])
            .show(ui, |ui| {
                for (index, card) in cards.iter().enumerate() {
                    if index > 0 {
                        let (rect, _) = ui.allocate_exact_size(
                            egui::vec2(ui.available_width(), 1.),
                            egui::Sense::hover(),
                        );
                        ui.painter().rect_filled(rect, 0., theme::LINE);
                    }
                    if let Some(verb) = controls::host_notice(ui, card) {
                        let _ = self.sender.send(Action {
                            ticket: card.ticket.clone(),
                            verb,
                        });
                    }
                }
            });
        // Item spacing already separates cards. Trailing spacers plus another
        // window margin used to leave an unpainted strip below the last card.
        let height = content.content_size.y.ceil().clamp(
            theme::NOTIFICATION_MIN_HEIGHT,
            theme::NOTIFICATION_MAX_HEIGHT,
        );
        if ui.ctx().input(|i| {
            i.viewport()
                .inner_rect
                .is_some_and(|r| (r.height() - height).abs() > 1.)
        }) {
            ui.ctx()
                .send_viewport_cmd(egui::ViewportCommand::InnerSize(egui::vec2(
                    theme::NOTIFICATION_WIDTH,
                    height,
                )));
        }
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(250));
    }
}
impl DeviceCenterApp {
    pub(super) fn tick_notifications(&mut self) {
        let host = if self.exit_requested || self.logout_pending {
            None
        } else {
            self.host.as_ref()
        };
        let snapshot = host.map(|h| h.status());
        let caption = snapshot.as_ref().and_then(|status| {
            super::controlled_caption::caption(status, self.devices.as_ref(), self.catalog.as_ref())
        });
        let ended = snapshot
            .as_ref()
            .and_then(|s| s.ended_connection.clone())
            .map(|end| {
                let seconds = end.connection.elapsed_seconds.unwrap_or(0);
                let terminal = end.connection.terminal;
                let peer = format!(
                    "active:{}:{}:{}",
                    end.connection.client_id, end.connection.device_id, end.connection.session_id
                );
                let status = crate::features::host::Status {
                    connected: true,
                    session_active: true,
                    assistance: end.assistance,
                    connection: Some(end.connection),
                    ..Default::default()
                };
                let source = super::controlled_caption::caption(
                    &status,
                    self.devices.as_ref(),
                    self.catalog.as_ref(),
                )
                .map(|c| c.source)
                .unwrap_or_else(|| "远程设备".into());
                EndNotice {
                    id: end.id,
                    peer,
                    card: Card {
                        disconnect: None,
                        ticket: String::new(),
                        title: if terminal {
                            "终端连接已结束"
                        } else {
                            "远程连接已结束"
                        }
                        .into(),
                        body: clean(&source),
                        detail: format!(
                            "本次连接 {}",
                            super::controlled_caption::duration(seconds)
                        ),
                        expires_at: end.ended_at.saturating_add(10),
                        confirmation: false,
                        busy: false,
                        connected: false,
                        terminal: false,
                        local_terminal: false,
                        transfer: None,
                    },
                }
            });
        self.notifications
            .update(self.login_generation, host, snapshot, caption, ended);
        while let Ok(action) = self.notifications.events.try_recv() {
            let Some(target) = self.notifications.targets.get(&action.ticket) else {
                continue;
            };
            if target.generation != self.login_generation
                || self.exit_requested
                || self.logout_pending
            {
                continue;
            }
            match action.verb {
                Verb::Disconnect => {
                    if let (Some(session), Some(host)) =
                        (target.disconnect.as_deref(), self.host.as_ref())
                    {
                        host.disconnect_session(session);
                    }
                }
                Verb::Dismiss => {
                    self.notifications.dismissed.insert(action.ticket);
                }
                Verb::OpenFolder => {
                    if let Some(folder) = target.folder.clone() {
                        self.notifications.worker.open_folder(folder);
                    }
                }
                Verb::Open => {
                    let _ = crate::ui::window_manager::send(
                        crate::ui::window_manager::Request::ShowMain,
                    );
                }
                Verb::Allow | Verb::Reject => {
                    if let Some(answer) = self.notifications.claim(&action, self.login_generation) {
                        if self
                            .worker
                            .commands
                            .send(super::messages::GuiCommand::HostAssist {
                                generation: self.login_generation,
                                action: answer,
                            })
                            .is_err()
                        {
                            self.notifications.error = Some("协助后台已停止".into());
                        }
                    }
                }
            }
        }
    }
}
