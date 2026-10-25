//! The parts of the server that settings start, restart or swap while it
//! runs (`docs/protocol/deployment-design.md` §3): the HTTPS listener, its
//! certificate and ACME, the builtin relay and NAT discovery endpoints, the
//! hub's limits and ICE servers, the log filter. One task owns them and
//! applies changes one at a time.

use crate::config::{Config, SettingError, TlsMode, TurnConfig, TurnMode};
use crate::http::App;
use crate::hub::HubConfig;
use crate::ice::IceProvider;
use crate::tls::{self, CertSlot};
use crate::{stun_server, turn_server};
use anyhow::{Context, Result};
use axum::Router;
use farwing_protocol::realtime::Limits;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;

/// A changed listen address must be confirmed from the new address within
/// this time, or the old one comes back.
pub const LISTEN_CONFIRM: Duration = Duration::from_secs(60);

/// Why settings were not applied.
#[derive(Debug)]
pub enum ApplyError {
    Setting(SettingError),
    /// The runtime has stopped.
    Stopped,
}

impl From<SettingError> for ApplyError {
    fn from(e: SettingError) -> Self {
        Self::Setting(e)
    }
}

fn unavailable(field: &str, error: impl std::fmt::Display) -> ApplyError {
    ApplyError::Setting(SettingError::new(field, "unavailable", error.to_string()))
}

/// A listen change waiting for confirmation.
#[derive(Clone, Debug, serde::Serialize)]
pub struct ListenPending {
    /// Where the web console answers on the new listener.
    pub url: String,
    /// Unix seconds; the old listener comes back after this.
    pub confirm_by: i64,
}

/// What applying settings did.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Outcome {
    /// The listener moved: confirm from `listen_url` before `confirm_by`
    /// (Unix seconds), or the old one comes back.
    pub listen_url: Option<String>,
    pub confirm_by: Option<i64>,
    /// The certificate's fingerprint changed.
    pub fingerprint_changed: bool,
    /// The builtin relay restarted (relayed connections reconnect).
    pub relay_restarted: bool,
    /// The address clients connect to or the certificate changed: this many
    /// online clients were told.
    pub clients_told: Option<usize>,
}

/// What saving settings would do (nothing is saved).
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Preview {
    /// Saving restarts the builtin relay.
    pub relay_restarts: bool,
    /// Allocations on the builtin relay now: they end when it restarts.
    pub relay_allocations: u32,
}

enum Command {
    Preview {
        cfg: Box<Config>,
        reply: oneshot::Sender<Preview>,
    },
    Apply {
        cfg: Box<Config>,
        /// Load the certificate again even if the settings did not change
        /// (a new uploaded certificate).
        reload_tls: bool,
        reply: oneshot::Sender<Result<Outcome, ApplyError>>,
    },
    ConfirmListen {
        reply: oneshot::Sender<bool>,
    },
    RevertListen {
        reply: oneshot::Sender<bool>,
    },
    Shutdown {
        reply: oneshot::Sender<()>,
    },
}

/// Handle to the runtime task (held by the app).
#[derive(Clone)]
pub struct Control {
    tx: mpsc::Sender<Command>,
}

impl Control {
    /// Validate and apply `cfg`; it is in force (and stored) when this
    /// returns `Ok`, except a listen change, which waits for
    /// [`Control::confirm_listen`].
    pub async fn apply(&self, cfg: Config) -> Result<Outcome, ApplyError> {
        self.send_apply(cfg, false).await
    }

    /// [`Control::apply`], loading the certificate again.
    pub async fn reload_tls(&self, cfg: Config) -> Result<Outcome, ApplyError> {
        self.send_apply(cfg, true).await
    }

