use super::{AGENT_PREFIX, Layout, Reply, Request, texture::Surface};
use crate::platform::windows::{
    capture::{Desktop, Frame},
    encoder,
    host_service::{install, pipe::Pipe, process},
    input::system,
};
use anyhow::{Result, ensure};
use std::time::Duration;
use windows::Win32::Graphics::Direct3D11::*;
pub(crate) fn run(name: &str, parent: u32) -> Result<()> {
    ensure!(
        name.starts_with(AGENT_PREFIX) && name.len() < 200,
        "采集会话管道无效"
    );
    install::verify_running(parent)?;
    let pipe = Pipe::client(name)?.ok_or_else(|| anyhow::anyhow!("采集会话已结束"))?;
    ensure!(pipe.peer_pid(false)? == parent, "采集服务身份无效");
    let session = process::session(std::process::id())?;
    let active = || process::active_session() == session && pipe.queued_bytes().is_ok();
    let Request::Open {
        screen: mut selected,
        client,
    } = pipe.receive(&active)?
    else {
        anyhow::bail!("缺少采集握手")
    };
    ensure!(
        process::session(client)? == session,
        "采集目标不属于本Windows会话"
    );
    install::verify_client(client)?;
    let _runtime = encoder::Runtime::new()?;
    let mut context = system::Desktop::new()?;
    if let Some((desktop, name)) = context.changed()? {
        context.switch(desktop, name)?;
    }
    let mut capture = Some(Desktop::open_local(&selected)?);
    let mut generation = 0u64;
    let mut native_generation = 0u64;
    let mut surface = None::<Surface>;
    let mut pending = None::<Frame>;
    let reply = Reply {
        screen: selected.clone(),
        generation,
        available: true,
        dxgi: capture.as_ref().unwrap().backend_name() == "DXGI",
        hdr: capture.as_ref().unwrap().hdr_available(),
        layout: None,
        needs_surface: false,
        frame: false,
        is_new: false,
        captured_qpc: 0,
        metadata: None,
        pointer: None,
        gone: false,
        error: None,
    };
    pipe.send(&reply, &active)?;
    while active() {
        if !pipe.available()? {
            continue;
        }
        let request: Request = pipe.receive(&active)?;
        if matches!(request, Request::Close) {
            break;
        }
        let Request::Next {
            timeout,
            quality,
            cursor,
            hdr,
            maximum,
            surface: offered,
        } = request
        else {
            anyhow::bail!("重复采集握手")
        };
        ensure!(
            timeout <= 100
                && (1..=6).contains(&quality)
                && maximum.0 <= 16384
                && maximum.1 <= 16384,
            "采集参数无效"
        );
        let mut reply = Reply {
            screen: selected.clone(),
            generation,
            available: false,
            dxgi: true,
            hdr: false,
            layout: None,
            needs_surface: false,
            frame: false,
            is_new: false,
            captured_qpc: 0,
            metadata: None,
            pointer: None,
            gone: false,
            error: None,
        };
        let result = (|| -> Result<()> {
            if let Some((desktop, name)) = context.changed()? {
                pending = None;
                surface = None;
                capture = None;
                context.switch(desktop, name)?;
                generation = generation.wrapping_add(1);
                native_generation = 0;
                tracing::info!(
                    ordinary = context.ordinary(),
                    "capture input desktop changed"
                );
            }
            if capture.is_none() {
                capture = Some(Desktop::open_local(&selected)?);
            }
            let capture = capture.as_mut().unwrap();
            let frame = if pending.is_some() {
                pending.take()
            } else {
                capture.next(timeout, quality, cursor, hdr, maximum)?
            };
            if native_generation != capture.generation {
                native_generation = capture.generation;
                generation = generation.wrapping_add(1);
                surface = None;
            }
            selected = capture.screen.clone();
            reply.screen = selected.clone();
            reply.generation = generation;
            reply.available = capture.available;
            reply.dxgi = capture.backend_name() == "DXGI";
            reply.hdr = capture.hdr_available();
            reply.pointer = capture.cursor.clone();
            let Some(frame) = frame else { return Ok(()) };
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            unsafe {
                frame.image.GetDesc(&mut desc);
            }
            let layout = Layout {
                width: desc.Width,
                height: desc.Height,
                format: desc.Format.0,
                adapter: selected.adapter,
            };
            reply.layout = Some(layout);
            if let Some(handle) = offered {
                surface = Some(Surface::open(&capture.device, client, handle, layout)?);
            }
            if surface.as_ref().is_none_or(|s| s.layout != layout) {
                pending = Some(frame);
                reply.needs_surface = true;
                return Ok(());
            }
            ensure!(active(), "采集已撤销");
            let surface = surface.as_ref().unwrap();
            let Some(guard) = surface.acquire(0)? else {
                // Preserve the capture source and request a fresh shared
                // surface. Never copy into a mutex we failed to acquire.
                pending = Some(frame);
                reply.needs_surface = true;
                return Ok(());
            };
            let gpu = unsafe { capture.device.GetImmediateContext()? };
            unsafe {
                gpu.CopyResource(&surface.texture, &frame.image);
                gpu.Flush();
            }
            guard.release(1)?;
            let (now, frequency) = super::counter()?;
            reply.captured_qpc = now
                .saturating_sub((frame.captured.elapsed().as_secs_f64() * frequency as f64) as i64);
            reply.frame = true;
            reply.is_new = frame.is_new;
            reply.metadata = frame.hdr_metadata;
            Ok(())
        })();
        if let Err(error) = result {
            if error
                .downcast_ref::<crate::media::capture::SourceGone>()
                .is_some()
            {
                reply.gone = true;
                reply.error = Some(error.to_string());
            } else {
                tracing::debug!(error=%format!("{error:#}"),"privileged capture temporarily unavailable");
                pending = None;
                surface = None;
                capture = None;
                generation = generation.wrapping_add(1);
                reply.generation = generation;
                reply.available = false;
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        pipe.send(&reply, &active)?;
    }
    Ok(())
}
