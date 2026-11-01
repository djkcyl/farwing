//! Realtime state: presence links, controller links, sessions (several
//! participants per host), tickets and resume tokens. All state lives behind
//! one mutex; nothing awaits while it is held. Frames are queued to
//! per-connection unbounded channels.
//!
//! A controller session is signalled either on its own controller link
//! (session ticket) or on the presence link of the controller's device,
//! which then carries it by `sid` (`docs/protocol/v2-design.md` §11).

use crate::crypto::{new_token, new_uuid, token_hash};
use crate::db::{self, now_s};
use crate::ice::IceProvider;
use farwing_protocol::api::{Participant, Purpose, Relation};
use farwing_protocol::ids;
use farwing_protocol::realtime::{
    ControllerInfo, Envelope, Hello, Limits, PeerIdentity, Ping, ResumeRefreshAck, Role,
    RtcCandidate, RtcDescription, SessionEnd, SessionIncoming, SessionPermissions,
    SessionBudget, SessionRelays, SessionStart, SessionStarted, close, reason, relay_denied, types,
};
use farwing_protocol::{ErrorBody, ErrorCode};
use parking_lot::Mutex;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};

/// Frame queued to a connection task.
#[derive(Debug)]
pub enum Outbound {
    Text(String),
    Close(u16, String),
}

/// One physical WebSocket connection.
#[derive(Clone, Debug)]
pub struct ConnHandle {
    pub conn_id: String,
    pub tx: UnboundedSender<Outbound>,
    pub ip: IpAddr,
}

impl ConnHandle {
    fn send(&self, env: &Envelope) {
        let _ = self.tx.send(Outbound::Text(env.encode()));
    }
    fn close(&self, code: u16, why: &str) {
        let _ = self.tx.send(Outbound::Close(code, why.to_owned()));
    }
}

/// Credential a link was admitted with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LinkAuth {
    Account { user_id: String, token_hash: String },
    Guest { token_hash: String },
}

impl LinkAuth {
    pub fn user_id(&self) -> Option<&str> {
        match self {
            LinkAuth::Account { user_id, .. } => Some(user_id),
            LinkAuth::Guest { .. } => None,
        }
    }
    pub fn is_guest(&self) -> bool {
        matches!(self, LinkAuth::Guest { .. })
    }
}

/// What a ticket admits.
#[derive(Clone, Debug)]
pub enum Admission {
    Presence { device_id: String, auth: LinkAuth },
    Controller { session_id: String },
    /// A chat connection (`crate::chat::conn`) of an account's device.
    Chat { device_id: String, auth: LinkAuth },
}

struct Ticket {
    admission: Admission,
    expires: Instant,
}

/// A logical connection that survives physical reconnects via resume tokens.
struct Link {
    id: String,
    role: Role,
    device_id: String,
    auth: LinkAuth,
    session_id: Option<String>,
    epoch: u64,
    resume_hash: String,
    conn: Option<ConnHandle>,
    detached_at: Option<Instant>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SessState {
    Ticketed,
    Started,
    Active,
}

struct Session {
    id: String,
    host_device_id: String,
    host_epoch: u64,
    host_public_key: String,
    controller_device_id: String,
    controller_auth: LinkAuth,
    controller: ControllerInfo,
    purpose: Purpose,
    connect_type: ConnectType,
    relation: Relation,
    /// Capabilities granted to the controller (empty for assistance).
    grants: Vec<String>,
    participant_id: String,
    /// The controller's visit (the session ID when it gave none).
    visit: String,
    created_at_s: i64,
    state: SessState,
    controller_link: Option<String>,
    ticket_deadline: Option<Instant>,
    attempt_id: Option<String>,
    cand_to_host: usize,
    cand_to_controller: usize,
    host_notified: bool,
    /// What it may use, as last told to both sides.
    access: crate::egress::Access,
}

/// Why a session ends; determines who is told what.
#[derive(Clone, Copy, Debug)]
enum End {
    Replaced,
    HostClosed(&'static str),
    HostOffline,
    ControllerLeft,
    Revoked { controller: bool, host: bool },
    /// The controller's grants no longer allow the session's purpose.
    PermissionRevoked,
    Expired,
    Shutdown,
    /// The server requires its relays and the session may no longer use
    /// them (`farwing_protocol::realtime::relay_denied`).
    RelayDenied(&'static str),
}

impl End {
    fn as_str(self) -> &'static str {
        match self {
            End::Replaced => reason::REPLACED,
            End::HostClosed(r) => r,
            End::HostOffline => reason::HOST_OFFLINE,
            End::ControllerLeft => reason::CONTROLLER_LEFT,
            End::Revoked { .. } => reason::REVOKED,
            End::PermissionRevoked => reason::PERMISSION_REVOKED,
            End::Expired => reason::EXPIRED,
            End::Shutdown => reason::SERVER_SHUTDOWN,
            End::RelayDenied(r) => r,
        }
    }
}

/// Notifications for the async side (device.changed pushes).
#[derive(Debug)]
pub enum HubEvent {
    PresenceChanged(String),
}

/// Session/device bookkeeping written to SQLite in order.
#[derive(Debug)]
pub enum PersistJob {
    SessionCreated(db::SessionRecord, Box<crate::records::Opened>),
    SessionState {
        id: String,
        state: &'static str,
        started_at: Option<i64>,
    },
    SessionEnded {
        id: String,
        reason: String,
    },
    LastSeen(String),
}

#[derive(Clone, Debug)]
pub struct HubConfig {
    pub heartbeat_ms: u32,
    pub idle_timeout_ms: u32,
    pub resume_ttl: Duration,
    pub ticket_ttl: Duration,
    pub limits: Limits,
    pub max_candidate_bytes: usize,
    pub force_relay: bool,
}

/// Input of [`Hub::create_session`].
pub struct SessionSpec {
    pub host_device_id: String,
    /// The host's device key, stated to the controller in `session.started`.
    pub host_public_key: String,
    pub controller_device_id: String,
    pub controller_auth: LinkAuth,
    pub controller: ControllerInfo,
    /// The account the host device belongs to (none: a guest host).
    pub host_owner: Option<String>,
    pub purpose: Purpose,
    pub connect_type: ConnectType,
    pub relation: Relation,
    pub grants: Vec<String>,
    /// Concurrent participants the host accepts (`policy.max_participants`).
    pub max_participants: u32,
    /// The controller signals over its presence connection: no ticket.
    pub presence: bool,
    /// The connection record: the visit, both sides, how the controller was
    /// admitted, its address; the hub fills in the session and the time.
    pub record: crate::records::Opened,
}

/// Result of processing one inbound frame.
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Close(u16, &'static str),
}

#[derive(Default)]
struct HubState {
    links: HashMap<String, Link>,
    resume: HashMap<String, String>,
    presence: HashMap<String, String>,
    sessions: HashMap<String, Session>,
    tickets: HashMap<String, Ticket>,
    closing: bool,
}

pub struct Hub {
    cfg: parking_lot::RwLock<HubConfig>,
    ice: parking_lot::RwLock<IceProvider>,
    /// Edge relay nodes: per-side relays and the devices' round trips.
    edges: Arc<crate::edge::EdgeHub>,
    /// Whose egress each session's is.
    egress: Arc<crate::egress::Egress>,
    state: Mutex<HubState>,
    events: UnboundedSender<HubEvent>,
    persist: UnboundedSender<PersistJob>,
    /// Chat connections end with the credentials they were admitted with.
    chat: std::sync::OnceLock<Arc<crate::chat::hub::ChatHub>>,
}

pub struct HubChannels {
    pub events: UnboundedReceiver<HubEvent>,
    pub persist: UnboundedReceiver<PersistJob>,
}

/// Snapshot of a session for HTTP handlers.
/// A session for the web console.
#[derive(Clone, Debug, serde::Serialize)]
pub struct LiveSession {
    pub id: String,
    pub host_device_id: String,
    pub controller_device_id: String,
    pub controller: ControllerInfo,
    pub purpose: Purpose,
    pub connect_type: ConnectType,
    pub relation: Relation,
    pub created_at: i64,
    /// The controller joined and the session runs.
    pub connected: bool,
}

#[derive(Clone, Debug)]
pub struct SessionView {
    pub id: String,
    pub host_device_id: String,
    pub controller_device_id: String,
    pub purpose: Purpose,
    pub connect_type: ConnectType,
    pub relation: Relation,
    pub grants: Vec<String>,
}

impl Session {
    fn view(&self) -> SessionView {
        SessionView {
            id: self.id.clone(),
            host_device_id: self.host_device_id.clone(),
            controller_device_id: self.controller_device_id.clone(),
            purpose: self.purpose,
            connect_type: self.connect_type,
            relation: self.relation,
            grants: self.grants.clone(),
        }
    }
}

/// Keeps only the discovery (STUN) addresses of `servers`.
fn discovery_only(servers: &mut Vec<farwing_protocol::realtime::IceServer>) {
    servers.retain_mut(|server| {
        server
            .urls
            .retain(|url| url.starts_with("stun:") || url.starts_with("stuns:"));
        server.username.clear();
        server.credential.clear();
        !server.urls.is_empty()
    });
}

fn err_env(re: Option<&str>, code: ErrorCode, msg: &str) -> Envelope {
    Envelope::error(re, ErrorBody::new(code, msg))
}

/// `not_found` for a session that is gone.
fn session_ended(msg: &str) -> ErrorBody {
    ErrorBody::new(ErrorCode::NotFound, msg)
        .with_details(json!({ "reason": farwing_protocol::error::reason::SESSION_ENDED }))
}

fn ended_env(sid: &str, r: &str) -> Envelope {
    Envelope::new(
        types::SESSION_ENDED,
        SessionEnd {
            reason: r.to_owned(),
        },
    )
    .with_sid(sid)
}

/// Which endpoint opened a session: an account's (`/sessions`) or an access
/// code's (`/assist`). Recorded for the console; hosts go by the relation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectType {
    Account,
    Assist,
}

fn connect_type_str(t: ConnectType) -> &'static str {
    match t {
        ConnectType::Account => "account",
        ConnectType::Assist => "assist",
    }
}

