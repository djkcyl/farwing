//! Process-wide UI dispatcher. Window creation and destruction stay on one thread.
use super::{WindowConfig, app::AppFactory};
use anyhow::{Context, Result};
use std::{
    sync::{Mutex, OnceLock},
    time::Instant,
};
use winit::{event_loop::EventLoopProxy, window::WindowId};

pub(crate) struct Repaint {
    pub window: WindowId,
    pub generation: u64,
    pub pass: u64,
    pub when: Instant,
}
pub(crate) enum Event {
    Repaint(Repaint),
    Request(Request),
}
pub(crate) enum Request {
    ShowMain,
    Exit,
    Open {
        key: String,
        config: WindowConfig,
        factory: AppFactory,
    },
    Viewer {
        key: String,
        config: crate::application::viewer::windows_presenter::ConnectingWindowsRunConfig,
        done: tokio::sync::oneshot::Sender<Result<()>>,
    },
    Focus(String),
}
fn dispatcher() -> &'static Mutex<Option<EventLoopProxy<Event>>> {
    static DISPATCHER: OnceLock<Mutex<Option<EventLoopProxy<Event>>>> = OnceLock::new();
    DISPATCHER.get_or_init(Mutex::default)
}
pub(crate) fn install(proxy: Option<EventLoopProxy<Event>>) {
    *dispatcher().lock().unwrap_or_else(|e| e.into_inner()) = proxy;
}
pub(crate) fn send(request: Request) -> Result<()> {
    dispatcher()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .context("窗口管理器已关闭")?
        .send_event(Event::Request(request))
        .map_err(|_| anyhow::anyhow!("窗口管理器已关闭"))
}
pub(crate) async fn viewer(
    key: String,
    config: crate::application::viewer::windows_presenter::ConnectingWindowsRunConfig,
) -> Result<()> {
    let (done, closed) = tokio::sync::oneshot::channel();
    send(Request::Viewer { key, config, done })?;
    closed.await.context("观看窗口已关闭")?
}