    async fn send_apply(&self, cfg: Config, reload_tls: bool) -> Result<Outcome, ApplyError> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Command::Apply {
                cfg: Box::new(cfg),
                reload_tls,
                reply,
            })
            .await
            .map_err(|_| ApplyError::Stopped)?;
        rx.await.map_err(|_| ApplyError::Stopped)?
    }

    /// What applying `cfg` would do.
    pub async fn preview(&self, cfg: Config) -> Preview {
        let (reply, rx) = oneshot::channel();
        let command = Command::Preview {
            cfg: Box::new(cfg),
            reply,
        };
        if self.tx.send(command).await.is_err() {
            return Preview::default();
        }
        rx.await.unwrap_or_default()
    }

    /// Keep the new listen address; `false` when none is waiting.
    pub async fn confirm_listen(&self) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::ConfirmListen { reply }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    /// Go back to the old listen address now; `false` when no change waits.
    pub async fn revert_listen(&self) -> bool {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::RevertListen { reply }).await.is_err() {
            return false;
        }
        rx.await.unwrap_or(false)
    }

    pub async fn shutdown(&self) {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(Command::Shutdown { reply }).await.is_ok() {
            let _ = rx.await;
        }
    }
}

pub fn hub_config(cfg: &Config) -> HubConfig {
    let l = &cfg.limits;
    HubConfig {
        heartbeat_ms: l.heartbeat_ms,
        idle_timeout_ms: l.idle_timeout_ms,
        resume_ttl: Duration::from_secs(l.resume_ttl_s),
        ticket_ttl: Duration::from_secs(l.ticket_ttl_s),
        limits: Limits {
            max_frame_bytes: l.max_frame_bytes,
            max_sdp_bytes: l.max_sdp_bytes,
            max_candidates: l.max_candidates,
            rate_per_s: l.ws_rate_per_s,
        },
        max_candidate_bytes: l.max_candidate_bytes,
        force_relay: cfg.turn.force_relay,
    }
}

/// Settings of the builtin relay and discovery endpoints that need a
/// restart when they change (TURN over TLS needs the web certificate, which
/// a reverse proxy holds instead).
fn relay_key(cfg: &Config) -> impl PartialEq + use<> {
    let t = &cfg.turn;
    (
        (t.mode, t.relay),
        (t.listen_udp, t.listen_tcp, t.listen_tls),
        t.public_ip,
        t.realm.clone(),
        t.relay_bind,
        t.relay_ip,
        (t.relay_port_min, t.relay_port_max),
        t.shared_secret.clone(),
        t.stun.clone(),
        cfg.tls.mode == TlsMode::Proxy,
    )
}

/// The builtin relay (or, with `relay` off, its discovery endpoints) can
/// run with these settings.
fn relay_ready(t: &TurnConfig) -> bool {
    t.mode == TurnMode::Builtin && t.builtin_public_ip().is_some()
}

/// The host clients reach TURN over TLS at: the public URL's name (on the
/// certificate), else the public IP.
fn tls_host(cfg: &Config) -> Option<String> {
    let url = url::Url::parse(&cfg.public_url).ok()?;
    match url.host()? {
        url::Host::Domain(name) => Some(name.to_owned()),
        _ => None,
    }
}

/// What the HTTPS listener depends on.
fn listener_key(cfg: &Config) -> (SocketAddr, bool) {
    (cfg.listen, cfg.tls.mode == TlsMode::Proxy)
}

/// What the certificate depends on.
fn tls_key(cfg: &Config) -> impl PartialEq + use<> {
    (
        cfg.tls.clone(),
        tls::self_signed_names(cfg),
        cfg.acme_domains(),
        cfg.public_url.clone(),
    )
}

struct Listener {
    addr: SocketAddr,
    proxy: bool,
    handle: axum_server::Handle<SocketAddr>,
    task: JoinHandle<std::io::Result<()>>,
}

impl Listener {
    async fn stop(self, grace: Duration) {
        self.handle.graceful_shutdown(Some(grace));
        let _ = tokio::time::timeout(grace + Duration::from_secs(1), self.task).await;
    }
}

struct Pending {
    listener: Listener,
    /// The settings before the change, back in force if not confirmed.
    previous: Arc<Config>,
    /// The old listener while it still runs (a different address).
    old: Option<Listener>,
    deadline: Instant,
}

