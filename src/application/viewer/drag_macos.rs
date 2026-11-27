//! One viewer drop target, as the Linux one is. winit reports the dropped
//! files on macOS; the hover preview and drags carried out of the window
//! wait on a real implementation (shortcut: drops are ignored until then).
use crate::{features::stream_control::StreamControlHandle, protocol::drag_drop::Point};
use winit::{dpi::PhysicalPosition, event::WindowEvent, window::Window};

pub(super) struct WindowDrag;

impl WindowDrag {
    pub(super) fn new(_owner: u64, _control: &StreamControlHandle) -> Self {
        Self
    }

    pub(super) fn event(&mut self, _event: &WindowEvent) -> bool {
        false
    }

    pub(super) fn tick(
        &mut self,
        _window: &Window,
        _ctx: &egui::Context,
        _available: bool,
        _point: impl Fn(PhysicalPosition<f64>) -> Option<Point>,
    ) {
    }

    pub(super) fn show(&mut self, _ctx: &egui::Context) {}
}
