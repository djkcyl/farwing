//! Distribution context is ordinary-user resource location, never host authority.
use anyhow::Result;
use farwing_bootstrap::{Ready, Runtime};
use std::{
    path::PathBuf,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
};
static RUNTIME: OnceLock<Runtime> = OnceLock::new();
static READY: Mutex<Option<Ready>> = Mutex::new(None);
static STARTED: AtomicBool = AtomicBool::new(false);

/// Called before argument parsing, logging or application worker creation.
pub fn initialize() -> Result<()> {
    crate::i18n::init();
    *READY.lock().unwrap_or_else(|e| e.into_inner()) = Ready::from_environment()?;
    if let Some(runtime) = Runtime::enter()? {
        let _ = RUNTIME.set(runtime);
    }
    // SAFETY: native main calls this before spawning threads. Do not propagate
    // untrusted bootstrap environment into services or plugin subprocesses.
    unsafe {
        std::env::remove_var(farwing_bootstrap::ORIGIN_ENV);
    }
    Ok(())
}
pub(crate) fn resource_origin() -> Option<PathBuf> {
    RUNTIME.get().and_then(|r| r.origin.clone())
}
pub(crate) fn cached() -> bool {
    RUNTIME.get().is_some()
}
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
                .map_or_else(farwing_bootstrap::collect, Runtime::collect)
            {
                tracing::debug!(%error,"runtime cleanup deferred");
            }
        });
}
/// Activation of an existing GUI must not replace its portable resource origin.
pub fn handoff() {
    if let Some(ready) = READY.lock().unwrap_or_else(|e| e.into_inner()).take() {
        if let Err(error) = ready.signal() {
            tracing::warn!(%error,"launcher handoff unavailable");
        }
    }
}