/// Bound addresses of the relay and discovery endpoints.
#[derive(Clone, Debug, Default)]
pub struct RelayAddrs {
    pub turn_udp: Option<SocketAddr>,
    pub turn_tcp: Option<SocketAddr>,
    pub turn_tls: Option<SocketAddr>,
    pub stun_udp: Vec<SocketAddr>,
}

pub struct Components {
    app: App,
    router: Router,
    slot: Arc<CertSlot>,
    tls_config: Arc<rustls::ServerConfig>,
    listener: Option<Listener>,
    pending: Option<Pending>,
    turn: Option<turn_server::BuiltinTurn>,
    stun: Option<stun_server::StunEndpoints>,
    acme_tasks: Vec<JoinHandle<()>>,
}

/// What `start` reports to the caller.
pub struct Started {
    pub control: Control,
    pub addr: SocketAddr,
    pub fingerprint: Option<String>,
    pub relay: RelayAddrs,
    pub task: JoinHandle<()>,
}

fn serve(
    listener: std::net::TcpListener,
    proxy: bool,
    tls_config: &Arc<rustls::ServerConfig>,
    router: &Router,
) -> Result<Listener> {
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let handle = axum_server::Handle::<SocketAddr>::new();
    let make = router
        .clone()
        .into_make_service_with_connect_info::<SocketAddr>();
    let task = if proxy {
        let server = axum_server::from_tcp(listener)?
            .acceptor(axum_server::accept::NoDelayAcceptor::new())
            .handle(handle.clone());
        tokio::spawn(server.serve(make))
    } else {
        let rustls = axum_server::tls_rustls::RustlsConfig::from_config(Arc::clone(tls_config));
        // Without TCP_NODELAY the second small realtime message of a
        // burst (an answer and its ICE candidates) waits for the peer's
        // delayed ACK, tens of milliseconds.
        let acceptor = axum_server::tls_rustls::RustlsAcceptor::new(rustls)
            .acceptor(axum_server::accept::NoDelayAcceptor::new());
        let server = axum_server::from_tcp(listener)?
            .acceptor(acceptor)
            .handle(handle.clone());
        tokio::spawn(server.serve(make))
    };
    Ok(Listener {
        addr,
        proxy,
        handle,
        task,
    })
}

fn bind(addr: SocketAddr) -> Result<std::net::TcpListener> {
    std::net::TcpListener::bind(addr).with_context(|| format!("binding {addr}"))
}

impl Components {
    /// Start everything for the settings in force. `listener` (already
    /// bound) replaces `cfg.listen` (tests bind port 0 first).
    pub async fn start(
        app: App,
        router: Router,
        listener: Option<std::net::TcpListener>,
    ) -> Result<Started> {
        let cfg = app.cfg();
        let prepared = tls::prepare(&cfg)?;
        let slot = CertSlot::new(
            prepared
                .as_ref()
                .map_or_else(tls::no_certificate, |p| Arc::clone(&p.resolver)),
        );
        let tls_config = tls::server_config(Arc::clone(&slot))?;
        let mut components = Self {
            app: Arc::clone(&app),
            router,
            slot,
            tls_config,
            listener: None,
            pending: None,
            turn: None,
            stun: None,
            acme_tasks: Vec::new(),
        };
        let fingerprint = match prepared {
            Some(prepared) => components.install_prepared(&cfg, prepared)?,
            None => None,
        };
        components
            .start_relay(&cfg)
            .await
            .map_err(|e| anyhow::anyhow!("{e:?}"))?;
        components.reconfigure_hub(&cfg);
        let std_listener = match listener {
            Some(l) => l,
            None => bind(cfg.listen)?,
        };
        let proxy = cfg.tls.mode == TlsMode::Proxy;
        if proxy {
            tracing::info!("TLS terminated by a reverse proxy (tls.mode = proxy)");
        }
        let l = serve(std_listener, proxy, &components.tls_config, &components.router)?;
        let addr = l.addr;
        components.listener = Some(l);
        let relay = components.relay_addrs();
        let (tx, rx) = mpsc::channel(8);
        let task = tokio::spawn(components.run(rx));
        Ok(Started {
            control: Control { tx },
            addr,
            fingerprint,
            relay,
            task,
        })
    }

