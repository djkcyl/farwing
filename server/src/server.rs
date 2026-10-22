//! Server assembly: settings, database, hub, background tasks and the
//! components that apply settings (`runtime`).

use crate::config::Config;
use crate::db::{self, Db};
use crate::http::{self, App, AppState};
use crate::hub::{Hub, HubEvent, PersistJob};
use crate::ice::IceProvider;
use crate::runtime::{self, Components, Control};
use crate::tls;
use anyhow::{Context, Result};
use farwing_protocol::realtime::{AssistCancelled, DeviceChangeKind, Envelope, types};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};
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
    pub turn_tls: Option<SocketAddr>,
    /// Bound addresses of the NAT discovery endpoints (builtin relay).
    pub stun_udp: Vec<SocketAddr>,
    /// The one-time setup code while no administrator exists.
    pub setup_code: Option<String>,
    control: Control,
    runtime: JoinHandle<()>,
    tasks: Vec<JoinHandle<()>>,
}

/// Replaces the process's log filter (set by the binary).
static LOG_RELOAD: OnceLock<Box<dyn Fn(&str) + Send + Sync>> = OnceLock::new();

pub fn set_log_reloader(f: impl Fn(&str) + Send + Sync + 'static) {
    let _ = LOG_RELOAD.set(Box::new(f));
}

pub(crate) fn reload_log(directive: &str) {
    if let Some(f) = LOG_RELOAD.get() {
        f(directive);
    }
}

/// Start the server with `cfg` (stored as the settings in force).
/// `listener` (already bound) is used instead of `cfg.listen` when given
/// (tests bind port 0 first).
pub async fn start(cfg: Config, listener: Option<std::net::TcpListener>) -> Result<RunningServer> {
    tls::install_crypto_provider();
    for w in cfg.warnings() {
        tracing::warn!("{w}");
    }
    std::fs::create_dir_all(&cfg.data_dir)
        .with_context(|| format!("creating the data directory {}", cfg.data_dir.display()))?;
    std::fs::create_dir_all(cfg.wallpaper_dir())?;
    let db = Db::open(&cfg.db_path())?;
    let stale = db.with(|c| db::end_open_sessions(c, "server_restart"))?;
    if stale > 0 {
        tracing::info!(stale, "closed sessions left open by a previous run");
    }
    db.with(|c| crate::settings::save(c, &cfg))?;
    let needs_setup = db.with(|c| crate::setup::needs_setup(c))?;
    let setup_code = if needs_setup {
        Some(db.with(|c| crate::setup::new_code(c))?)
    } else {
        None
    };
    let (hub, channels) = Hub::new(runtime::hub_config(&cfg), IceProvider::new(&cfg.turn, None));
    let app: App = Arc::new(AppState {
        settings: crate::settings::Settings::new(cfg),
        db: db.clone(),
        hub: Arc::clone(&hub),
        assist: Default::default(),
        limiter: Default::default(),
        replay: Default::default(),
        password_slots: tokio::sync::Semaphore::new(
            std::thread::available_parallelism().map_or(2, |n| n.get().clamp(1, 4)),
        ),
        nat_discovery: Default::default(),
        relay: Default::default(),
        started_at: std::time::Instant::now(),
        tls: Default::default(),
        setup_pending: AtomicBool::new(needs_setup),
        listen_pending: parking_lot::RwLock::new(None),
        control: OnceLock::new(),
    });
    let router = http::router(Arc::clone(&app));
    let started = Components::start(Arc::clone(&app), router, listener).await?;
    let _ = app.control.set(started.control.clone());

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
    // Audit log retention (`web.audit_days`).
    {
        let app = Arc::clone(&app);
        tasks.push(tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(3600));
            loop {
                tick.tick().await;
                let keep = i64::from(app.cfg().web.audit_days.max(1)) * 86_400;
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


    let cfg = app.cfg();
    tracing::info!(
        listen = %started.addr,
        public_url = %cfg.public_url,
        endpoint = %cfg.ws_endpoint(),
        turn = ?cfg.turn.mode,
        guests = cfg.guest.enabled,
        "Farwing Server {} started",
        env!("CARGO_PKG_VERSION")
    );
    if let Some(code) = &setup_code {
        tracing::info!(
            "This server has not been set up. Open https://<this server>:{}/setup and enter the setup code {code} (valid for 24 hours).",
            started.addr.port()
        );
    }
    Ok(RunningServer {
        addr: started.addr,
        app,
        fingerprint: started.fingerprint,
        turn_udp: started.relay.turn_udp,
        turn_tcp: started.relay.turn_tcp,
        turn_tls: started.relay.turn_tls,
        stun_udp: started.relay.stun_udp,
        setup_code,
        control: started.control,
        runtime: started.task,
        tasks,
    })
}

impl RunningServer {
    /// Graceful shutdown: notify sessions, close connections, stop listeners.
    pub async fn shutdown(self) {
        tracing::info!("shutting down");
        self.app.hub.shutdown();
        // Give connection tasks a moment to flush their close frames.
        tokio::time::sleep(Duration::from_millis(200)).await;
        self.control.shutdown().await;
        let _ = tokio::time::timeout(Duration::from_secs(10), self.runtime).await;
        for t in self.tasks {
            t.abort();
        }
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

/// Run the server from the settings stored in `data_dir` until Ctrl-C or
/// SIGTERM. `listen` fixes the listen address.
pub async fn serve(data_dir: PathBuf, listen: Option<SocketAddr>) -> Result<()> {
    std::fs::create_dir_all(&data_dir)
        .with_context(|| format!("creating the data directory {}", data_dir.display()))?;
    let cfg = {
        let conn = db::open_connection(&data_dir.join("farwing.db"))?;
        crate::settings::load(&conn, &data_dir, listen)?
    };
    reload_log(&cfg.log);
    let server = start(cfg, None).await?;
    shutdown_signal().await;
    server.shutdown().await;
    Ok(())
}