pub fn relation_str(r: Relation) -> &'static str {
    match r {
        Relation::Own => "own",
        Relation::Friend => "friend",
        Relation::Assist => "assist",
        Relation::Trusted => "trusted",
    }
}

impl Hub {
    pub fn new(
        cfg: HubConfig,
        ice: IceProvider,
        edges: Arc<crate::edge::EdgeHub>,
        egress: Arc<crate::egress::Egress>,
    ) -> (Arc<Hub>, HubChannels) {
        let (events, events_rx) = unbounded_channel();
        let (persist, persist_rx) = unbounded_channel();
        (
            Arc::new(Hub {
                cfg: parking_lot::RwLock::new(cfg),
                ice: parking_lot::RwLock::new(ice),
                edges,
                egress,
                state: Mutex::new(HubState::default()),
                events,
                persist,
                chat: std::sync::OnceLock::new(),
            }),
            HubChannels {
                events: events_rx,
                persist: persist_rx,
            },
        )
    }

    /// The chat connections revoked credentials close.
    pub fn set_chat(&self, chat: Arc<crate::chat::hub::ChatHub>) {
        let _ = self.chat.set(chat);
    }

    /// Limits and timers for new links and sessions.
    pub fn config(&self) -> HubConfig {
        self.cfg.read().clone()
    }

    /// New limits and ICE servers (settings changed).
    pub fn reconfigure(&self, cfg: HubConfig, ice: IceProvider) {
        *self.cfg.write() = cfg;
        *self.ice.write() = ice;
    }


    /// The relays one side of session `sid` gets: this server's, then the
    /// edge nodes nearest to `device_id`; discovery only when the session
    /// may not use relays.
    fn servers_for(
        &self,
        sid: &str,
        device_id: &str,
        access: &crate::egress::Access,
        admitting: bool,
    ) -> Vec<farwing_protocol::realtime::IceServer> {
        let mut servers = self.ice.read().servers(sid);
        // This server's egress is taken: a starting session relays elsewhere.
        if admitting && self.egress.local_full() {
            discovery_only(&mut servers);
        }
        servers.extend(self.edges.servers_for(sid, device_id));
        if !access.relay.allowed {
            discovery_only(&mut servers);
        }
        servers
    }

    /// Session `sid`'s budget while relayed (kbit/s; 0: none) to its host,
    /// and to its controller to show.
    pub fn push_budget(&self, sid: &str, budget_kbps: u32) {
        let st = self.state.lock();
        let Some(s) = st.sessions.get(sid) else { return };
        let envelope = Envelope::new(types::SESSION_BUDGET, SessionBudget { budget_kbps }).with_sid(sid);
        if s.host_notified
            && let Some(h) = Self::host_conn(&st, &s.host_device_id)
        {
            h.send(&envelope);
        }
        if let Some(c) = s
            .controller_link
            .as_deref()
            .and_then(|id| st.links.get(id))
            .and_then(|l| l.conn.as_ref())
        {
            c.send(&envelope);
        }
    }

    /// Tells both sides of session `sid` its relays and what it may use.
    fn send_relays(&self, st: &HubState, sid: &str, s: &Session) {
        let host_current = st
            .presence
            .get(&s.host_device_id)
            .and_then(|id| st.links.get(id))
            .is_some_and(|l| l.epoch == s.host_epoch);
        if s.host_notified
            && host_current
            && let Some(h) = Self::host_conn(st, &s.host_device_id)
        {
            let relays = SessionRelays {
                ice_servers: self.servers_for(sid, &s.host_device_id, &s.access, false),
                relay: s.access.relay.clone(),
                forward: Some(s.access.forward),
            };
            h.send(&Envelope::new(types::SESSION_RELAYS, relays).with_sid(sid));
        }
        if let Some(c) = s
            .controller_link
            .as_deref()
            .and_then(|id| st.links.get(id))
            .and_then(|l| l.conn.as_ref())
        {
            let relays = SessionRelays {
                ice_servers: self.servers_for(sid, &s.controller_device_id, &s.access, false),
                relay: s.access.relay.clone(),
                forward: None,
            };
            c.send(&Envelope::new(types::SESSION_RELAYS, relays).with_sid(sid));
        }
    }

    /// What each started session may use, decided again (policies, monthly
    /// totals); sessions whose access changed are told and their gauges
    /// follow. Called every second.
    pub fn refresh_access(&self) {
        let forced = self.cfg.read().force_relay;
        let mut st = self.state.lock();
        let changed: Vec<String> = st
            .sessions
            .iter_mut()
            .filter(|(_, s)| s.state != SessState::Ticketed)
            .filter_map(|(sid, s)| {
                let access = self.egress.access(sid, false);
                (access != s.access).then(|| {
                    s.access = access;
                    sid.clone()
                })
            })
            .collect();
        for sid in changed {
            let s = &st.sessions[&sid];
            tracing::info!(session_id = %sid, relay = s.access.relay.allowed,
                reason = ?s.access.relay.reason, forward = s.access.forward, "session access changed");
            self.egress.apply(&sid, &s.access);
            // Relays are required and this session may not use them.
            if forced && !s.access.relay.allowed {
                let why = match s.access.relay.reason.as_deref() {
                    Some(relay_denied::QUOTA) => relay_denied::QUOTA,
                    Some(relay_denied::BUSY) => relay_denied::BUSY,
                    _ => relay_denied::NOT_ALLOWED,
                };
                self.end_session_locked(&mut st, &sid, End::RelayDenied(why));
                continue;
            }
            self.send_relays(&st, &sid, s);
        }
    }

