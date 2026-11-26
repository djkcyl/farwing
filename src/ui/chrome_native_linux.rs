//! Window manipulation for X11 and Wayland behind the shared borderless chrome.
//!
//! Wayland gives a client no way to place its own window, so moving and
//! resizing are handed to the compositor through winit's drag requests. The
//! gesture then runs outside the application, which is why there is no cursor
//! tracking or pointer capture here.
use winit::dpi::PhysicalSize;
use winit::window::{Fullscreen, ResizeDirection, Window};

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowMoveState {
    /// Set while the compositor owns an interactive move for this window.
    dragging: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WindowResizeState {
    dragging: bool,
    pub(crate) min_size: Option<PhysicalSize<u32>>,
    /// The compositor decides the new size, so nothing is requested up front.
    pub(crate) requested_render_size: Option<PhysicalSize<u32>>,
}

pub(crate) fn cancel_pointer_operation(
    moving: &mut WindowMoveState,
    resizing: &mut WindowResizeState,
) {
    moving.dragging = false;
    resizing.dragging = false;
}

/// The window manager sizes a fullscreen window itself; nothing to prepare.
pub(crate) fn set_fullscreen(window: &Window, fullscreen: Option<Fullscreen>) {
    window.set_fullscreen(fullscreen);
    configure_dwm_window(window);
}

/// The desktop environment owns titlebar styling; only the icon is ours.
pub(crate) fn configure_dwm_window(window: &Window) {
    window.set_window_icon(Some(crate::ui::branding::window_icon()));
}

pub(crate) fn update_nonmodal_window_move(
    _ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    state: &mut WindowMoveState,
) {
    if window.fullscreen().is_some() {
        state.dragging = false;
        return;
    }
    if response.double_clicked() {
        state.dragging = false;
        if window.is_resizable()
            && window
                .enabled_buttons()
                .contains(winit::window::WindowButtons::MAXIMIZE)
        {
            window.set_maximized(!window.is_maximized());
        }
        return;
    }
    if response.drag_started() && !state.dragging {
        if window.is_maximized() {
            window.set_maximized(false);
        }
        match window.drag_window() {
            Ok(()) => {
                state.dragging = true;
                tracing::debug!("compositor title drag started");
            }
            Err(error) => tracing::debug!(%error, "compositor refused a window move"),
        }
    }
    if response.drag_stopped() {
        state.dragging = false;
    }
}

pub(crate) fn update_nonmodal_window_resize(
    _ctx: &egui::Context,
    window: &Window,
    response: &egui::Response,
    direction: ResizeDirection,
    state: &mut WindowResizeState,
    _aspect: Option<(u32, u32)>,
) {
    // A compositor-driven resize reports its result through WindowEvent::Resized,
    // so the aspect ratio is re-applied there rather than predicted here.
    if response.drag_started() && !state.dragging {
        match window.drag_resize_window(direction) {
            Ok(()) => state.dragging = true,
            Err(error) => tracing::debug!(%error, "compositor refused a window resize"),
        }
    }
    if response.drag_stopped() {
        state.dragging = false;
    }
}
