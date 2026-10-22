//! System events that interrupt hosting: suspend and a change of the active
//! Windows session (fast user switching, a remote desktop takeover).
//! Participants are told before the interruption (`HOST_EVENT`, §10).
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Power::{
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, PowerRegisterSuspendResumeNotification,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMSUSPEND,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Event {
    /// The system is about to sleep or hibernate.
    Suspending,
    Resumed,
    /// Another Windows session became the active console session.
    SessionSwitched,
}

static HANDLER: std::sync::OnceLock<fn(Event)> = std::sync::OnceLock::new();
static STARTED: AtomicBool = AtomicBool::new(false);
static POWER_REGISTERED: std::sync::Once = std::sync::Once::new();
/// Woken when the system resumes from sleep (for a viewer: its network
/// paths may be gone). Waiters must be waiting when it fires.
pub(crate) static RESUMED: tokio::sync::Notify = tokio::sync::Notify::const_new();

/// How long the suspend notification is held so a just-queued frame leaves
/// before the network goes down.
const SUSPEND_GRACE: Duration = Duration::from_millis(300);
const SESSION_POLL: Duration = Duration::from_secs(1);

unsafe extern "system" fn power_callback(
    _context: *const core::ffi::c_void,
    kind: u32,
    _setting: *const core::ffi::c_void,
) -> u32 {
    let event = match kind {
        PBT_APMSUSPEND => Event::Suspending,
        PBT_APMRESUMEAUTOMATIC => Event::Resumed,
        _ => return 0,
    };
    if event == Event::Resumed {
        RESUMED.notify_waiters();
    }
    if let Some(handler) = HANDLER.get() {
        handler(event);
        if event == Event::Suspending {
            std::thread::sleep(SUSPEND_GRACE);
        }
    }
    0
}

/// Report events to `handler` for the rest of the process (idempotent; the
/// first handler wins). Handlers run on system or watcher threads and must
/// only queue work.
pub(crate) fn start(handler: fn(Event)) {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    let _ = HANDLER.set(handler);
    register_power();
    let spawned = std::thread::Builder::new()
        .name("farwing-session-watch".into())
        .spawn(|| {
            let mut active = super::host_service::process::active_session();
            loop {
                std::thread::sleep(SESSION_POLL);
                let now = super::host_service::process::active_session();
                if now != active {
                    tracing::info!(from = active, to = now, "active Windows session changed");
                    active = now;
                    if let Some(handler) = HANDLER.get() {
                        handler(Event::SessionSwitched);
                    }
                }
            }
        });
    if let Err(error) = spawned {
        tracing::warn!(%error, "session watcher unavailable");
    }
}

/// Subscribe to suspend and resume notifications once per process (the
/// registration lives as long as the process).
fn register_power() {
    POWER_REGISTERED.call_once(|| {
        let parameters = Box::leak(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_callback),
            Context: std::ptr::null_mut(),
        }));
        let mut registration = std::ptr::null_mut();
        let status = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                HANDLE((parameters as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS).cast()),
                &mut registration,
            )
        };
        if status.is_err() {
            tracing::warn!(?status, "suspend notifications unavailable");
        }
    });
}

/// Make [`RESUMED`] fire in this process (a viewer that is not a host).
pub(crate) fn watch_resume() {
    register_power();
}
