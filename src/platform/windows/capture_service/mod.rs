//! Privileged capture only. Account, encoding and network ownership stay in the GUI.
mod agent;
mod client;
mod texture;
use super::{
    capture::Screen,
    host_service::{pipe::Pipe, process},
};
pub(crate) use agent::run as agent;
use anyhow::{Result, ensure};
pub(crate) use client::Client;
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
static NAME: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Capture.v1"));
static AGENT_PREFIX: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| crate::product::pipe_name("Capture.Agent."));
const LIMIT: u32 = 8;

#[derive(Serialize, Deserialize)]
enum Request {
    Open {
        screen: Screen,
        client: u32,
    },
    Next {
        timeout: u32,
        quality: i32,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
        surface: Option<u64>,
    },
    Close,
}
#[derive(Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
struct Layout {
    width: u32,
    height: u32,
    format: i32,
    adapter: u64,
}
#[derive(Serialize, Deserialize)]
struct Reply {
    screen: Screen,
    generation: u64,
    available: bool,
    dxgi: bool,
    hdr: bool,
    layout: Option<Layout>,
    needs_surface: bool,
    frame: bool,
    is_new: bool,
    captured_qpc: i64,
    metadata: Option<crate::media::video_color::HdrMetadata>,
    pointer: Option<super::cursor_shape::Snapshot>,
    gone: bool,
    error: Option<String>,
}
fn counter() -> Result<(i64, i64)> {
    use windows::Win32::System::Performance::*;
    let (mut now, mut frequency) = (0, 0);
    unsafe {
        QueryPerformanceCounter(&mut now)?;
        QueryPerformanceFrequency(&mut frequency)?;
    }
    ensure!(frequency > 0, "采集时钟不可用");
    Ok((now, frequency))
}
pub(crate) fn serve(running: impl Fn() -> bool + Sync) -> Result<()> {
    std::thread::scope(|scope| -> Result<()> {
        let mut workers = Vec::new();
        let mut listener = Pipe::listener(&NAME, true, LIMIT, true)?;
        while running() {
            workers.retain(|thread: &std::thread::ScopedJoinHandle<'_, ()>| !thread.is_finished());
            if workers.len() >= LIMIT as usize - 1 {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            if listener.accept(&running).is_err() {
                continue;
            }
            // Keep a listening instance alive so another process cannot claim the name.
            let next = Pipe::listener(&NAME, true, LIMIT, false)?;
            let accepted = std::mem::replace(&mut listener, next);
            let running = &running;
            workers.push(scope.spawn(move || {
                if let Err(error) = relay(accepted, running) {
                    tracing::debug!(%error,"capture service connection ended");
                }
            }));
        }
        Ok(())
    })
}
fn relay(pipe: Pipe, running: &impl Fn() -> bool) -> Result<()> {
    let pid = pipe.peer_pid(true)?;
    super::host_service::install::verify_client(pid)?;
    let session = process::session(pid)?;
    let active =
        || running() && process::active_session() == session && pipe.queued_bytes().is_ok();
    ensure!(active(), "采集请求不属于当前Windows会话");
    let Request::Open { screen, .. } = pipe.receive(&active)? else {
        anyhow::bail!("缺少采集服务握手")
    };
    let name = format!("{}{}", *AGENT_PREFIX, uuid::Uuid::new_v4());
    let target = Pipe::server(&name, false)?;
    let child = process::Agent::start_capture(session, &name)?;
    let started = Instant::now();
    target.accept(|| active() && child.alive() && started.elapsed() < Duration::from_secs(10))?;
    ensure!(target.peer_pid(true)? == child.pid, "采集会话进程身份无效");
    target.send(
        &Request::Open {
            screen,
            client: pid,
        },
        &active,
    )?;
    pipe.send(
        &target.receive_timeout::<Reply>(Duration::from_secs(10), &active)?,
        &active,
    )?;
    while active() && child.alive() {
        if !pipe.available()? {
            continue;
        }
        let request: Request = pipe.receive(&active)?;
        let close = matches!(request, Request::Close);
        ensure!(!matches!(request, Request::Open { .. }), "重复采集握手");
        target.send(&request, &active)?;
        if close {
            break;
        }
        let reply: Reply = target.receive(&active)?;
        pipe.send(&reply, &active)?;
    }
    Ok(())
}
