//! Server assembly: database, hub, background tasks, listeners.

use crate::config::{Config, TurnMode};
use crate::db::{self, Db};
use crate::http::{self, App, AppState};
use crate::hub::{Hub, HubConfig, HubEvent, PersistJob};
use crate::ice::IceProvider;
use crate::{stun_server, tls, turn_server};
use anyhow::{Context, Result};
use farwing_protocol::realtime::{AssistCancelled, DeviceChangeKind, Envelope, Limits, types};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

pub struct RunningServer {
    /// Bound HTTPS (or proxy-mode HTTP) address.
    pub addr: SocketAddr,
    pub app: App,
    /// SHA-256 fingerprint of the served certificate (TLS mode).
    pub fingerprint: Option<String>,
    pub turn_udp: Option<SocketAddr>,
    pub turn_tcp: Option<SocketAddr>,
    /// Bound addresses of the `[[turn.stun]]` endpoints (builtin mode).
    pub stun_udp: Vec<SocketAddr>,
    handle: axum_server::Handle<SocketAddr>,
    server_task: JoinHandle<std::io::Result<()>>,
    tasks: Vec<JoinHandle<()>>,
    turn: Option<turn_server::BuiltinTurn>,
    stun: Option<stun_server::StunEndpoints>,
}

fn hub_config(cfg: &Config) -> HubConfig {
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
        network: cfg.network.clone(),
    }
}

