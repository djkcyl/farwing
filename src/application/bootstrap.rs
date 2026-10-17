//! Distribution context is ordinary-user resource location, never host authority.
use anyhow::Result;
#[cfg(windows)]
use openuuyc_bootstrap::{Ready, Runtime};
#[cfg(windows)]
use std::{
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
#[cfg(windows)]
static RUNTIME: OnceLock<Runtime> = OnceLock::new();
#[cfg(windows)]
static READY: Mutex<Option<Ready>> = Mutex::new(None);
#[cfg(windows)]
static STARTED: AtomicBool = AtomicBool::new(false);

/// Called before argument parsing, logging or application worker creation.
#[cfg(windows)]
pub fn initialize() -> Result<()> {
    *READY.lock().unwrap_or_else(|e| e.into_inner()) = Ready::from_environment()?;
    if let Some(runtime) = Runtime::enter()? {
        let _ = RUNTIME.set(runtime);
    }
    // SAFETY: native main calls this before spawning threads. Do not propagate
    // untrusted bootstrap environment into services or plugin subprocesses.
    unsafe {
        std::env::remove_var(openuuyc_bootstrap::ORIGIN_ENV);
    }
    Ok(())
}
#[cfg(windows)]
pub(crate) fn resource_origin() -> Option<PathBuf> {
    RUNTIME.get().and_then(|r| r.origin.clone())
}
#[cfg(windows)]
pub(crate) fn cached() -> bool {
    RUNTIME.get().is_some()
}
#[cfg(windows)]
pub fn ready() {
    if STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    if let Some(runtime) = RUNTIME.get() {
        if let Err(error) = runtime.ready() {
            tracing::warn!(%error,"runtime launch record unavailable");
        }
    }
    handoff();
    let _ = std::thread::Builder::new()
        .name("runtime-cleanup".into())
        .spawn(|| {
            if let Err(error) = RUNTIME
                .get()
                .map_or_else(openuuyc_bootstrap::collect, Runtime::collect)
            {
                tracing::debug!(%error,"runtime cleanup deferred");
            }
        });
}
/// Activation of an existing GUI must not replace its portable resource origin.
#[cfg(windows)]
pub fn handoff() {
    if let Some(ready) = READY.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if let Err(error) = ready.signal() {
            tracing::warn!(%error,"launcher handoff unavailable");
        }
    }
}

// Linux ships one binary with no compressed launcher in front of it: there is
// no cached runtime to enter, no resource origin and no launcher to signal.
#[cfg(not(windows))]
pub fn initialize() -> Result<()> {
    Ok(())
}
#[cfg(not(windows))]
pub(crate) fn resource_origin() -> Option<std::path::PathBuf> {
    None
}
#[cfg(not(windows))]
pub fn ready() {}
#[cfg(not(windows))]
pub fn handoff() {}
