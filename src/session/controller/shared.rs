//! One network owner per device; viewing and TCP mapping hold separate leases.
use super::*;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, Weak};
use tokio::sync::watch;

#[derive(Clone)]
struct End {
    message: String,
    signal: Option<SignalFailure>,
}

pub(super) struct Session {
    pub peer: Arc<NativePeer>,
    forwarder: Mutex<Option<RtpForwarder>>,
    shutdown: Mutex<Option<oneshot::Sender<()>>>,
    ended: watch::Receiver<Option<Option<End>>>,
    viewing: AtomicBool,
    owner: Mutex<Option<CancellationToken>>,
}
fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}
fn pool() -> &'static Mutex<HashMap<String, Weak<Session>>> {
    static POOL: OnceLock<Mutex<HashMap<String, Weak<Session>>>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
pub(super) fn key(controller: &str, target: &str) -> String {
    format!("{controller}:{target}")
}
pub(super) fn get(key: &str) -> Option<Arc<Session>> {
    let mut pool = lock(pool());
    pool.retain(|_, s| s.strong_count() > 0);
    pool.get(key).and_then(Weak::upgrade).filter(|s| {
        s.ended.borrow().is_none()
            && lock(&s.owner)
                .as_ref()
                .is_none_or(|owner| !owner.is_cancelled())
    })
}
pub(super) fn register(key: String, session: &Arc<Session>, owner: CancellationToken) {
    *lock(&session.owner) = Some(owner);
    lock(pool()).insert(key, Arc::downgrade(session));
}
pub(super) fn connection_gate(key: &str) -> Arc<tokio::sync::Mutex<()>> {
    static GATES: OnceLock<Mutex<HashMap<String, Weak<tokio::sync::Mutex<()>>>>> = OnceLock::new();
    let mut gates = lock(GATES.get_or_init(Mutex::default));
    gates.retain(|_, gate| gate.strong_count() > 0);
    if let Some(gate) = gates.get(key).and_then(Weak::upgrade) {
        return gate;
    }
    let gate = Arc::new(tokio::sync::Mutex::new(()));
    gates.insert(key.into(), Arc::downgrade(&gate));
    gate
}
pub(super) async fn acquire_connection(
    key: &str,
    cancel: &CancellationToken,
) -> Result<tokio::sync::OwnedMutexGuard<()>> {
    cancellable(cancel, async {
        Ok(connection_gate(key).lock_owned().await)
    })
    .await
}
fn jobs() -> &'static Mutex<Vec<(Weak<Session>, CancellationToken)>> {
    static JOBS: OnceLock<Mutex<Vec<(Weak<Session>, CancellationToken)>>> = OnceLock::new();
    JOBS.get_or_init(Mutex::default)
}
pub(super) async fn shutdown_all() {
    let jobs = std::mem::take(&mut *lock(jobs()));
    for (session, _) in &jobs {
        if let Some(session) = session.upgrade() {
            session.request_close();
        }
    }
    for (_, done) in jobs {
        done.cancelled().await;
    }
}
impl Session {
    pub(super) fn activity(&self) -> super::LocalConnectionActivity {
        super::LocalConnectionActivity {
            viewing: self.viewing.load(Ordering::Acquire),
            controlling: self.peer.stream_control_handle().mouse().mode()
                != crate::features::remote_input::MouseMode::View,
        }
    }
    pub fn new(
        peer: Arc<NativePeer>,
        forwarder: RtpForwarder,
        shutdown: oneshot::Sender<()>,
        task: tokio::task::JoinHandle<Result<()>>,
    ) -> Arc<Self> {
        let (tx, ended) = watch::channel(None);
        let done = CancellationToken::new();
        let finish = done.clone();
        tokio::spawn(async move {
            let _finish = finish.drop_guard();
            let result = flatten_signal_task(task.await);
            tx.send_replace(Some(result.err().map(|e| End {
                signal: e.downcast_ref::<SignalFailure>().cloned(),
                message: format!("{e:#}"),
            })));
        });
        let session = Arc::new(Self {
            peer,
            forwarder: Mutex::new(Some(forwarder)),
            shutdown: Mutex::new(Some(shutdown)),
            ended,
            viewing: AtomicBool::new(false),
            owner: Mutex::new(None),
        });
        {
            let mut jobs = lock(jobs());
            jobs.retain(|(_, done)| !done.is_cancelled());
            jobs.push((Arc::downgrade(&session), done));
        }
        session
    }
    pub fn lease(self: &Arc<Self>, viewing: bool) -> Result<ForwarderLease> {
        let forwarder = if viewing {
            anyhow::ensure!(
                self.viewing
                    .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok(),
                "该设备的观看窗口已打开"
            );
            lock(&self.forwarder).take()
        } else {
            None
        };
        Ok(ForwarderLease {
            session: Arc::clone(self),
            forwarder,
            viewing,
        })
    }
    pub async fn ended(&self) -> Result<()> {
        self.end_waiter().await
    }
    pub fn end_waiter(
        &self,
    ) -> impl std::future::Future<Output = Result<()>> + Send + 'static + use<> {
        let mut rx = self.ended.clone();
        async move {
            loop {
                if let Some(end) = rx.borrow_and_update().clone() {
                    return match end {
                        None => Ok(()),
                        Some(e) => Err(e
                            .signal
                            .map(anyhow::Error::new)
                            .unwrap_or_else(|| anyhow!(e.message))),
                    };
                }
                if rx.changed().await.is_err() {
                    bail!("设备连接已结束");
                }
            }
        }
    }
    pub fn request_close(&self) {
        if let Some(tx) = lock(&self.shutdown).take() {
            let _ = tx.send(());
        }
    }
}
impl Drop for Session {
    fn drop(&mut self) {
        self.request_close();
    }
}
pub(super) struct ForwarderLease {
    pub session: Arc<Session>,
    forwarder: Option<RtpForwarder>,
    pub viewing: bool,
}
impl std::ops::Deref for ForwarderLease {
    type Target = RtpForwarder;
    fn deref(&self) -> &Self::Target {
        self.forwarder
            .as_ref()
            .expect("viewing lease owns media receiver")
    }
}
impl std::ops::DerefMut for ForwarderLease {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.forwarder
            .as_mut()
            .expect("viewing lease owns media receiver")
    }
}
impl Drop for ForwarderLease {
    fn drop(&mut self) {
        if self.viewing {
            *lock(&self.session.forwarder) = self.forwarder.take();
            self.session.viewing.store(false, Ordering::Release);
        }
    }
}