    fn relay_addrs(&self) -> RelayAddrs {
        RelayAddrs {
            turn_udp: self.turn.as_ref().map(|t| t.udp_addr()),
            turn_tcp: self.turn.as_ref().and_then(|t| t.tcp_addr()),
            turn_tls: self.turn.as_ref().and_then(|t| t.tls_addr()),
            stun_udp: self
                .stun
                .as_ref()
                .map(|s| s.local_addrs().to_vec())
                .unwrap_or_default(),
        }
    }

    /// Put the certificate for `cfg` in the slot (and run ACME); the
    /// presented fingerprint.
    fn install_tls(&mut self, cfg: &Config) -> Result<Option<String>> {
        for t in self.acme_tasks.drain(..) {
            t.abort();
        }
        let Some(prepared) = tls::prepare(cfg)? else {
            *self.app.tls.write() = None;
            return Ok(None);
        };
        self.install_prepared(cfg, prepared)
    }

    fn install_prepared(
        &mut self,
        cfg: &Config,
        prepared: tls::Prepared,
    ) -> Result<Option<String>> {
        for t in self.acme_tasks.drain(..) {
            t.abort();
        }
        self.slot.set(Arc::clone(&prepared.resolver));
        let fingerprint = prepared.presented.fingerprint();
        tracing::info!(sha256_fingerprint = %fingerprint, mode = ?cfg.tls.mode, "TLS certificate in use");
        *self.app.tls.write() = Some(prepared.presented);
        if let Some(acme) = prepared.acme {
            // HTTP-01 answers and the redirect to public_url on port 80.
            if let Some(http) = cfg.tls.acme.http_listen {
                let listener = bind(http)?;
                listener.set_nonblocking(true)?;
                let listener = tokio::net::TcpListener::from_std(listener)?;
                tracing::info!(listen = %http, "HTTP listener for ACME challenges and redirects");
                let router = crate::acme::http_router(Arc::clone(&acme));
                self.acme_tasks.push(tokio::spawn(async move {
                    if let Err(e) = axum::serve(listener, router).await {
                        tracing::warn!(error = %e, "HTTP listener stopped");
                    }
                }));
            }
            self.acme_tasks.push(tokio::spawn(acme.run()));
        }
        Ok(Some(fingerprint))
    }

    async fn stop_relay(&mut self) {
        if let Some(turn) = self.turn.take() {
            turn.close().await;
        }
        if let Some(stun) = self.stun.take() {
            stun.close().await;
        }
    }

    /// Start the builtin relay and discovery endpoints for `cfg` (nothing
    /// when not ready).
    async fn start_relay(&mut self, cfg: &Config) -> Result<(), ApplyError> {
        let t = &cfg.turn;
        if !relay_ready(t) {
            if t.mode == TurnMode::Builtin {
                tracing::info!("builtin relay waits for this server's public address");
            }
            return Ok(());
        }
        if !t.stun.is_empty() {
            self.stun = Some(
                stun_server::start(t)
                    .await
                    .map_err(|e| unavailable("turn.stun", format!("{e:#}")))?,
            );
        }
        if !t.relay || t.listen_udp.is_none() {
            return Ok(());
        }
        let tls = (cfg.tls.mode != TlsMode::Proxy)
            .then(|| tls::turns_config(Arc::clone(&self.slot)))
            .transpose()
            .map_err(|e| unavailable("turn.listen_tls", format!("{e:#}")))?;
        match turn_server::start(t, tls).await {
            Ok(turn) => self.turn = Some(turn),
            Err(e) => {
                if let Some(stun) = self.stun.take() {
                    stun.close().await;
                }
                let text = format!("{e:#}");
                let field = if text.contains("TURN TLS") {
                    "turn.listen_tls"
                } else if text.contains("TURN TCP") {
                    "turn.listen_tcp"
                } else {
                    "turn.listen_udp"
                };
                return Err(unavailable(field, text));
            }
        }
        Ok(())
    }