/// Start the server. `listener` (already bound) is used instead of
/// `cfg.listen` when given (tests bind port 0 first).
pub async fn start(cfg: Config, listener: Option<std::net::TcpListener>) -> Result<RunningServer> {
    tls::install_crypto_provider();
    for w in cfg.warnings() {
        tracing::warn!("{w}");
    }
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating data_dir {}", cfg.data_dir.display()))?;
    std::fs::create_dir_all(cfg.wallpaper_dir())?;
    let db = Db::open(&cfg.db_path())?;
    let stale = db.with(|c| db::end_open_sessions(c, "server_restart"))?;
    if stale > 0 {
        tracing::info!(stale, "closed sessions left open by a previous run");
    }
    let last_event = db.with(|c| db::max_event_id(c))?;

    // NAT behaviour discovery endpoints; clients get them as stun: URLs.
    let stun = if cfg.turn.mode == TurnMode::Builtin && !cfg.turn.stun.is_empty() {
        Some(stun_server::start(&cfg.turn).await?)
    } else {
        None
    };
    let stun_public = stun.as_ref().map_or(&[][..], |s| s.public_addrs());
    let ice = IceProvider::new(&cfg.turn).with_stun_endpoints(stun_public);
    let stun_udp = stun
        .as_ref()
        .map(|s| s.local_addrs().to_vec())
        .unwrap_or_default();
    let (hub, channels) = Hub::new(hub_config(&cfg), ice);
    let tls = tls::load(&cfg)?;
    let stored = db.with(|c| crate::webdb::settings(c))?;
    let settings = crate::settings::RuntimeSettings::load(&cfg, &stored);
    let cfg = Arc::new(cfg);
    let app: App = Arc::new(AppState {
        cfg: Arc::clone(&cfg),
        db: db.clone(),
        hub: Arc::clone(&hub),
        assist: Default::default(),
        limiter: Default::default(),
        replay: Default::default(),
        path_prefix: cfg.public_path_prefix(),
        password_slots: tokio::sync::Semaphore::new(
            std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4)),
        ),
        endpoints: vec![cfg.ws_endpoint()],
        nat_discovery: stun_public
            .iter()
            .map(|addr| match addr {
                std::net::SocketAddr::V4(v4) => format!("stun:{}:{}", v4.ip(), v4.port()),
                std::net::SocketAddr::V6(v6) => format!("stun:[{}]:{}", v6.ip(), v6.port()),
            })
            .collect(),
        settings: parking_lot::RwLock::new(settings),
        started_at: std::time::Instant::now(),
        tls: tls.as_ref().map(|t| match &t.acme {
            Some(acme) => tls::Presented::Acme(Arc::clone(acme)),
            None => tls::Presented::Fixed(t.fingerprint.clone()),
        }),
    });

    let turn = if cfg.turn.mode == TurnMode::Builtin {
        Some(turn_server::start(&cfg.turn).await?)
    } else {
        None
    };
    let turn_udp = turn.as_ref().map(|t| t.udp_addr());
    let turn_tcp = turn.as_ref().and_then(|t| t.tcp_addr());

    let mut tasks = Vec::new();
    // Ordered persistence of session state.
    {
        let db = db.clone();
        let mut rx = channels.persist;
        tasks.push(tokio::spawn(async move {
            while let Some(job) = rx.recv().await {
                let res = db
                    .call(move |c| match job {
                        PersistJob::SessionCreated(rec) => db::insert_session(c, &rec),
                        PersistJob::SessionState {
                            id,
                            state,
                            started_at,
                        } => db::set_session_state(c, &id, state, started_at),
                        PersistJob::SessionEnded { id, reason } => db::end_session(c, &id, &reason),
                        PersistJob::LastSeen(id) => db::touch_last_seen(c, &id).map(|_| ()),
                    })
                    .await;
                if let Err(e) = res {
                    tracing::warn!(error = %format!("{e:#}"), "persisting realtime state failed");
                }
            }
        }));
    }
    // device.changed pushes for presence changes.
    {
        let app = Arc::clone(&app);
        let mut rx = channels.events;
        tasks.push(tokio::spawn(async move {
            while let Some(ev) = rx.recv().await {
                match ev {
                    HubEvent::PresenceChanged(device_id) => {
                        http::devices::notify_changed(
                            &app,
                            &device_id,
                            DeviceChangeKind::Updated,
                            false,
                        )
                        .await;
                    }
                }
            }
        }));
    }
    // Expiry sweeps.
    {
        let app = Arc::clone(&app);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            let mut n: u64 = 0;
            loop {
                tick.tick().await;
                app.hub.sweep();
                for (request_id, host) in app.assist.prune() {
                    app.hub.push(
                        &host,
                        &Envelope::new(types::ASSIST_CANCELLED, AssistCancelled { request_id }),
                    );
                }
                n += 1;
                if n.is_multiple_of(240) {
                    app.limiter.prune(Duration::from_secs(3600));
                    app.replay.prune();
                }
            }
        }));
    }
    // Changes made by the admin CLI while the server runs.
    {
        let app = Arc::clone(&app);
        tasks.push(tokio::spawn(async move {
            let mut last = last_event;
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                let events = match app.db.call(move |c| db::events_after(c, last)).await {
                    Ok(e) => e,
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "reading admin events failed");
                        continue;
                    }
                };
                for ev in events {
                    last = ev.id;
                    apply_admin_event(&app, ev).await;
                }
            }
        }));
    }
    // Audit log retention (`[web] audit_days`).
    {
        let app = Arc::clone(&app);
        tasks.push(tokio::spawn(async move {
            let keep = i64::from(app.cfg.web.audit_days.max(1)) * 86_400;
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                match app
                    .db
                    .call(move |c| crate::webdb::prune_audit(c, keep))
                    .await
                {
                    Ok(0) => {}
                    Ok(n) => tracing::info!(removed = n, "old audit entries removed"),
                    Err(e) => tracing::warn!(error = %format!("{e:#}"), "pruning the audit log failed"),
                }
            }
        }));
    }

    let router = http::router(Arc::clone(&app));
    let listener = match listener {
        Some(l) => l,
        None => std::net::TcpListener::bind(cfg.listen)
            .with_context(|| format!("binding {}", cfg.listen))?,
    };
    listener.set_nonblocking(true)?;
    let addr = listener.local_addr()?;
    let handle = axum_server::Handle::<SocketAddr>::new();
    let make = router.into_make_service_with_connect_info::<SocketAddr>();
    let fingerprint = tls.as_ref().map(|t| t.fingerprint.clone());
    let acme = tls.as_ref().and_then(|t| t.acme.clone());
    let server_task = match tls {
        Some(material) => {
            tracing::info!(
                cert = %material.cert_path.display(),
                sha256_fingerprint = %material.fingerprint,
                "TLS certificate loaded"
            );
            let rustls = axum_server::tls_rustls::RustlsConfig::from_config(material.config);
            let server =
                axum_server::tls_rustls::from_tcp_rustls(listener, rustls)?.handle(handle.clone());
            tokio::spawn(server.serve(make))
        }
        None => {
            tracing::info!("TLS terminated by a reverse proxy (tls.mode = \"proxy\")");
            let server = axum_server::from_tcp(listener)?.handle(handle.clone());
            tokio::spawn(server.serve(make))
        }
    };
    // ACME: obtain and renew once the TLS listener answers TLS-ALPN-01; the
    // plain-HTTP listener answers HTTP-01 and redirects to public_url.
    if let Some(acme) = acme {
        if let Some(http) = cfg.tls.acme.http_listen {
            let listener = tokio::net::TcpListener::bind(http)
                .await
                .with_context(|| format!("binding [tls.acme] http_listen {http}"))?;
            tracing::info!(listen = %http, "HTTP listener for ACME challenges and redirects");
            let router = crate::acme::http_router(Arc::clone(&acme));
            tasks.push(tokio::spawn(async move {
                if let Err(e) = axum::serve(listener, router).await {
                    tracing::warn!(error = %e, "HTTP listener stopped");
                }
            }));
        }
        tasks.push(tokio::spawn(acme.run()));
    }
    tracing::info!(
        listen = %addr,
        public_url = %cfg.public_url,
        endpoint = %cfg.ws_endpoint(),
        turn = ?cfg.turn.mode,
        guests = app.settings.read().guests,
        "Farwing Server {} started",
        env!("CARGO_PKG_VERSION")
    );
    Ok(RunningServer {
        addr,
        app,
        fingerprint,
        turn_udp,
        turn_tcp,
        stun_udp,
        handle,
        server_task,
        tasks,
        turn,
        stun,
    })
}

