//! Desktop notifications, viewer-only: the macOS port does not speak to
//! NSUserNotificationCenter yet, so showing a native notice fails and the
//! in-app notice window carries them alone. The activation sink plumbing is
//! the platform-neutral part both sides share.
use anyhow::{Context, Result};
use std::sync::{Mutex, OnceLock, mpsc};

type Action = crate::application::app::notifications::Action;

fn activation_sink() -> &'static Mutex<Option<mpsc::Sender<Action>>> {
    static SINK: OnceLock<Mutex<Option<mpsc::Sender<Action>>>> = OnceLock::new();
    SINK.get_or_init(Mutex::default)
}
pub(crate) fn set_activation_sink(sender: Option<mpsc::Sender<Action>>) {
    *activation_sink().lock().unwrap_or_else(|p| p.into_inner()) = sender;
}
pub(crate) fn receive_activation(uri: &str) -> bool {
    let Ok(action) = Action::parse(uri) else {
        return false;
    };
    activation_sink()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|s| s.send(action).is_ok())
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ToastAction {
    pub label: String,
    pub uri: String,
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Toast {
    /// File-transfer progress in per mille, updated in place.
    pub progress: Option<u16>,
    pub key: String,
    pub title: String,
    pub body: String,
    pub expires_at: i64,
    pub buttons: Vec<ToastAction>,
}

/// No native notification center integration yet; every synchronize fails and
/// the caller keeps the in-app notices.
pub(crate) struct Native;
impl Native {
    pub fn new() -> Result<Self> {
        bail_native()
    }
    pub fn synchronize(&mut self, _items: &[Toast]) -> Result<()> {
        bail_native()
    }
    pub fn clear(&mut self) -> Result<()> {
        Ok(())
    }
}

fn bail_native() -> Result<()> {
    anyhow::bail!("macOS 桌面通知尚未接入")
}

/// Keeps the in-app notice window in the lower right corner of its monitor,
/// above other windows. Same winit placement as the Linux side.
pub(crate) struct Placement {
    failed: bool,
}

impl Placement {
    pub(crate) fn new(window: &winit::window::Window) -> Result<Self> {
        window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
        let placement = Self { failed: false };
        let monitor = window
            .primary_monitor()
            .or_else(|| window.current_monitor())
            .context("找不到用于放置通知的显示器")?;
        placement.move_to(window, &monitor);
        Ok(placement)
    }

    /// Follows resolution changes of the monitor the window is on.
    pub(crate) fn refresh(&mut self, window: &winit::window::Window) {
        match window
            .current_monitor()
            .or_else(|| window.primary_monitor())
        {
            Some(monitor) => {
                self.failed = false;
                self.move_to(window, &monitor);
            }
            None => {
                if !self.failed {
                    tracing::warn!("notification placement unavailable");
                }
                self.failed = true;
            }
        }
    }

    fn move_to(&self, window: &winit::window::Window, monitor: &winit::monitor::MonitorHandle) {
        let size = window.outer_size();
        let margin =
            (crate::ui::theme::NOTIFICATION_MARGIN * window.scale_factor() as f32).round() as i32;
        let origin = monitor.position();
        let area = monitor.size();
        let right = origin.x + area.width as i32;
        let bottom = origin.y + area.height as i32;
        let position = winit::dpi::PhysicalPosition::new(
            (right - size.width as i32 - margin).max(origin.x),
            (bottom - size.height as i32 - margin).max(origin.y),
        );
        if window.outer_position().ok() != Some(position) {
            window.set_outer_position(position);
        }
    }
}