    fn reconfigure_hub(&self, cfg: &Config) {
        let stun_public = self.stun.as_ref().map_or(&[][..], |s| s.public_addrs());
        // Advertise what the builtin relay actually runs (a configured port
        // 0 binds any).
        let mut turn = cfg.turn.clone();
        let bound = self.relay_addrs();
        if turn.mode == TurnMode::Builtin {
            turn.listen_udp = bound.turn_udp;
            turn.listen_tcp = bound.turn_tcp;
            turn.listen_tls = bound.turn_tls;
        }
        let ice = IceProvider::new(&turn, tls_host(cfg).as_deref()).with_stun_endpoints(stun_public);
        *self.app.nat_discovery.write() = stun_public
            .iter()
            .map(|addr| match addr {
                SocketAddr::V4(v4) => format!("stun:{}:{}", v4.ip(), v4.port()),
                SocketAddr::V6(v6) => format!("stun:[{}]:{}", v6.ip(), v6.port()),
            })
            .collect();
        self.app.hub.reconfigure(hub_config(cfg), ice);
        self.app.edges.reconfigure(&cfg.turn, &cfg.sfu);
        *self.app.relay.write() = bound;
    }

    /// What clients connect to: the public URL and the certificate served
    /// here (none behind a reverse proxy).
    fn address(&self) -> (String, Option<String>) {
        let tls = self.app.tls.read().as_ref().map(|t| t.fingerprint());
        (self.app.cfg().public_url.clone(), tls)
    }

    /// Tell the online clients where to connect, if that changed: how many
    /// were told.
    fn announce(&self, before: (String, Option<String>)) -> Option<usize> {
        let (url, sha256) = self.address();
        if (&url, &sha256) == (&before.0, &before.1) {
            return None;
        }
        let changed = farwing_protocol::realtime::ServerChanged {
            url,
            previous: before.0,
            sha256,
        };
        let sent = self.app.hub.broadcast(&farwing_protocol::realtime::Envelope::new(
            farwing_protocol::realtime::types::SERVER_CHANGED,
            &changed,
        ));
        tracing::info!(url = %changed.url, previous = %changed.previous,
            certificate = changed.sha256.is_some(), clients = sent, "address announced to online clients");
        Some(sent)
    }

    async fn run(mut self, mut rx: mpsc::Receiver<Command>) {
        let mut stats = tokio::time::interval(Duration::from_secs(10));
        loop {
            let deadline = self.pending.as_ref().map(|p| p.deadline);
            let command = tokio::select! {
                command = rx.recv() => command,
                _ = stats.tick() => {
                    let current = match &self.turn {
                        Some(turn) => turn.stats().await,
                        None => Default::default(),
                    };
                    *self.app.local_relay.write() = current;
                    continue;
                }
                _ = async {
                    match deadline {
                        Some(at) => tokio::time::sleep_until(at).await,
                        None => std::future::pending().await,
                    }
                } => {
                    let before = self.address();
                    let _ = self.revert_listen().await;
                    self.announce(before);
                    continue;
                }
            };
            match command {
                Some(Command::Preview { cfg, reply }) => {
                    let old = self.app.cfg();
                    let relay_restarts = relay_key(&cfg) != relay_key(&old)
                        && (relay_ready(&cfg.turn) || relay_ready(&old.turn));
                    let relay_allocations = match &self.turn {
                        Some(turn) => turn.stats().await.allocations,
                        None => 0,
                    };
                    let _ = reply.send(Preview {
                        relay_restarts,
                        relay_allocations,
                    });
                }
                Some(Command::Apply {
                    cfg,
                    reload_tls,
                    reply,
                }) => {
                    let before = self.address();
                    let mut result = self.apply(*cfg, reload_tls).await;
                    let told = self.announce(before);
                    if let Ok(outcome) = &mut result {
                        outcome.clients_told = told;
                    }
                    let _ = reply.send(result);
                }
                Some(Command::ConfirmListen { reply }) => {
                    let _ = reply.send(self.confirm_listen().await);
                }
                Some(Command::RevertListen { reply }) => {
                    let before = self.address();
                    let reverted = self.revert_listen().await;
                    self.announce(before);
                    let _ = reply.send(reverted);
                }
                Some(Command::Shutdown { reply }) => {
                    self.stop().await;
                    let _ = reply.send(());
                    return;
                }
                None => {
                    self.stop().await;
                    return;
                }
            }
        }
    }

