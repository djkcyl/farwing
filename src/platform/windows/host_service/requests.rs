//! What the background host asks of the service: work that needs SYSTEM
//! (shutting down, Wake-on-LAN adapter setup), handles to devices whose
//! access lists admit only SYSTEM or the signed-in user, and which user
//! backend the service started. Only the service's current resident child is
//! answered; the service opens the devices itself and never a path it is
//! given.
use super::pipe::{Handle, Pipe};
use crate::i18n::t;
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

static NAME: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Resident.Requests.v1"));

#[derive(Serialize, Deserialize)]
pub(crate) enum Request {
    /// Shut down or restart Windows (permitted by the host and the
    /// participant before it is asked for).
    Power(crate::features::host::power::Action),
    /// Turn on Wake-on-LAN on one adapter.
    WolConfigure(crate::platform::windows::wol::setup::AdapterKey),
    /// A handle to a device, duplicated into the caller.
    Device(Device),
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub(crate) enum Device {
    /// The virtual display driver.
    Display,
    /// The remote microphone's audio bridge.
    AudioBridge,
}

#[derive(Serialize, Deserialize)]
enum Reply {
    Done,
    Adapters(Vec<crate::platform::windows::wol::setup::Adapter>),
    /// A handle value valid in the caller's process.
    Handle(u64),
    Error(String),
}

// ------------------------------------------------------------ service side

/// Serve the resident `child` (its PID, 0 while there is none).
pub(super) fn serve(running: &impl Fn() -> bool, child: &AtomicU32) -> Result<()> {
    while running() {
        let pipe = Pipe::server(&NAME, false)?;
        if pipe.accept(running).is_err() {
            continue;
        }
        if let Err(error) = connection(&pipe, running, child) {
            tracing::debug!(%error, "resident request connection ended");
        }
    }
    Ok(())
}

fn connection(pipe: &Pipe, running: &impl Fn() -> bool, child: &AtomicU32) -> Result<()> {
    let pid = pipe.peer_pid(true)?;
    ensure!(
        pid != 0 && pid == child.load(Ordering::Acquire),
        "{}",
        t().system.service_resident_identity_mismatch
    );
    let current = || running() && child.load(Ordering::Acquire) == pid;
    while current() {
        if !pipe.available_within(super::resident::IDLE_WAIT)? {
            continue;
        }
        let request: Request = pipe.receive_timeout(Duration::from_secs(5), current)?;
        let reply = handle(request, pid).unwrap_or_else(|error| Reply::Error(format!("{error:#}")));
        pipe.send(&reply, current)?;
    }
    Ok(())
}

fn handle(request: Request, pid: u32) -> Result<Reply> {
    Ok(match request {
        Request::Power(action) => {
            crate::platform::windows::system_power::execute(action)?;
            Reply::Done
        }
        Request::WolConfigure(key) => Reply::Adapters(crate::platform::windows::wol::setup::configure(
            &key,
            &tokio_util::sync::CancellationToken::new(),
        )?),
        Request::Device(device) => Reply::Handle(duplicate_into(open(device)?, pid)?),
    })
}

fn open(device: Device) -> Result<Handle> {
    match device {
        Device::Display => crate::platform::windows::display::virtual_driver::Driver::raw(),
        Device::AudioBridge => crate::platform::windows::virtual_audio::Bridge::raw(),
    }
}

fn duplicate_into(handle: Handle, pid: u32) -> Result<u64> {
    use windows::Win32::{Foundation::*, System::Threading::*};
    let target = Handle(unsafe { OpenProcess(PROCESS_DUP_HANDLE, false, pid)? });
    let mut duplicated = HANDLE::default();
    unsafe {
        DuplicateHandle(
            GetCurrentProcess(),
            handle.0,
            target.0,
            &mut duplicated,
            0,
            false,
            DUPLICATE_SAME_ACCESS,
        )?;
    }
    Ok(duplicated.0 as u64)
}

// ----------------------------------------------------------- resident side

fn ask(request: Request) -> Result<Reply> {
    static CONNECTION: std::sync::Mutex<Option<Pipe>> = std::sync::Mutex::new(None);
    let mut connection = CONNECTION.lock().unwrap_or_else(|e| e.into_inner());
    if connection.as_ref().is_some_and(|pipe| pipe.queued_bytes().is_err()) {
        connection.take();
    }
    if connection.is_none() {
        let pipe = Pipe::client(&NAME)?.context(t().system.service_resident_not_started)?;
        // The service started this process; its PID is the parent's.
        ensure!(
            Some(pipe.peer_pid(false)?) == super::resident::service_pid(),
            "{}",
            t().system.service_invalid_identity
        );
        *connection = Some(pipe);
    }
    let pipe = connection.as_ref().unwrap();
    // Wake-on-LAN setup runs PowerShell and may take a while.
    let result = pipe
        .send(&request, || true)
        .and_then(|_| pipe.receive_timeout::<Reply>(Duration::from_secs(90), || true));
    if result.is_err() {
        connection.take();
    }
    match result? {
        Reply::Error(error) => anyhow::bail!(error),
        reply => Ok(reply),
    }
}

pub(crate) fn power(action: crate::features::host::power::Action) -> Result<()> {
    ask(Request::Power(action)).map(drop)
}

pub(crate) fn wol_configure(
    key: &crate::platform::windows::wol::setup::AdapterKey,
) -> Result<Vec<crate::platform::windows::wol::setup::Adapter>> {
    match ask(Request::WolConfigure(key.clone()))? {
        Reply::Adapters(adapters) => Ok(adapters),
        _ => anyhow::bail!("{}", t().system.service_message_invalid),
    }
}

/// `device`, opened by the service for this process.
pub(crate) fn device(device: Device) -> Result<Handle> {
    match ask(Request::Device(device))? {
        Reply::Handle(value) => Ok(Handle(windows::Win32::Foundation::HANDLE(value as *mut _))),
        _ => anyhow::bail!("{}", t().system.service_message_invalid),
    }
}