    fn persist(&self, job: PersistJob) {
        let _ = self.persist.send(job);
    }

    fn presence_changed(&self, device_id: &str) {
        let _ = self
            .events
            .send(HubEvent::PresenceChanged(device_id.to_owned()));
        self.persist(PersistJob::LastSeen(device_id.to_owned()));
    }

    // ------------------------------------------------------------- queries

    fn host_conn<'a>(st: &'a HubState, device_id: &str) -> Option<&'a ConnHandle> {
        st.presence
            .get(device_id)
            .and_then(|id| st.links.get(id))
            .and_then(|l| l.conn.as_ref())
    }

    /// Send `env` on every attached presence connection.
    pub fn broadcast(&self, env: &Envelope) -> usize {
        let st = self.state.lock();
        let conns: Vec<&ConnHandle> = st
            .presence
            .values()
            .filter_map(|id| st.links.get(id))
            .filter_map(|l| l.conn.as_ref())
            .collect();
        for c in &conns {
            c.send(env);
        }
        conns.len()
    }

    /// The builtin relay restarted: sessions `sids` had allocations on it.
    /// Both sides get the relays anew (`session.relays`); how many
    /// sessions were told.
    pub fn relay_lost(&self, sids: &[String]) -> usize {
        let st = self.state.lock();
        let mut told = 0;
        for sid in sids {
            let Some(s) = st.sessions.get(sid) else { continue };
            self.send_relays(&st, sid, s);
            told += 1;
        }
        told
    }

    /// A presence connection is currently attached.
    pub fn is_online(&self, device_id: &str) -> bool {
        Self::host_conn(&self.state.lock(), device_id).is_some()
    }

    /// Remote IP of the attached presence connection.
    pub fn presence_ip(&self, device_id: &str) -> Option<IpAddr> {
        Self::host_conn(&self.state.lock(), device_id).map(|c| c.ip)
    }

    /// Send a frame to the device's attached presence connection.
    pub fn push(&self, device_id: &str, env: &Envelope) -> bool {
        match Self::host_conn(&self.state.lock(), device_id) {
            Some(c) => {
                c.send(env);
                true
            }
            None => false,
        }
    }

    /// Participants (sessions) of the host `device_id`, oldest first.
    pub fn participants(&self, device_id: &str) -> Vec<Participant> {
        let st = self.state.lock();
        let mut out: Vec<Participant> = st
            .sessions
            .values()
            .filter(|s| s.host_device_id == device_id)
            .map(|s| Participant {
                participant_id: s.participant_id.clone(),
                session_id: s.id.clone(),
                user_id: s.controller.user_id.clone(),
                nickname: s.controller.display_name().to_owned(),
                device_id: s.controller_device_id.clone(),
                device_alias: s.controller.alias.clone(),
                relation: s.relation,
                purpose: s.purpose.as_str().to_owned(),
                started_at: s.created_at_s,
            })
            .collect();
        out.sort_by(|a, b| (a.started_at, &a.session_id).cmp(&(b.started_at, &b.session_id)));
        out
    }

    pub fn session(&self, sid: &str) -> Option<SessionView> {
        self.state.lock().sessions.get(sid).map(Session::view)
    }

    /// `(links, attached connections, sessions)`.
    pub fn counts(&self) -> (usize, usize, usize) {
        let st = self.state.lock();
        let attached = st.links.values().filter(|l| l.conn.is_some()).count();
        (st.links.len(), attached, st.sessions.len())
    }

    // ------------------------------------------------------------- tickets

    fn issue_ticket(&self, st: &mut HubState, admission: Admission) -> String {
        let token = new_token(ids::TICKET_PREFIX);
        st.tickets.insert(
            token_hash(&token),
            Ticket {
                admission,
                expires: Instant::now() + self.cfg.read().ticket_ttl,
            },
        );
        token
    }

    pub fn issue_presence_ticket(&self, device_id: &str, auth: LinkAuth) -> String {
        let mut st = self.state.lock();
        self.issue_ticket(
            &mut st,
            Admission::Presence {
                device_id: device_id.to_owned(),
                auth,
            },
        )
    }

    pub fn issue_chat_ticket(&self, device_id: &str, auth: LinkAuth) -> String {
        let mut st = self.state.lock();
        self.issue_ticket(
            &mut st,
            Admission::Chat {
                device_id: device_id.to_owned(),
                auth,
            },
        )
    }

    /// Consume a single-use ticket.
    pub fn redeem_ticket(&self, ticket: &str) -> Option<Admission> {
        if !ids::is_token(ticket, ids::TICKET_PREFIX) {
            return None;
        }
        let mut st = self.state.lock();
        let t = st.tickets.remove(&token_hash(ticket))?;
        (t.expires > Instant::now()).then_some(t.admission)
    }

    /// Whether `token` currently identifies a resumable link.
    pub fn resume_valid(&self, token: &str) -> bool {
        if !ids::is_token(token, ids::RESUME_TOKEN_PREFIX) {
            return false;
        }
        let st = self.state.lock();
        st.resume
            .get(&token_hash(token))
            .and_then(|id| st.links.get(id))
            .is_some_and(|l| {
                l.detached_at
                    .is_none_or(|t| t.elapsed() < self.cfg.read().resume_ttl)
            })
    }

    fn hello(&self, link: &Link, resume_token: &str, resumed: bool) -> Envelope {
        Envelope::new(
            types::HELLO,
            Hello {
                epoch: link.epoch,
                session_id: link.session_id.clone(),
                resume_token: resume_token.to_owned(),
                heartbeat_ms: self.cfg.read().heartbeat_ms,
                idle_timeout_ms: self.cfg.read().idle_timeout_ms,
                resumed,
                probe_nodes: if link.role == Role::Host {
                    self.edges.probe_list()
                } else {
                    Vec::new()
                },
            },
        )
    }

    fn remove_link(st: &mut HubState, link_id: &str) -> Option<Link> {
        let link = st.links.remove(link_id)?;
        st.resume.remove(&link.resume_hash);
        if link.role == Role::Host
            && st.presence.get(&link.device_id).map(String::as_str) == Some(link_id)
        {
            st.presence.remove(&link.device_id);
        }
        Some(link)
    }

    // -------------------------------------------------------------- attach

    /// A fresh (non-resumed) presence connection. Supersedes any previous
    /// presence link of the device (4001) and ends its sessions
    /// (`host_offline`). Returns the link ID.
    pub fn attach_presence(
        &self,
        device_id: &str,
        auth: LinkAuth,
        epoch: u64,
        conn: ConnHandle,
    ) -> Option<String> {
        let mut st = self.state.lock();
        if st.closing {
            conn.close(close::NORMAL, "server shutting down");
            return None;
        }
        let was_online = Self::host_conn(&st, device_id).is_some();
        if let Some(old_id) = st.presence.get(device_id).cloned()
            && let Some(old) = Self::remove_link(&mut st, &old_id)
        {
            self.end_carried(&mut st, &old_id);
            if let Some(c) = old.conn {
                c.close(close::SUPERSEDED, "superseded by a newer connection");
            }
        }
        let hosted: Vec<String> = st
            .sessions
            .values()
            .filter(|s| s.host_device_id == device_id)
            .map(|s| s.id.clone())
            .collect();
        for sid in hosted {
            self.end_session_locked(&mut st, &sid, End::HostOffline);
        }
        let token = new_token(ids::RESUME_TOKEN_PREFIX);
        let link = Link {
            id: new_uuid(),
            role: Role::Host,
            device_id: device_id.to_owned(),
            auth,
            session_id: None,
            epoch,
            resume_hash: token_hash(&token),
            conn: Some(conn.clone()),
            detached_at: None,
        };
        conn.send(&self.hello(&link, &token, false));
        let id = link.id.clone();
        st.resume.insert(link.resume_hash.clone(), id.clone());
        st.presence.insert(device_id.to_owned(), id.clone());
        st.links.insert(id.clone(), link);
        drop(st);
        tracing::info!(device_id, epoch, "presence connected");
        if !was_online {
            self.presence_changed(device_id);
        }
        Some(id)
    }

    /// Controller connection created from a session ticket.
    pub fn attach_controller(&self, sid: &str, conn: ConnHandle) -> Option<String> {
        let mut st = self.state.lock();
        if st.closing {
            conn.close(close::NORMAL, "server shutting down");
            return None;
        }
        let Some(old) = st.sessions.get(sid).map(|s| s.controller_link.clone()) else {
            conn.close(close::SESSION_ENDED, "session ended");
            return None;
        };
        if let Some(old_id) = old
            && let Some(old) = Self::remove_link(&mut st, &old_id)
            && let Some(c) = old.conn
        {
            c.close(close::SUPERSEDED, "superseded by a newer connection");
        }
        let token = new_token(ids::RESUME_TOKEN_PREFIX);
        let s = st.sessions.get_mut(sid).expect("checked");
        let link = Link {
            id: new_uuid(),
            role: Role::Controller,
            device_id: s.controller_device_id.clone(),
            auth: s.controller_auth.clone(),
            session_id: Some(sid.to_owned()),
            epoch: s.host_epoch,
            resume_hash: token_hash(&token),
            conn: Some(conn.clone()),
            detached_at: None,
        };
        s.controller_link = Some(link.id.clone());
        s.ticket_deadline = None;
        conn.send(&self.hello(&link, &token, false));
        let id = link.id.clone();
        st.resume.insert(link.resume_hash.clone(), id.clone());
        st.links.insert(id.clone(), link);
        tracing::info!(session_id = sid, "controller connected");
        Some(id)
    }

    /// Re-attach a link with its resume token.
    pub fn attach_resume(&self, token: &str, conn: ConnHandle) -> Option<String> {
        let mut st = self.state.lock();
        if st.closing {
            conn.close(close::NORMAL, "server shutting down");
            return None;
        }
        let hash = token_hash(token);
        let Some(link_id) = st.resume.get(&hash).cloned() else {
            conn.close(close::INVALID_TICKET, "invalid resume token");
            return None;
        };
        let ttl = self.cfg.read().resume_ttl;
        let link = st.links.get_mut(&link_id).expect("resume index consistent");
        if link.detached_at.is_some_and(|t| t.elapsed() >= ttl) {
            conn.close(close::INVALID_TICKET, "resume token expired");
            return None;
        }
        if let Some(old) = link.conn.take() {
            old.close(close::SUPERSEDED, "resumed on a newer connection");
        }
        let was_detached = link.detached_at.take().is_some();
        link.conn = Some(conn.clone());
        let hello = self.hello(link, token, true);
        conn.send(&hello);
        let (role, device, epoch) = (link.role, link.device_id.clone(), link.epoch);
        if role == Role::Host && was_detached {
            // Permission changes while detached were not delivered: the
            // host's sessions get their current grants again.
            for s in st.sessions.values().filter(|s| {
                s.host_device_id == device && s.host_notified && s.host_epoch == epoch
            }) {
                let grants = s.grants.clone();
                conn.send(
                    &Envelope::new(types::SESSION_PERMISSIONS, SessionPermissions { grants })
                        .with_sid(&s.id),
                );
            }
        }
        drop(st);
        tracing::info!(device_id = %device, ?role, "connection resumed");
        if role == Role::Host && was_detached {
            self.presence_changed(&device);
        }
        Some(link_id)
    }

    /// The physical connection ended without `bye`: keep the link for the
    /// resume window.
    pub fn connection_closed(&self, link_id: &str, conn_id: &str) {
        let mut st = self.state.lock();
        let Some(link) = st.links.get_mut(link_id) else {
            return;
        };
        if link.conn.as_ref().is_none_or(|c| c.conn_id != conn_id) {
            return;
        }
        link.conn = None;
        link.detached_at = Some(Instant::now());
        let (role, device) = (link.role, link.device_id.clone());
        drop(st);
        tracing::info!(device_id = %device, ?role, "connection lost; resumable");
        if role == Role::Host {
            self.presence_changed(&device);
        }
    }

    // ------------------------------------------------------------ sessions

    /// Create a session (state `ticketed`) and its first ticket (empty when
    /// the controller signals over its presence connection).
    ///
    /// A host accepts up to `max_participants` concurrent sessions. A
    /// controller device has at most one session per host: a new one replaces
    /// its previous session (`replaced`); sessions of other controllers are
    /// never displaced (`busy` when the host is full).
    pub fn create_session(&self, spec: SessionSpec) -> Result<(String, String), ErrorCode> {
        let mut st = self.state.lock();
        if st.closing {
            return Err(ErrorCode::Unavailable);
        }
        let host = st
            .presence
            .get(&spec.host_device_id)
            .and_then(|id| st.links.get(id))
            .filter(|l| l.conn.is_some())
            .ok_or(ErrorCode::DeviceOffline)?;
        if spec.connect_type == ConnectType::Account && host.auth.is_guest() {
            return Err(ErrorCode::Forbidden);
        }
        let epoch = host.epoch;
        let (own, others): (Vec<&Session>, Vec<&Session>) = st
            .sessions
            .values()
            .filter(|s| s.host_device_id == spec.host_device_id)
            .partition(|s| s.controller_device_id == spec.controller_device_id);
        let limit = spec
            .max_participants
            .clamp(1, farwing_protocol::limits::MAX_PARTICIPANTS) as usize;
        if others.len() >= limit {
            return Err(ErrorCode::Busy);
        }
        let replaced: Vec<String> = own.iter().map(|s| s.id.clone()).collect();
        for sid in replaced {
            self.end_session_locked(&mut st, &sid, End::Replaced);
        }
        let sid = new_uuid();
        let session = Session {
            id: sid.clone(),
            host_device_id: spec.host_device_id.clone(),
            host_epoch: epoch,
            host_public_key: spec.host_public_key.clone(),
            controller_device_id: spec.controller_device_id.clone(),
            controller_auth: spec.controller_auth,
            controller: spec.controller.clone(),
            purpose: spec.purpose,
            connect_type: spec.connect_type,
            relation: spec.relation,
            grants: spec.grants.clone(),
            participant_id: new_uuid(),
            visit: if spec.record.visit.is_empty() {
                sid.clone()
            } else {
                spec.record.visit.clone()
            },
            created_at_s: now_s(),
            state: SessState::Ticketed,
            controller_link: None,
            ticket_deadline: Some(Instant::now() + self.cfg.read().ticket_ttl),
            attempt_id: None,
            cand_to_host: 0,
            cand_to_controller: 0,
            host_notified: false,
            access: crate::egress::Access::default(),
        };
        self.persist(PersistJob::SessionCreated(db::SessionRecord {
            id: sid.clone(),
            host_device_id: spec.host_device_id.clone(),
            controller_device_id: spec.controller_device_id.clone(),
            controller_user_id: spec.controller.user_id.clone(),
            purpose: spec.purpose.as_str().to_owned(),
            connect_type: connect_type_str(spec.connect_type).to_owned(),
            relation: relation_str(spec.relation).to_owned(),
            state: "ticketed".to_owned(),
            created_at: session.created_at_s,
            started_at: None,
        }, Box::new(crate::records::Opened {
            session_id: sid.clone(),
            visit: session.visit.clone(),
            at: session.created_at_s,
            ..spec.record
        })));
        self.egress.ledger.open(
            &sid,
            crate::egress::Payer::of(&spec.controller.user_id, spec.host_owner.as_deref()),
        );
        st.sessions.insert(sid.clone(), session);
        let ticket = if spec.presence {
            String::new()
        } else {
            self.issue_ticket(
                &mut st,
                Admission::Controller {
                    session_id: sid.clone(),
                },
            )
        };
        tracing::info!(
            session_id = %sid,
            host = %spec.host_device_id,
            controller = %spec.controller_device_id,
            relation = relation_str(spec.relation),
            purpose = spec.purpose.as_str(),
            "session created"
        );
        Ok((sid, ticket))
    }

    /// New ticket for an existing session of `controller_device_id`.
    pub fn refresh_session_ticket(
        &self,
        sid: &str,
        controller_device_id: &str,
        auth: LinkAuth,
    ) -> Result<(SessionView, String), ErrorCode> {
        let mut st = self.state.lock();
        let ttl = self.cfg.read().ticket_ttl;
        let s = st
            .sessions
            .get_mut(sid)
            .filter(|s| s.controller_device_id == controller_device_id)
            .ok_or(ErrorCode::NotFound)?;
        // The same account (or a guest staying a guest): a session cannot be
        // moved to a credential that revoking the account would not reach.
        if s.controller_auth.user_id() != auth.user_id() {
            return Err(ErrorCode::Forbidden);
        }
        s.controller_auth = auth;
        if s.controller_link.is_none() {
            s.ticket_deadline = Some(Instant::now() + ttl);
        }
        let view = s.view();
        let ticket = self.issue_ticket(
            &mut st,
            Admission::Controller {
                session_id: sid.to_owned(),
            },
        );
        Ok((view, ticket))
    }

    fn end_session_locked(&self, st: &mut HubState, sid: &str, why: End) {
        let Some(s) = st.sessions.remove(sid) else {
            return;
        };
        self.egress.ledger.close(sid);
        self.egress.ended(sid);
        // A controller link exists for this session only; a presence link
        // that carries it stays (and is not closed).
        let (ctrl_conn, carried) = match s
            .controller_link
            .as_deref()
            .and_then(|id| st.links.get(id).map(|l| (id.to_owned(), l.role)))
        {
            Some((id, Role::Controller)) => {
                (Self::remove_link(st, &id).and_then(|l| l.conn), false)
            }
            Some((id, Role::Host)) => (st.links.get(&id).and_then(|l| l.conn.clone()), true),
            None => (None, false),
        };
        let host_conn = Self::host_conn(st, &s.host_device_id).cloned();
        let (ctrl_reason, ctrl_close, host_reason): (Option<&str>, (u16, &str), Option<&str>) =
            match why {
                End::Replaced => (
                    Some(reason::REPLACED),
                    (close::SESSION_ENDED, "session replaced"),
                    Some(reason::REPLACED),
                ),
                End::HostClosed(r) => (Some(r), (close::SESSION_ENDED, "session ended"), None),
                End::HostOffline => (
                    Some(reason::HOST_OFFLINE),
                    (close::SESSION_ENDED, "host offline"),
                    None,
                ),
                End::ControllerLeft => {
                    (None, (close::NORMAL, "bye"), Some(reason::CONTROLLER_LEFT))
                }
                End::Revoked { controller, host } => (
                    Some(reason::REVOKED),
                    if controller {
                        (close::REVOKED, "credentials revoked")
                    } else {
                        (close::SESSION_ENDED, "session ended")
                    },
                    (!host).then_some(reason::REVOKED),
                ),
                End::PermissionRevoked => (
                    Some(reason::PERMISSION_REVOKED),
                    (close::SESSION_ENDED, "permission revoked"),
                    Some(reason::PERMISSION_REVOKED),
                ),
                End::Expired => (
                    Some(reason::EXPIRED),
                    (close::SESSION_ENDED, "session expired"),
                    Some(reason::EXPIRED),
                ),
                End::Shutdown => (
                    Some(reason::SERVER_SHUTDOWN),
                    (1001, "server shutting down"),
                    None,
                ),
                End::RelayDenied(r) => (Some(r), (close::SESSION_ENDED, "relay unavailable"), Some(r)),
            };
        if let Some(c) = ctrl_conn {
            if let Some(r) = ctrl_reason {
                c.send(&ended_env(sid, r));
            }
            if !carried {
                c.close(ctrl_close.0, ctrl_close.1);
            }
        }
        if s.host_notified
            && let (Some(h), Some(r)) = (host_conn, host_reason)
        {
            h.send(&ended_env(sid, r));
        }
        self.persist(PersistJob::SessionEnded {
            id: sid.to_owned(),
            reason: why.as_str().to_owned(),
        });
        tracing::info!(session_id = sid, reason = why.as_str(), "session ended");
    }

    /// A presence link went away: end the controller sessions it carried.
    fn end_carried(&self, st: &mut HubState, link_id: &str) {
        let carried: Vec<String> = st
            .sessions
            .values()
            .filter(|s| s.controller_link.as_deref() == Some(link_id))
            .map(|s| s.id.clone())
            .collect();
        for sid in carried {
            if let Some(s) = st.sessions.get_mut(&sid) {
                // The link is gone: nobody on the controller side to tell.
                s.controller_link = None;
            }
            self.end_session_locked(st, &sid, End::ControllerLeft);
        }
    }

    // --------------------------------------------------------------- grants

    /// Apply changed grants to the live friend sessions of `controller_user`:
    /// `caps` maps host device IDs to the capabilities the user now holds
    /// there (hosts not in the map are unaffected). Sessions whose purpose is
    /// no longer allowed end with `permission_revoked`; the others get
    /// `session.permissions` on both sides when their grants changed.
    pub fn regrant(&self, controller_user: &str, caps: &HashMap<String, Vec<String>>) {
        let mut st = self.state.lock();
        let affected: Vec<(String, Vec<String>)> = st
            .sessions
            .values()
            .filter(|s| {
                s.relation == Relation::Friend && s.controller.user_id == controller_user
            })
            .filter_map(|s| {
                caps.get(&s.host_device_id)
                    .map(|c| (s.id.clone(), s.controller.limit_grants(c.clone())))
            })
            .collect();
        for (sid, grants) in affected {
            let s = st.sessions.get_mut(&sid).expect("listed");
            if !grants.iter().any(|c| c == s.purpose.required_cap()) {
                self.end_session_locked(&mut st, &sid, End::PermissionRevoked);
                continue;
            }
            if s.grants == grants {
                continue;
            }
            s.grants = grants.clone();
            let env = Envelope::new(types::SESSION_PERMISSIONS, SessionPermissions { grants })
                .with_sid(&sid);
            let s = &st.sessions[&sid];
            let host_current = st
                .presence
                .get(&s.host_device_id)
                .and_then(|id| st.links.get(id))
                .is_some_and(|l| l.epoch == s.host_epoch);
            if s.host_notified
                && host_current
                && let Some(h) = Self::host_conn(&st, &s.host_device_id)
            {
                h.send(&env);
            }
            if let Some(c) = s
                .controller_link
                .as_deref()
                .and_then(|id| st.links.get(id))
                .and_then(|l| l.conn.as_ref())
            {
                c.send(&env);
            }
            tracing::info!(session_id = %sid, "session permissions changed");
        }
    }

    // ----------------------------------------------------------- revocation

    fn revoke_where(&self, matches: impl Fn(&str, &LinkAuth) -> bool) {
        let mut st = self.state.lock();
        st.tickets.retain(|_, t| match &t.admission {
            Admission::Presence { device_id, auth } | Admission::Chat { device_id, auth } => {
                !matches(device_id, auth)
            }
            Admission::Controller { .. } => true,
        });
        if let Some(chat) = self.chat.get() {
            chat.revoke_where(&matches);
        }
        // Sessions whose controller credential is revoked.
        let ctrl_sids: Vec<String> = st
            .sessions
            .values()
            .filter(|s| matches(&s.controller_device_id, &s.controller_auth))
            .map(|s| s.id.clone())
            .collect();
        for sid in ctrl_sids {
            self.end_session_locked(
                &mut st,
                &sid,
                End::Revoked {
                    controller: true,
                    host: false,
                },
            );
        }
        // Presence links admitted with a revoked credential.
        let hosts: Vec<String> = st
            .links
            .values()
            .filter(|l| l.role == Role::Host && matches(&l.device_id, &l.auth))
            .map(|l| l.id.clone())
            .collect();
        let mut offline = Vec::new();
        for id in hosts {
            let Some(link) = Self::remove_link(&mut st, &id) else {
                continue;
            };
            self.end_carried(&mut st, &id);
            let hosted: Vec<String> = st
                .sessions
                .values()
                .filter(|s| s.host_device_id == link.device_id)
                .map(|s| s.id.clone())
                .collect();
            for sid in hosted {
                self.end_session_locked(
                    &mut st,
                    &sid,
                    End::Revoked {
                        controller: false,
                        host: true,
                    },
                );
            }
            if let Some(c) = link.conn {
                c.close(close::REVOKED, "credentials revoked");
                offline.push(link.device_id.clone());
            }
            tracing::info!(device_id = %link.device_id, "presence revoked");
        }
        drop(st);
        for d in offline {
            self.presence_changed(&d);
        }
    }

    /// Close every connection of `device_id` admitted with an account token.
    pub fn revoke_device(&self, device_id: &str) {
        self.revoke_where(|d, auth| d == device_id && !auth.is_guest());
    }

    /// Close connections of `device_id` admitted with another account's token.
    pub fn revoke_device_other_users(&self, device_id: &str, keep_user: &str) {
        self.revoke_where(|d, auth| {
            d == device_id && auth.user_id().is_some_and(|u| u != keep_user)
        });
    }

    /// Close every connection admitted with a token of `user_id`.
    pub fn revoke_user(&self, user_id: &str) {
        self.revoke_where(|_, auth| auth.user_id() == Some(user_id));
    }

    /// Close every connection of `device_id`, whatever admitted it (an
    /// administrator forcing it offline).
    pub fn disconnect_device(&self, device_id: &str) {
        self.revoke_where(|d, _| d == device_id);
    }

    /// End a session (an administrator); both sides are told.
    pub fn end_session(&self, sid: &str) -> bool {
        let mut st = self.state.lock();
        if !st.sessions.contains_key(sid) {
            return false;
        }
        self.end_session_locked(
            &mut st,
            sid,
            End::Revoked {
                controller: true,
                host: true,
            },
        );
        true
    }

    /// Every session the hub knows (the web console's session list).
    pub fn live_sessions(&self) -> Vec<LiveSession> {
        let st = self.state.lock();
        let mut out: Vec<LiveSession> = st
            .sessions
            .values()
            .map(|s| LiveSession {
                id: s.id.clone(),
                host_device_id: s.host_device_id.clone(),
                controller_device_id: s.controller_device_id.clone(),
                controller: s.controller.clone(),
                purpose: s.purpose,
                connect_type: s.connect_type,
                relation: s.relation,
                created_at: s.created_at_s,
                connected: matches!(s.state, SessState::Active),
            })
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    /// Devices with a live presence connection.
    pub fn online_devices(&self) -> Vec<String> {
        let st = self.state.lock();
        st.presence
            .iter()
            .filter(|(_, link)| st.links.get(*link).is_some_and(|l| l.conn.is_some()))
            .map(|(device, _)| device.clone())
            .collect()
    }

    // --------------------------------------------------------- maintenance

    /// Expire tickets, resume windows and unused sessions.
    pub fn sweep(&self) {
        let now = Instant::now();
        let mut st = self.state.lock();
        st.tickets.retain(|_, t| t.expires > now);
        let ttl = self.cfg.read().resume_ttl;
        let expired: Vec<String> = st
            .links
            .values()
            .filter(|l| l.detached_at.is_some_and(|t| now.duration_since(t) >= ttl))
            .map(|l| l.id.clone())
            .collect();
        for id in expired {
            let Some(link) = Self::remove_link(&mut st, &id) else {
                continue;
            };
            match link.role {
                Role::Host => {
                    self.end_carried(&mut st, &id);
                    let hosted: Vec<String> = st
                        .sessions
                        .values()
                        .filter(|s| s.host_device_id == link.device_id)
                        .map(|s| s.id.clone())
                        .collect();
                    for sid in hosted {
                        self.end_session_locked(&mut st, &sid, End::HostOffline);
                    }
                    tracing::info!(device_id = %link.device_id, "presence resume window expired");
                }
                Role::Controller => {
                    if let Some(sid) = link.session_id.as_deref() {
                        if let Some(s) = st.sessions.get_mut(sid) {
                            s.controller_link = None;
                        }
                        self.end_session_locked(&mut st, sid, End::ControllerLeft);
                    }
                }
            }
        }
        let stale: Vec<String> = st
            .sessions
            .values()
            .filter(|s| s.controller_link.is_none() && s.ticket_deadline.is_some_and(|d| d <= now))
            .map(|s| s.id.clone())
            .collect();
        for sid in stale {
            self.end_session_locked(&mut st, &sid, End::Expired);
        }
    }

    /// End all sessions and close every connection (graceful shutdown).
    pub fn shutdown(&self) {
        let mut st = self.state.lock();
        st.closing = true;
        let sids: Vec<String> = st.sessions.keys().cloned().collect();
        for sid in sids {
            self.end_session_locked(&mut st, &sid, End::Shutdown);
        }
        for link in st.links.values() {
            if let Some(c) = &link.conn {
                c.close(1001, "server shutting down");
            }
        }
        st.links.clear();
        st.resume.clear();
        st.presence.clear();
        st.tickets.clear();
    }

    // ------------------------------------------------------- inbound frames

    /// Process one text frame from `conn_id` on `link_id`.
    pub fn handle_text(&self, link_id: &str, conn_id: &str, text: &str) -> Flow {
        let value: Value = match serde_json::from_str(text) {
            Ok(v @ Value::Object(_)) => v,
            _ => return Flow::Close(close::PROTOCOL_ERROR, "invalid frame"),
        };
        match value.get("v").and_then(Value::as_u64) {
            Some(v) if v == farwing_protocol::PROTOCOL_VERSION as u64 => {}
            Some(_) => return Flow::Close(close::VERSION_UNSUPPORTED, "unsupported version"),
            None => return Flow::Close(close::PROTOCOL_ERROR, "missing version"),
        }
        let env: Envelope = match serde_json::from_value(value) {
            Ok(e) => e,
            Err(_) => return Flow::Close(close::PROTOCOL_ERROR, "invalid envelope"),
        };
        let mut st = self.state.lock();
        let Some(link) = st.links.get(link_id) else {
            // The link was removed; a close frame is already queued.
            return Flow::Continue;
        };
        let Some(conn) = link.conn.clone().filter(|c| c.conn_id == conn_id) else {
            return Flow::Continue;
        };
        let role = link.role;
        let re = env.id.as_deref();
        match env.kind.as_str() {
            types::PING => {
                let t = env.data.get("t").and_then(Value::as_i64).unwrap_or(0);
                conn.send(&Envelope::new(types::PONG, Ping { t }));
            }
            types::PONG | types::ACK | types::ERROR => {}
            types::RESUME_REFRESH => {
                let Some(id) = re else {
                    conn.send(&err_env(
                        None,
                        ErrorCode::BadRequest,
                        "resume.refresh requires id",
                    ));
                    return Flow::Continue;
                };
                let token = new_token(ids::RESUME_TOKEN_PREFIX);
                let hash = token_hash(&token);
                let link = st.links.get_mut(link_id).expect("present");
                let old = std::mem::replace(&mut link.resume_hash, hash.clone());
                st.resume.remove(&old);
                st.resume.insert(hash, link_id.to_owned());
                conn.send(&Envelope::ack(
                    id,
                    ResumeRefreshAck {
                        resume_token: token,
                    },
                ));
            }
            types::BYE => match role {
                Role::Controller => {
                    let sid = link.session_id.clone().unwrap_or_default();
                    if st.sessions.contains_key(&sid) {
                        self.end_session_locked(&mut st, &sid, End::ControllerLeft);
                    } else {
                        Self::remove_link(&mut st, link_id);
                        conn.close(close::NORMAL, "bye");
                    }
                }
                Role::Host => {
                    let device = link.device_id.clone();
                    Self::remove_link(&mut st, link_id);
                    self.end_carried(&mut st, link_id);
                    let hosted: Vec<String> = st
                        .sessions
                        .values()
                        .filter(|s| s.host_device_id == device)
                        .map(|s| s.id.clone())
                        .collect();
                    for sid in hosted {
                        self.end_session_locked(&mut st, &sid, End::HostOffline);
                    }
                    conn.close(close::NORMAL, "bye");
                    drop(st);
                    tracing::info!(device_id = %device, "presence closed (bye)");
                    self.presence_changed(&device);
                }
            },
            types::SESSION_START => self.on_session_start(&mut st, link_id, &conn, env),
            types::RTC_OFFER | types::RTC_ANSWER | types::RTC_CANDIDATE => {
                self.on_relay(&mut st, link_id, &conn, env)
            }
            types::SESSION_END => self.on_session_end(&mut st, link_id, &conn, env),
            types::SESSION_LEAVE => self.on_session_leave(&mut st, link_id, &conn, env),
            types::NODES_RTT if role == Role::Host => {
                if let Ok(report) = env.body::<farwing_protocol::realtime::NodesRtt>() {
                    self.edges.record_rtt(&link.device_id, report.rtt);
                }
            }
            other => {
                tracing::debug!(kind = other, "unknown realtime message type");
                conn.send(&err_env(re, ErrorCode::BadRequest, "unknown message type"));
            }
        }
        Flow::Continue
    }

    fn on_session_start(&self, st: &mut HubState, link_id: &str, conn: &ConnHandle, env: Envelope) {
        let re = env.id.as_deref();
        let link = &st.links[link_id];
        let sid = match link.role {
            Role::Controller => {
                let sid = link.session_id.clone().unwrap_or_default();
                if env.sid.as_deref().is_some_and(|s| s != sid) {
                    conn.send(&err_env(
                        re,
                        ErrorCode::BadRequest,
                        "sid does not match this connection",
                    ));
                    return;
                }
                sid
            }
            // A presence connection carries the controller sessions of its
            // own device, created by the same account.
            Role::Host => {
                let user = link.auth.user_id();
                let owned = env.sid.as_deref().and_then(|sid| st.sessions.get(sid)).filter(|s| {
                    s.controller_device_id == link.device_id
                        && user.is_some()
                        && s.controller_auth.user_id() == user
                });
                let Some(s) = owned else {
                    conn.send(&err_env(
                        re,
                        ErrorCode::Forbidden,
                        "session.start is only valid on a controller connection or for this device's own session",
                    ));
                    return;
                };
                s.id.clone()
            }
        };
        let Some(id) = re else {
            conn.send(&err_env(
                None,
                ErrorCode::BadRequest,
                "session.start requires id",
            ));
            return;
        };
        let start: SessionStart = match env.body() {
            Ok(s) => s,
            Err(_) => {
                conn.send(&err_env(re, ErrorCode::BadRequest, "invalid session.start"));
                return;
            }
        };
        if start.attempt_id.is_empty() || start.attempt_id.len() > 64 {
            conn.send(&err_env(re, ErrorCode::BadRequest, "invalid attempt_id"));
            return;
        }
        let Some(s) = st.sessions.get(&sid) else {
            conn.send(&Envelope::error(re, session_ended("session ended")));
            return;
        };
        let host = st
            .presence
            .get(&s.host_device_id)
            .and_then(|id| st.links.get(id))
            .filter(|l| l.epoch == s.host_epoch);
        let Some(host_conn) = host.and_then(|l| l.conn.clone()) else {
            conn.send(&err_env(re, ErrorCode::DeviceOffline, "target device is offline"));
            return;
        };
        if s.connect_type == ConnectType::Account && host.is_some_and(|l| l.auth.is_guest()) {
            conn.send(&err_env(
                re,
                ErrorCode::Forbidden,
                "guest hosts only accept assistance sessions",
            ));
            return;
        }
        let access = self.egress.access(&sid, true);
        if self.cfg.read().force_relay && !access.relay.allowed {
            conn.send(&Envelope::error(
                re,
                ErrorBody::new(
                    ErrorCode::RelayUnavailable,
                    "this server requires its relays and this session may not use them",
                )
                .with_details(json!({ "reason": access.relay.reason })),
            ));
            return;
        }
        // Signalled on this presence connection from now on; a per-session
        // controller link it had before is superseded.
        let carrier = (st.links[link_id].role == Role::Host).then(|| link_id.to_owned());
        if let Some(carrier) = &carrier {
            let previous = st
                .sessions
                .get_mut(&sid)
                .expect("present")
                .controller_link
                .replace(carrier.clone());
            if let Some(previous) = previous.filter(|p| p != carrier)
                && st.links.get(&previous).is_some_and(|l| l.role == Role::Controller)
                && let Some(old) = Self::remove_link(st, &previous)
                && let Some(c) = old.conn
            {
                c.close(close::SUPERSEDED, "session moved to the presence connection");
            }
        }
        let s = st.sessions.get_mut(&sid).expect("present");
        if carrier.is_some() {
            s.ticket_deadline = None;
        }
        s.attempt_id = Some(start.attempt_id.clone());
        s.cand_to_host = 0;
        s.cand_to_controller = 0;
        s.host_notified = true;
        s.access = access.clone();
        self.egress.apply(&sid, &access);
        let first_start = s.state == SessState::Ticketed;
        if first_start {
            s.state = SessState::Started;
        }
        // Purpose, relation, grants and the controller identity come from
        // the session (server state), never from the controller's frame.
        let incoming = SessionIncoming {
            participant_id: s.participant_id.clone(),
            attempt_id: start.attempt_id,
            visit: s.visit.clone(),
            purpose: s.purpose,
            relation: s.relation,
            grants: s.grants.clone(),
            controller: s.controller.clone(),
            options: start.options,
            ice_servers: self.servers_for(&sid, &s.host_device_id, &access, true),
            force_relay: self.cfg.read().force_relay,
            relay: access.relay.clone(),
            forward: access.forward,
            budget_kbps: 0,
        };
        host_conn.send(&Envelope::new(types::SESSION_INCOMING, incoming).with_sid(&sid));
        conn.send(&Envelope::ack(
            id,
            SessionStarted {
                participant_id: s.participant_id.clone(),
                session_id: sid.clone(),
                ice_servers: self.servers_for(&sid, &s.controller_device_id, &access, true),
                force_relay: self.cfg.read().force_relay,
                relay: access.relay.clone(),
                host: PeerIdentity {
                    device_id: s.host_device_id.clone(),
                    public_key: s.host_public_key.clone(),
                },
                relation: s.relation,
                grants: s.grants.clone(),
            },
        ));
        if first_start {
            self.persist(PersistJob::SessionState {
                id: sid.clone(),
                state: "started",
                started_at: Some(now_s()),
            });
        }
        tracing::info!(session_id = %sid, presence = carrier.is_some(), "session started");
    }

    fn on_relay(&self, st: &mut HubState, link_id: &str, conn: &ConnHandle, env: Envelope) {
        let re = env.id.as_deref();
        let Some(sid) = env.sid.clone() else {
            conn.send(&err_env(re, ErrorCode::BadRequest, "sid required"));
            return;
        };
        let link = &st.links[link_id];
        let (role, link_device) = (link.role, link.device_id.clone());
        let link_session = link.session_id.clone();
        let is_presence = st.presence.get(&link_device).map(String::as_str) == Some(link_id);
        let Some(s) = st.sessions.get(&sid) else {
            conn.send(&Envelope::error(re, session_ended("unknown session")));
            return;
        };
        let from_controller = match role {
            Role::Controller => link_session.as_deref() == Some(sid.as_str()),
            // The presence link carrying this controller session.
            Role::Host => s.controller_link.as_deref() == Some(link_id),
        };
        let from_host = role == Role::Host && is_presence && link_device == s.host_device_id;
        if !from_controller && !from_host {
            conn.send(&err_env(
                re,
                ErrorCode::Forbidden,
                "not a party of this session",
            ));
            return;
        }
        let kind = env.kind.as_str();
        let allowed = if from_controller {
            matches!(kind, types::RTC_OFFER | types::RTC_CANDIDATE)
        } else {
            matches!(kind, types::RTC_ANSWER | types::RTC_CANDIDATE)
        };
        if !allowed {
            conn.send(&err_env(
                re,
                ErrorCode::Forbidden,
                "message not allowed in this direction",
            ));
            return;
        }
        let Some(attempt) = s.attempt_id.clone() else {
            conn.send(&err_env(re, ErrorCode::Conflict, "session not started"));
            return;
        };
        let limits = self.cfg.read().limits.clone();
        let limits = &limits;
        match kind {
            types::RTC_OFFER | types::RTC_ANSWER => {
                let Ok(d) = env.body::<RtcDescription>() else {
                    conn.send(&err_env(re, ErrorCode::BadRequest, "invalid description"));
                    return;
                };
                if d.attempt_id != attempt {
                    conn.send(&err_env(re, ErrorCode::Conflict, "stale attempt_id"));
                    return;
                }
                if d.sdp.len() > limits.max_sdp_bytes {
                    conn.send(&err_env(re, ErrorCode::PayloadTooLarge, "SDP too large"));
                    return;
                }
            }
            types::RTC_CANDIDATE => {
                let Ok(c) = env.body::<RtcCandidate>() else {
                    conn.send(&err_env(re, ErrorCode::BadRequest, "invalid candidate"));
                    return;
                };
                if c.attempt_id != attempt {
                    conn.send(&err_env(re, ErrorCode::Conflict, "stale attempt_id"));
                    return;
                }
                let size = serde_json::to_string(&c.candidate).map_or(usize::MAX, |s| s.len());
                if size > self.cfg.read().max_candidate_bytes {
                    conn.send(&err_env(
                        re,
                        ErrorCode::PayloadTooLarge,
                        "candidate too large",
                    ));
                    return;
                }
                let count = if from_controller {
                    s.cand_to_host
                } else {
                    s.cand_to_controller
                };
                if count >= limits.max_candidates {
                    conn.send(&err_env(
                        re,
                        ErrorCode::PayloadTooLarge,
                        "too many candidates",
                    ));
                    return;
                }
            }
            _ => {}
        }
        let dest = if from_controller {
            Self::host_conn(st, &s.host_device_id)
                .filter(|_| {
                    st.presence
                        .get(&s.host_device_id)
                        .and_then(|id| st.links.get(id))
                        .is_some_and(|l| l.epoch == s.host_epoch)
                })
                .cloned()
        } else {
            s.controller_link
                .as_deref()
                .and_then(|id| st.links.get(id))
                .and_then(|l| l.conn.clone())
        };
        let Some(dest) = dest else {
            conn.send(&err_env(re, ErrorCode::DeviceOffline, "peer not connected"));
            return;
        };
        let s = st.sessions.get_mut(&sid).expect("present");
        if kind == types::RTC_CANDIDATE {
            if from_controller {
                s.cand_to_host += 1;
            } else {
                s.cand_to_controller += 1;
            }
        }
        if kind == types::RTC_ANSWER && s.state == SessState::Started {
            s.state = SessState::Active;
            self.persist(PersistJob::SessionState {
                id: sid.clone(),
                state: "active",
                started_at: None,
            });
        }
        let forwarded = Envelope {
            v: farwing_protocol::PROTOCOL_VERSION,
            kind: env.kind.clone(),
            id: None,
            re: None,
            sid: Some(sid),
            generation: env.generation,
            data: env.data,
        };
        dest.send(&forwarded);
        if let Some(id) = re {
            conn.send(&Envelope::ack(id, json!({})));
        }
    }

    /// The controller leaves session `sid` carried by this link.
    fn on_session_leave(
        &self,
        st: &mut HubState,
        link_id: &str,
        conn: &ConnHandle,
        env: Envelope,
    ) {
        let re = env.id.as_deref();
        let Some(sid) = env.sid.clone() else {
            conn.send(&err_env(re, ErrorCode::BadRequest, "sid required"));
            return;
        };
        if st
            .sessions
            .get(&sid)
            .is_none_or(|s| s.controller_link.as_deref() != Some(link_id))
        {
            conn.send(&Envelope::error(re, session_ended("unknown session")));
            return;
        }
        // A per-session controller link closes with the session.
        if let Some(id) = re {
            conn.send(&Envelope::ack(id, json!({})));
        }
        self.end_session_locked(st, &sid, End::ControllerLeft);
    }

    fn on_session_end(&self, st: &mut HubState, link_id: &str, conn: &ConnHandle, env: Envelope) {
        let re = env.id.as_deref();
        let link = &st.links[link_id];
        if link.role != Role::Host {
            conn.send(&err_env(
                re,
                ErrorCode::Forbidden,
                "session.end is only valid on a host connection",
            ));
            return;
        }
        let Some(sid) = env.sid.as_deref() else {
            conn.send(&err_env(re, ErrorCode::BadRequest, "sid required"));
            return;
        };
        let device = link.device_id.clone();
        if st
            .sessions
            .get(sid)
            .is_none_or(|s| s.host_device_id != device)
        {
            conn.send(&Envelope::error(re, session_ended("unknown session")));
            return;
        }
        let requested = env
            .data
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or(reason::HOST_CLOSED);
        // Reasons a host may state; anything else is reported as `host_closed`.
        let mapped = [
            reason::REVOKED,
            reason::KICKED,
            reason::IDENTITY_MISMATCH,
            reason::MEDIA_FAILED,
            reason::CONNECTION_LOST,
        ]
        .into_iter()
        .find(|r| *r == requested)
        .unwrap_or(reason::HOST_CLOSED);
        self.end_session_locked(st, sid, End::HostClosed(mapped));
        if let Some(id) = re {
            conn.send(&Envelope::ack(id, json!({})));
        }
    }
}