    async fn stop(&mut self) {
        *self.app.listen_pending.write() = None;
        if let Some(p) = self.pending.take() {
            p.listener.stop(Duration::from_secs(5)).await;
            if let Some(old) = p.old {
                old.stop(Duration::from_secs(5)).await;
            }
        }
        if let Some(l) = self.listener.take() {
            l.stop(Duration::from_secs(5)).await;
        }
        for t in self.acme_tasks.drain(..) {
            t.abort();
        }
        self.stop_relay().await;
    }

    async fn apply(&mut self, mut new: Config, reload_tls: bool) -> Result<Outcome, ApplyError> {
        let old = self.app.cfg();
        new.data_dir = old.data_dir.clone();
        new.listen_locked = old.listen_locked;
        if old.listen_locked && new.listen != old.listen {
            return Err(SettingError::new(
                "listen",
                "locked",
                "the listen address is fixed by --listen",
            )
            .into());
        }
        new.validate()?;
        let listen_changed = listener_key(&new) != listener_key(&old);
        if listen_changed {
            if self.pending.is_some() {
                return Err(SettingError::new(
                    "listen",
                    "conflict",
                    "another listen change waits for confirmation",
                )
                .into());
            }
            let mut alone = new.clone();
            alone.listen = old.listen;
            alone.tls.mode = old.tls.mode;
            alone.public_url = old.public_url.clone();
            if alone != *old {
                return Err(SettingError::new(
                    "listen",
                    "conflict",
                    "save the listen address and the reverse proxy mode on their own",
                )
                .into());
            }
        }
        let mut outcome = Outcome::default();

        // Prepare everything that can fail before anything changes.
        let tls_changed = reload_tls || tls_key(&new) != tls_key(&old);
        let prepared = if tls_changed {
            tls::prepare(&new).map_err(|e| unavailable("tls", format!("{e:#}")))?
        } else {
            None
        };
        let moved = listen_changed && new.listen != old.listen;
        let new_socket = if moved {
            Some(bind(new.listen).map_err(|e| unavailable("listen", format!("{e:#}")))?)
        } else {
            None
        };
        if relay_key(&new) != relay_key(&old) {
            self.stop_relay().await;
            if let Err(e) = self.start_relay(&new).await {
                // Back to the relay that ran before.
                let _ = self.start_relay(&old).await;
                return Err(e);
            }
            outcome.relay_restarted = relay_ready(&new.turn) || relay_ready(&old.turn);
        }

        // Commit.
        if tls_changed {
            let before = self.app.tls.read().as_ref().map(|t| t.fingerprint());
            let after = match prepared {
                Some(prepared) => self
                    .install_prepared(&new, prepared)
                    .map_err(|e| unavailable("tls", format!("{e:#}")))?,
                None => {
                    for t in self.acme_tasks.drain(..) {
                        t.abort();
                    }
                    *self.app.tls.write() = None;
                    None
                }
            };
            outcome.fingerprint_changed = before != after;
            if let Some(after) = after.filter(|_| outcome.fingerprint_changed) {
                self.app.edges.control_fingerprint(&after);
            }
        }
        self.reconfigure_hub(&new);
        if new.sfu != old.sfu || new.turn.relay_bind != old.turn.relay_bind
            || new.turn.builtin_public_ip() != old.turn.builtin_public_ip()
        {
            self.app.forwarding.reconfigure(&new).await;
        }
        if new.log != old.log {
            crate::server::reload_log(&new.log);
        }
        if listen_changed {
            let proxy = new.tls.mode == TlsMode::Proxy;
            let socket = match new_socket {
                Some(s) => s,
                None => {
                    // Same address, other protocol: the old listener goes now.
                    if let Some(l) = self.listener.take() {
                        l.stop(Duration::from_millis(500)).await;
                    }
                    bind(new.listen).map_err(|e| unavailable("listen", format!("{e:#}")))?
                }
            };
            let listener = serve(socket, proxy, &self.tls_config, &self.router)
                .map_err(|e| unavailable("listen", format!("{e:#}")))?;
            let url = listen_url(&new, listener.addr);
            tracing::info!(listen = %listener.addr, proxy, "new listener waits for confirmation");
            self.pending = Some(Pending {
                listener,
                previous: Arc::clone(&old),
                old: self.listener.take(),
                deadline: Instant::now() + LISTEN_CONFIRM,
            });
            let confirm_by = crate::db::now_s() + LISTEN_CONFIRM.as_secs() as i64;
            *self.app.listen_pending.write() = Some(ListenPending {
                url: url.clone(),
                confirm_by,
            });
            outcome.listen_url = Some(url);
            outcome.confirm_by = Some(confirm_by);
            // In force now; stored once confirmed.
            self.app.settings.replace(new);
            return Ok(outcome);
        }
        let stored = new.clone();
        self.app
            .db
            .call(move |c| crate::settings::save(c, &stored))
            .await
            .map_err(|e| unavailable("settings", format!("{e:#}")))?;
        self.app.settings.replace(new);
        Ok(outcome)
    }

