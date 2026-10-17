//! Keeps the desktop from acting on the shortcuts a remote desktop needs.
//!
//! "Intercept shortcuts" promises that a key the viewer forwards stops here,
//! which the Windows viewer keeps with a low-level keyboard hook. A Linux
//! desktop takes its bindings before the focused window sees them: an X11
//! window manager through passive key grabs, a Wayland compositor before it
//! delivers anything. Each has its own answer, and this module picks the one
//! the window lives on:
//!
//! - Wayland: `zwp_keyboard_shortcuts_inhibit_v1`, the compositor's own
//!   protocol. A window that holds an inhibitor gets its keys while it has the
//!   keyboard, and the compositor's bindings stand down. The compositor scopes
//!   it to focus, may ask the user first (GNOME does, once) and keeps an escape
//!   shortcut of its own.
//! - X11: an active keyboard grab on the viewer window, taken on winit's own
//!   connection so the keys keep arriving through winit, and only while the
//!   window has the X input focus, so taking it moves no focus.
//!
//! Whether to hold the shortcuts back is decided by the shared input gate; this
//! module is only the mechanism.
use wayland_client::backend::ObjectId;
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_registry, wl_seat, wl_surface};
use wayland_client::{Connection, Dispatch, EventQueue, Proxy as _, QueueHandle};
use wayland_protocols::wp::keyboard_shortcuts_inhibit::zv1::client::{
    zwp_keyboard_shortcuts_inhibit_manager_v1 as manager,
    zwp_keyboard_shortcuts_inhibitor_v1 as inhibitor,
};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

/// The desktop's shortcuts, held back or let go for one viewer window.
pub(super) enum Shortcuts {
    Wayland(Inhibitor),
    X11(super::linux_keyboard_grab::KeyboardGrab),
}

impl Shortcuts {
    /// `None` when the desktop offers no way to hold its shortcuts back (a
    /// compositor without the protocol); they then stay with the desktop.
    pub(super) fn new(window: &winit::window::Window) -> Option<Self> {
        match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Wayland(_) => {
                let inhibitor = Inhibitor::new(window);
                if inhibitor.is_none() {
                    tracing::info!(
                        "the compositor does not offer keyboard-shortcuts-inhibit; its shortcuts stay with the desktop"
                    );
                }
                inhibitor.map(Self::Wayland)
            }
            RawDisplayHandle::Xlib(_) => {
                super::linux_keyboard_grab::KeyboardGrab::new(window).map(Self::X11)
            }
            _ => None,
        }
    }

    /// Follows the shared gate. `intercepts` is the shortcut rule (setting,
    /// remote keyboard, no recorder or text field). Called on every frame and
    /// window event; repeating the current state costs nothing.
    pub(super) fn update(
        &mut self,
        gate: super::input_gate::InputGate,
        context: &egui::Context,
        intercepts: bool,
    ) {
        match self {
            // The compositor scopes an inhibitor to the focused surface, so it
            // is kept across focus changes rather than renewed on each one
            // (which would also ask GNOME's permission again).
            Self::Wayland(inhibitor) => {
                inhibitor.set_active(intercepts && gate.holds_unless_unfocused(context))
            }
            // An X11 grab takes the keyboard from every window, so it follows
            // the full gate and is gone the moment focus leaves this window.
            Self::X11(grab) => grab.set_active(intercepts && gate.holds(context)),
        }
    }
}

pub(super) struct Inhibitor {
    connection: Connection,
    queue: EventQueue<State>,
    state: State,
    surface: wl_surface::WlSurface,
    seat: wl_seat::WlSeat,
    manager: manager::ZwpKeyboardShortcutsInhibitManagerV1,
    inhibitor: Option<inhibitor::ZwpKeyboardShortcutsInhibitorV1>,
}

#[derive(Default)]
struct State {
    /// What the compositor last said about the current inhibitor: `None`
    /// until it answers. A compositor that asks the user first answers only
    /// after the user did; one that refuses never answers.
    active: Option<bool>,
    /// The first grant is logged at info, later ones (each focus-in) at debug.
    granted_once: bool,
}

impl Inhibitor {
    fn new(window: &winit::window::Window) -> Option<Self> {
        let display = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Wayland(handle) => handle.display.as_ptr(),
            _ => return None,
        };
        let surface = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Wayland(handle) => handle.surface.as_ptr(),
            _ => return None,
        };
        // Safe because the display belongs to the event loop, which outlives
        // every window and every player (they are dropped on `exiting`), and
        // a foreign-display backend never closes the connection it borrows.
        let backend =
            unsafe { wayland_client::backend::Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);
        // Safe because winit created this surface and the window is still
        // alive. winit's proxy is managed by the same wayland-backend, so the
        // id carries winit's liveness flag: once winit destroys the surface,
        // requests naming it fail instead of touching freed memory.
        let id = unsafe { ObjectId::from_ptr(wl_surface::WlSurface::interface(), surface.cast()) }
            .ok()?;
        let surface = wl_surface::WlSurface::from_id(&connection, id).ok()?;
        let (globals, queue) = registry_queue_init::<State>(&connection).ok()?;
        let handle = queue.handle();
        let manager: manager::ZwpKeyboardShortcutsInhibitManagerV1 =
            globals.bind(&handle, 1..=1, ()).ok()?;
        let seat: wl_seat::WlSeat = match globals.bind(&handle, 1..=1, ()) {
            Ok(seat) => seat,
            Err(_) => {
                manager.destroy();
                let _ = connection.flush();
                return None;
            }
        };
        Some(Self {
            connection,
            queue,
            state: State::default(),
            surface,
            seat,
            manager,
            inhibitor: None,
        })
    }

    fn set_active(&mut self, active: bool) {
        // Answers arrive on this queue whenever winit reads the socket; take
        // them here, where they are only reported.
        let previous = self.state.active;
        let _ = self.queue.dispatch_pending(&mut self.state);
        if self.state.active != previous {
            match self.state.active {
                Some(true) if !self.state.granted_once => {
                    self.state.granted_once = true;
                    tracing::info!("the compositor holds its shortcuts back");
                }
                Some(true) => tracing::debug!("the compositor holds its shortcuts back"),
                // Sent on focus loss, and when the user takes the shortcuts
                // back with the compositor's escape shortcut. The inhibitor is
                // left alone until the gate closes and reopens, so that choice
                // stands.
                Some(false) => tracing::debug!("the compositor restored its own shortcuts"),
                None => {}
            }
        }
        if active == self.inhibitor.is_some() {
            return;
        }
        if active {
            self.state.active = None;
            let handle = self.queue.handle();
            self.inhibitor =
                Some(
                    self.manager
                        .inhibit_shortcuts(&self.surface, &self.seat, &handle, ()),
                );
        } else if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
            self.state.active = None;
        }
        let _ = self.connection.flush();
    }
}

impl Drop for Inhibitor {
    fn drop(&mut self) {
        if let Some(inhibitor) = self.inhibitor.take() {
            inhibitor.destroy();
        }
        self.manager.destroy();
        let _ = self.connection.flush();
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

wayland_client::delegate_noop!(State: ignore wl_seat::WlSeat);
wayland_client::delegate_noop!(State: ignore manager::ZwpKeyboardShortcutsInhibitManagerV1);

impl Dispatch<inhibitor::ZwpKeyboardShortcutsInhibitorV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &inhibitor::ZwpKeyboardShortcutsInhibitorV1,
        event: inhibitor::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            inhibitor::Event::Active => state.active = Some(true),
            inhibitor::Event::Inactive => state.active = Some(false),
            _ => {}
        }
    }
}
