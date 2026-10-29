//! Remote-access notices share one owner, independent of delivery surface.

mod files;
mod worker;
use super::DeviceCenterApp;
use crate::features::host::assist;
use crate::i18n::t;
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
        let text = &t().app;
        match self {
            Self::Gui => text.mode_gui,
            Self::Windows => text.mode_windows,
            Self::Both => text.mode_both,
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
    /// When the notification first appeared (Unix seconds).
    pub at: i64,
    pub title: String,
    pub body: String,
    pub detail: String,
    pub expires_at: i64,
    pub confirmation: bool,
    /// A pairing request: its answers are "deny" and "pair".
    pub pairing: bool,
    /// Let in, never trusted: a browser (often someone else's next) or a
    /// participant asking for control.
    pub no_trust: bool,
    pub busy: bool,
    pub connected: bool,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verb {
    Allow,
    /// Allow and trust the asking device from now on.
    Trust,
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
        let text = &t().app;
        if self.confirmation && self.pairing {
            let enabled = !self.busy && self.expires_at > chrono::Utc::now().timestamp();
            return vec![
                CardAction {
                    label: t().common.deny,
                    verb: Verb::Reject,
                    kind: ButtonKind::Normal,
                    enabled,
                },
                CardAction {
                    label: if self.busy { text.submitting } else { text.pair },
                    verb: Verb::Trust,
                    kind: ButtonKind::Primary,
                    enabled,
                },
            ];
        }
        if self.confirmation {
            let enabled = !self.busy && self.expires_at > chrono::Utc::now().timestamp();
            let mut actions = vec![CardAction {
                label: t().common.deny,
                verb: Verb::Reject,
                kind: ButtonKind::Normal,
                enabled,
            }];
            if !self.no_trust {
                actions.push(CardAction {
                    label: text.allow_and_trust,
                    verb: Verb::Trust,
                    kind: ButtonKind::Normal,
                    enabled,
                });
            }
            actions.extend([
                CardAction {
                    label: if self.busy {
                        text.submitting
                    } else {
                        text.allow_once
                    },
                    verb: Verb::Allow,
                    kind: ButtonKind::Primary,
                    enabled,
                },
            ]);
            return actions;
        }
        let (label, verb) = if self.transfer.as_ref().is_some_and(|t| t.folder.is_some()) {
            (text.open_folder, Verb::OpenFolder)
        } else {
            (
                if self.expires_at == 0 {
                    text.control_center
                } else {
                    text.view
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
                label: t().common.disconnect,
                verb: Verb::Disconnect,
                kind: ButtonKind::Danger,
                enabled: true,
            });
        }
        actions
    }
}
#[derive(Clone)]
pub(crate) struct Action {
    pub ticket: String,
    pub verb: Verb,
}
impl Action {
    pub(crate) fn parse(uri: &str) -> anyhow::Result<Self> {
        let prefix = format!("{}://", crate::product::notification_scheme());
        let s = uri
            .strip_prefix(prefix.as_str())
            .ok_or_else(|| anyhow::anyhow!(t().app.invalid_notification_uri))?;
        let (verb, ticket) = s
            .split_once('/')
            .ok_or_else(|| anyhow::anyhow!(t().app.invalid_notification_parameters))?;
        anyhow::ensure!(
            ticket.len() == 32 && ticket.bytes().all(|c| c.is_ascii_hexdigit()),
            t().app.invalid_notification_id
        );
        let verb = match verb {
            "allow" => Verb::Allow,
            "trust" => Verb::Trust,
            "reject" => Verb::Reject,
            "open" => Verb::Open,
            "folder" => Verb::OpenFolder,
            "disconnect" => Verb::Disconnect,
            _ => anyhow::bail!(t().app.unsupported_notification_action),
        };
        Ok(Self {
            ticket: ticket.into(),
            verb,
        })
    }
    pub(crate) fn uri(ticket: &str, verb: Verb) -> String {
        let verb = match verb {
            Verb::Allow => "allow",
            Verb::Trust => "trust",
            Verb::Reject => "reject",
            Verb::OpenFolder => "folder",
            Verb::Disconnect => "disconnect",
            _ => "open",
        };
        format!(
            "{}://{verb}/{ticket}",
            crate::product::notification_scheme()
        )
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
/// A "being accessed" card: one per participant.
struct Active {
    key: String,
    disconnect: Option<String>,
    caption: crate::ui::chrome::TitleBarAlert,
    lost: bool,
}
fn active_key(client_id: &str, device_id: &str, session_id: &str) -> String {
    format!("active:{client_id}:{device_id}:{session_id}")
}
fn active_cards(
    status: &crate::features::host::Status,
    devices: Option<&crate::account::api::DeviceList>,
    catalog: Option<&super::catalog::Catalog>,
) -> Vec<Active> {
    use super::controlled_caption::{caption, participant_status};
    if status.connections.len() > 1 {
        // Each card ends exactly its own participant.
        return (0..status.connections.len())
            .filter_map(|index| {
                let single = participant_status(status, index)?;
                let connection = single.connection.as_ref()?;
                Some(Active {
                    key: active_key(
                        &connection.client_id,
                        &connection.device_id,
                        &connection.session_id,
                    ),
                    disconnect: single.disconnect_target().map(str::to_owned),
                    caption: caption(&single, devices, catalog)?,
                    lost: connection.observation_lost,
                })
            })
            .collect();
    }
    caption(status, devices, catalog)
        .map(|caption| Active {
            key: status
                .connection
                .as_ref()
                .map(|c| active_key(&c.client_id, &c.device_id, &c.session_id))
                .unwrap_or_else(|| "active".into()),
            disconnect: status.disconnect_target().map(str::to_owned),
            caption,
            lost: status
                .connection
                .as_ref()
                .is_some_and(|c| c.observation_lost),
        })
        .into_iter()
        .collect()
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
    /// Each card's ticket and when it first appeared.
    keys: HashMap<String, (String, i64)>,
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
    /// Each participant's last shown source, for its end notice.
    last_source: HashMap<String, String>,
    /// The account's notifications (`docs/notifications.md`), by ID.
    account: Vec<(i64, Card)>,
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
        crate::platform::windows::notifications::set_activation_sink(Some(sender.clone()));
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
            last_source: HashMap::new(),
            account: Vec::new(),
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
    /// One of the account's notifications, shown for a while.
    pub fn show_notice(&mut self, id: i64, title: String, body: String) {
        self.account.retain(|(other, _)| *other != id);
        self.account.push((
            id,
            Card {
                disconnect: None,
                ticket: String::new(),
                at: 0,
                title: clean(&title),
                body: clean(&body),
                detail: String::new(),
                expires_at: chrono::Utc::now().timestamp() + 12,
                confirmation: false,
                pairing: false,
                no_trust: false,
                busy: false,
                connected: false,
                transfer: None,
            },
        ));
    }
    pub fn report(&mut self, error: String) {
        self.feedback = Some((error, chrono::Utc::now().timestamp() + 10));
    }
    pub fn available(&self) -> bool {
        !self.cards.is_empty()
    }
    fn claim(&mut self, action: &Action, generation: u64) -> Option<assist::Action> {
        if !matches!(action.verb, Verb::Allow | Verb::Trust | Verb::Reject)
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
            allow: action.verb != Verb::Reject,
            trust: action.verb == Verb::Trust,
        })
    }
    fn update(
        &mut self,
        generation: u64,
        host: Option<&crate::features::host::Handle>,
        status_snapshot: Option<crate::features::host::Status>,
        active: Vec<Active>,
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
            self.last_source.clear();
        }
        if host.is_none() {
            self.feedback = None;
            self.assist_error = None;
            self.ended = EndNotices::default();
            self.last_source.clear();
        }
        let now = chrono::Utc::now().timestamp();
        let text = &t().app;
        // Transfers are not tied to a participant: only end the only one.
        let disconnect = status_snapshot
            .as_ref()
            .filter(|s| s.connections.len() <= 1)
            .and_then(|s| s.disconnect_target())
            .map(str::to_owned);
        let mut items = Vec::new();
        if let Some(host) = host {
            let ended = ended.map(|mut notice| {
                if let Some(source) = self.last_source.get(&notice.peer) {
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
                // What the visitor gets once allowed: the visitor
                // permissions, within the server's limit on browsers.
                let browser = p.caps_limit.is_some();
                let gets = snapshot
                    .settings
                    .grants()
                    .into_iter()
                    .filter(|c| p.caps_limit.as_ref().is_none_or(|limit| limit.contains(c)))
                    .map(|c| super::friends::cap_label(&c))
                    .collect::<Vec<_>>()
                    .join("、");
                let pairing = p.pairing.clone();
                items.push((
                    key,
                    Card {
                        disconnect: None,
                        ticket: String::new(),
                at: 0,
                        title: if pairing.is_some() {
                            text.pairing_request
                        } else if p.control {
                            text.control_request
                        } else {
                            text.connection_request
                        }
                        .into(),
                        body: if p.device_id.is_empty() {
                            clean(&p.name)
                        } else {
                            (text.name_with_device_id)(
                                &clean(&p.name),
                                &crate::account::api::device_id_text(&p.device_id),
                            )
                        },
                        detail: if p.responding {
                            text.submitting_ellipsis.into()
                        } else if let Some(code) = &pairing {
                            (text.pairing_code)(
                                &code[..code.len().min(3)],
                                &code[code.len().min(3)..],
                            )
                        } else if p.control {
                            text.control_this_session.into()
                        } else if browser {
                            (text.browser_gets)(&gets)
                        } else {
                            (text.visitor_gets)(&gets)
                        },
                        expires_at: p.expires_at,
                        confirmation: true,
                        pairing: pairing.is_some(),
                        no_trust: browser || p.control,
                        busy: p.responding,
                        connected: false,
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
                at: 0,
                        title: text.connection_attempt.into(),
                        body: text.incoming_connection.into(),
                        detail: if a.verifying {
                            text.verifying_access_code.into()
                        } else {
                            text.no_control_permission.into()
                        },
                        expires_at: a.expires_at,
                        confirmation: false,
                        pairing: false,
                        no_trust: false,
                        busy: false,
                        connected: false,
                        transfer: None,
                    },
                    None,
                ));
            }
            let connected = status_snapshot.as_ref().is_some_and(|s| s.connected);
            let keys: HashSet<_> = active.iter().map(|a| a.key.clone()).collect();
            for active in active {
                let title = if active.lost {
                    text.confirming_host
                } else if connected {
                    text.being_accessed
                } else {
                    text.remote_connection_lost
                };
                if connected && !active.lost {
                    self.last_source
                        .insert(active.key.clone(), clean(&active.caption.source));
                }
                items.push((
                    active.key,
                    Card {
                        disconnect: active.disconnect,
                        ticket: String::new(),
                at: 0,
                        title: title.into(),
                        body: clean(&active.caption.source),
                        detail: active.caption.duration,
                        expires_at: 0,
                        confirmation: false,
                        pairing: false,
                        no_trust: false,
                        busy: false,
                        connected,
                        transfer: None,
                    },
                    None,
                ));
            }
            // A departed participant's source outlives its card until its
            // end notice was shown; bound what is kept.
            if self.last_source.len() > 16 {
                self.last_source.retain(|key, _| keys.contains(key));
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
                at: 0,
                    title: text.connection_notice.into(),
                    body: clean(error),
                    detail: String::new(),
                    expires_at: *until,
                    confirmation: false,
                    pairing: false,
                    no_trust: false,
                    busy: false,
                    connected: false,
                    transfer: None,
                },
                None,
            ));
        }
        self.account.retain(|(_, card)| card.expires_at > now);
        for (id, card) in &self.account {
            items.push((format!("notice:{id}"), card.clone(), None));
        }
        let valid: HashSet<_> = items.iter().map(|(key, _, _)| key.clone()).collect();
        self.keys.retain(|key, _| valid.contains(key));
        let mut cards = Vec::new();
        let mut targets = HashMap::new();
        for (key, mut card, request) in items {
            let (ticket, at) = self
                .keys
                .entry(key)
                .or_insert_with(|| (uuid::Uuid::new_v4().simple().to_string(), now))
                .clone();
            card.ticket = ticket.clone();
            card.at = at;
            card.busy |= self.claimed.contains(&ticket);
            if card.confirmation && card.busy {
                card.detail = text.submitting_ellipsis.into();
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
                            .with_title((text.notifications_window_title)(crate::APP_NAME))
                            .with_inner_size([theme::NOTIFICATION_WIDTH, 220.])
                            .with_resizable(false)
                            .with_active(false)
                            .with_visible(true),
                        centered: false,
                        notification: true,
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
        crate::platform::windows::notifications::set_activation_sink(None);
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
        let active = snapshot
            .as_ref()
            .map(|status| active_cards(status, self.devices.as_ref(), self.catalog.as_ref()))
            .unwrap_or_default();
        let ended = snapshot
            .as_ref()
            .and_then(|s| s.ended_connection.clone())
            .map(|end| {
                let seconds = end.connection.elapsed_seconds.unwrap_or(0);
                let peer = active_key(
                    &end.connection.client_id,
                    &end.connection.device_id,
                    &end.connection.session_id,
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
                .unwrap_or_else(|| t().app.remote_device.into());
                EndNotice {
                    id: end.id,
                    peer,
                    card: Card {
                        disconnect: None,
                        ticket: String::new(),
                at: 0,
                        title: t().app.remote_connection_ended.into(),
                        body: clean(&source),
                        detail: (t().app.session_length)(&super::controlled_caption::duration(
                            seconds,
                        )),
                        expires_at: end.ended_at.saturating_add(10),
                        confirmation: false,
                        pairing: false,
                        no_trust: false,
                        busy: false,
                        connected: false,
                        transfer: None,
                    },
                }
            });
        self.notifications
            .update(self.login_generation, host, snapshot, active, ended);
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
                Verb::Allow | Verb::Trust | Verb::Reject => {
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
                            self.notifications.error = Some(t().app.submit_failed.into());
                        }
                    }
                }
            }
        }
    }
}