    async fn confirm_listen(&mut self) -> bool {
        let Some(p) = self.pending.take() else {
            return false;
        };
        *self.app.listen_pending.write() = None;
        // In the background: the confirmation may have come through it.
        if let Some(old) = p.old {
            tokio::spawn(old.stop(Duration::from_secs(5)));
        }
        self.listener = Some(p.listener);
        let cfg = (*self.app.cfg()).clone();
        let saved = self
            .app
            .db
            .call(move |c| crate::settings::save(c, &cfg))
            .await;
        if let Err(e) = saved {
            tracing::warn!(error = %format!("{e:#}"), "storing the confirmed listen address failed");
        }
        tracing::info!("new listen address confirmed");
        true
    }

    async fn revert_listen(&mut self) -> bool {
        let Some(p) = self.pending.take() else {
            return false;
        };
        *self.app.listen_pending.write() = None;
        tracing::warn!(listen = %p.listener.addr, "listen change not confirmed; reverting");
        let proxy_changed = p.listener.proxy != (p.previous.tls.mode == TlsMode::Proxy);
        match p.old {
            Some(old) => {
                self.listener = Some(old);
                // In the background: the request to revert may have come
                // through it.
                tokio::spawn(p.listener.stop(Duration::from_millis(500)));
            }
            None => {
                p.listener.stop(Duration::from_millis(500)).await;
                let restored = bind(p.previous.listen).and_then(|s| {
                    serve(
                        s,
                        p.previous.tls.mode == TlsMode::Proxy,
                        &self.tls_config,
                        &self.router,
                    )
                });
                match restored {
                    Ok(l) => self.listener = Some(l),
                    Err(e) => tracing::error!(error = %format!("{e:#}"), "restoring the listener failed"),
                }
            }
        }
        if proxy_changed && let Err(e) = self.install_tls(&p.previous) {
            tracing::error!(error = %format!("{e:#}"), "restoring the certificate failed");
        }
        self.app.settings.replace((*p.previous).clone());
        true
    }
}

/// Where the web console continues after a listen change.
fn listen_url(cfg: &Config, bound: SocketAddr) -> String {
    let scheme = if cfg.tls.mode == TlsMode::Proxy { "http" } else { "https" };
    let host = url::Url::parse(&cfg.public_url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_else(|| bound.ip().to_string());
    format!("{scheme}://{host}:{}", bound.port())
}