async fn apply_admin_event(app: &App, ev: db::AdminEvent) {
    match ev.kind.as_str() {
        db::EVENT_REVOKE_USER => {
            if let Some(uid) = ev.user_id.as_deref() {
                tracing::info!(user_id = uid, "admin: revoking user sessions");
                app.hub.revoke_user(uid);
            }
        }
        db::EVENT_REVOKE_DEVICE => {
            let Some(did) = ev.device_id.clone() else {
                return;
            };
            tracing::info!(device_id = %did, "admin: revoking device");
            app.hub.revoke_device(&did);
            // The CLI already unbound the device (and dropped its grants):
            // tell the former owner's devices and the owner's friends.
            let Some(owner) = ev.user_id.clone() else {
                return;
            };
            let (id, uid) = (did.clone(), owner.clone());
            let Ok((Some(row), Some(owner_row), friends)) = app
                .db
                .call(move |c| {
                    Ok((
                        db::get_device(c, &id)?,
                        db::get_user(c, &uid)?,
                        db::friend_ids(c, &uid)?,
                    ))
                })
                .await
            else {
                return;
            };
            let mut device = http::devices::build_device(app, &row, &http::devices::Viewer::Owner);
            device.status = farwing_protocol::api::STATUS_OFFLINE.to_owned();
            device.participants.clear();
            let env = Envelope::new(
                types::DEVICE_CHANGED,
                farwing_protocol::realtime::DeviceChanged {
                    kind: DeviceChangeKind::Removed,
                    device,
                    shared: false,
                },
            );
            http::devices::send_to_user(app, &owner, Some(&did), &env).await;
            let summary = http::devices::user_summary(&owner_row);
            for friend in friends {
                http::devices::push_shared(
                    app,
                    &friend,
                    &row,
                    &summary,
                    Vec::new(),
                    DeviceChangeKind::Removed,
                )
                .await;
            }
        }
        other => tracing::warn!(kind = other, "unknown admin event"),
    }
}

impl RunningServer {
    /// Graceful shutdown: notify sessions, close connections, stop listeners.
    pub async fn shutdown(self) {
        tracing::info!("shutting down");
        self.app.hub.shutdown();
        // Give connection tasks a moment to flush their close frames.
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.handle.graceful_shutdown(Some(Duration::from_secs(5)));
        let _ = tokio::time::timeout(Duration::from_secs(10), self.server_task).await;
        for t in self.tasks {
            t.abort();
        }
        if let Some(turn) = self.turn {
            turn.close().await;
        }
        // Dropping the STUN endpoints stops them.
        drop(self.stun);
        tracing::info!("stopped");
    }
}

/// Wait for Ctrl-C or SIGTERM.
pub async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = term => {},
    }
}

/// `farwing-server serve`.
pub async fn serve(cfg: Config) -> Result<()> {
    let server = start(cfg, None).await?;
    shutdown_signal().await;
    server.shutdown().await;
    Ok(())
}
