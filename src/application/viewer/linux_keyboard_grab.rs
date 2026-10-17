//! An active X11 keyboard grab, the X11 counterpart of the Windows keyboard
//! hook: while the viewer owns input, the window manager's passive key grabs
//! (Alt+Tab, Super, its own bindings) no longer see the keys.
//!
//! The grab is made on winit's own Xlib connection with `owner_events`, so the
//! server keeps reporting keys to winit's window exactly as before; a grab from
//! a second connection would send them to that connection instead. It is only
//! taken while the viewer window holds the X input focus, so it moves no focus.
//! The server reports a grab on the focused window with core FocusOut/FocusIn
//! events only; winit follows XInput2 focus, which stays put, so the window is
//! not told it lost the focus (the Xvfb test below checks exactly that).
//!
//! A grab blocks every other window's keyboard, so it is let go as soon as the
//! gate closes, on focus loss and when the player goes away; the server also
//! drops it by itself when the window is unmapped or the connection closes.
use std::ffi::{c_int, c_ulong, c_void};
use std::time::{Duration, Instant};
use winit::raw_window_handle::{
    HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle,
};

const CURRENT_TIME: c_ulong = 0;
const GRAB_MODE_ASYNC: c_int = 1;
const TRUE: c_int = 1;
/// How long a refused grab waits before the next attempt.
const RETRY: Duration = Duration::from_millis(500);
/// The shortest time between letting the keyboard go and taking it again. If
/// some X server did report the grab as a focus change after all, a release
/// on that "focus loss" and the grab on the next focus-in would chase each
/// other; this bounds that to a slow, visible flicker instead of a spin.
const REGRAB: Duration = Duration::from_millis(250);

type Display = c_void;

struct Xlib {
    grab_keyboard:
        unsafe extern "C" fn(*mut Display, c_ulong, c_int, c_int, c_int, c_ulong) -> c_int,
    ungrab_keyboard: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
    get_input_focus: unsafe extern "C" fn(*mut Display, *mut c_ulong, *mut c_int) -> c_int,
    flush: unsafe extern "C" fn(*mut Display) -> c_int,
    // Keeps the function pointers above valid. winit has the same library
    // loaded for the same display, so this only adds a reference.
    _library: libloading::Library,
}

impl Xlib {
    fn open() -> Option<Self> {
        // Safe because libX11 runs no code of ours on load and the symbols
        // are declared with their C prototypes.
        unsafe {
            let library = libloading::Library::new("libX11.so.6").ok()?;
            Some(Self {
                grab_keyboard: *library.get(b"XGrabKeyboard\0").ok()?,
                ungrab_keyboard: *library.get(b"XUngrabKeyboard\0").ok()?,
                get_input_focus: *library.get(b"XGetInputFocus\0").ok()?,
                flush: *library.get(b"XFlush\0").ok()?,
                _library: library,
            })
        }
    }
}

/// Why the last attempt did not grab, so each reason is logged once.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Refusal {
    /// The X input focus is on another window (or on the frame or root).
    Unfocused(c_ulong),
    /// `XGrabKeyboard` status: AlreadyGrabbed, GrabInvalidTime,
    /// GrabNotViewable or GrabFrozen.
    Status(c_int),
}

pub(super) struct KeyboardGrab {
    xlib: Xlib,
    display: *mut Display,
    window: c_ulong,
    grabbed: bool,
    /// No attempt before this instant: after a refusal or a release.
    retry_at: Option<Instant>,
    retry: Duration,
    regrab: Duration,
    refused: Option<Refusal>,
}

impl KeyboardGrab {
    /// `None` when the window is not an Xlib window or libX11 cannot be found.
    pub(super) fn new(window: &winit::window::Window) -> Option<Self> {
        let display = match window.display_handle().ok()?.as_raw() {
            RawDisplayHandle::Xlib(handle) => handle.display?.as_ptr(),
            _ => return None,
        };
        let window = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(handle) => handle.window,
            _ => return None,
        };
        let grab = Self::from_raw(display, window);
        if grab.is_none() {
            tracing::warn!("libX11 is missing XGrabKeyboard; window-manager shortcuts stay local");
        }
        grab
    }

    /// `display` must stay open for as long as this value lives; winit's
    /// display belongs to the event loop, which outlives every player.
    fn from_raw(display: *mut Display, window: c_ulong) -> Option<Self> {
        Some(Self {
            xlib: Xlib::open()?,
            display,
            window,
            grabbed: false,
            retry_at: None,
            retry: RETRY,
            regrab: REGRAB,
            refused: None,
        })
    }

    /// Grabs or releases the keyboard. Repeating the current state costs no
    /// X request; a refused grab is retried at most every [`RETRY`].
    pub(super) fn set_active(&mut self, active: bool) {
        if !active {
            self.release();
            return;
        }
        if self.grabbed
            || self
                .retry_at
                .is_some_and(|retry_at| Instant::now() < retry_at)
        {
            return;
        }
        match self.grab() {
            Ok(()) => {
                self.grabbed = true;
                self.retry_at = None;
                self.refused = None;
                tracing::debug!(window = self.window, "keyboard grabbed for the remote");
            }
            Err(refusal) => {
                self.retry_at = Some(Instant::now() + self.retry);
                if self.refused != Some(refusal) {
                    self.refused = Some(refusal);
                    tracing::info!(
                        ?refusal,
                        "the keyboard could not be grabbed; window-manager shortcuts stay local for now"
                    );
                }
            }
        }
    }

    fn focus(&self) -> c_ulong {
        let mut focus: c_ulong = 0;
        let mut revert: c_int = 0;
        // Safe because the display is open (see `from_raw`) and both
        // out-pointers are valid for the call.
        unsafe { (self.xlib.get_input_focus)(self.display, &mut focus, &mut revert) };
        focus
    }

    fn grab(&self) -> Result<(), Refusal> {
        let focus = self.focus();
        if focus != self.window {
            return Err(Refusal::Unfocused(focus));
        }
        // Safe as above; the window is winit's and still mapped while it has
        // the focus.
        let status = unsafe {
            (self.xlib.grab_keyboard)(
                self.display,
                self.window,
                TRUE,
                GRAB_MODE_ASYNC,
                GRAB_MODE_ASYNC,
                CURRENT_TIME,
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(Refusal::Status(status))
        }
    }

    fn release(&mut self) {
        if !self.grabbed {
            // A refusal is forgotten once the gate closes; the next opening
            // tries at once and logs its own outcome.
            if self.refused.take().is_some() {
                self.retry_at = None;
            }
            return;
        }
        self.grabbed = false;
        self.retry_at = Some(Instant::now() + self.regrab);
        // Safe because the display is open (see `from_raw`). Releasing a grab
        // the server already dropped is a no-op.
        unsafe {
            (self.xlib.ungrab_keyboard)(self.display, CURRENT_TIME);
            (self.xlib.flush)(self.display);
        }
        tracing::debug!(window = self.window, "keyboard released");
    }
}

impl Drop for KeyboardGrab {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    //! Run against a private X server, never the desktop's:
    //! `Xvfb :98 & DISPLAY=:98 cargo test --lib linux_keyboard_grab -- --ignored --test-threads=1`
    use super::*;
    use std::ffi::{c_char, c_uint};
    use x11rb::connection::Connection as _;
    use x11rb::protocol::xproto::{ConnectionExt as _, GrabMode, GrabStatus};

    const REVERT_TO_PARENT: c_int = 2;
    const POINTER_ROOT: c_ulong = 1;

    fn private_display() -> String {
        let name = std::env::var("DISPLAY").unwrap();
        assert!(
            name != ":0" && name != ":1",
            "point DISPLAY at a private Xvfb, not the desktop"
        );
        name
    }

    /// The Xlib calls the test needs to stand in for winit.
    struct Client {
        open_display: unsafe extern "C" fn(*const c_char) -> *mut Display,
        close_display: unsafe extern "C" fn(*mut Display) -> c_int,
        default_root_window: unsafe extern "C" fn(*mut Display) -> c_ulong,
        create_simple_window: unsafe extern "C" fn(
            *mut Display,
            c_ulong,
            c_int,
            c_int,
            c_uint,
            c_uint,
            c_uint,
            c_ulong,
            c_ulong,
        ) -> c_ulong,
        map_window: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
        unmap_window: unsafe extern "C" fn(*mut Display, c_ulong) -> c_int,
        set_input_focus: unsafe extern "C" fn(*mut Display, c_ulong, c_int, c_ulong) -> c_int,
        sync: unsafe extern "C" fn(*mut Display, c_int) -> c_int,
        _library: libloading::Library,
    }

    impl Client {
        fn open() -> Self {
            unsafe {
                let library = libloading::Library::new("libX11.so.6").unwrap();
                Self {
                    open_display: *library.get(b"XOpenDisplay\0").unwrap(),
                    close_display: *library.get(b"XCloseDisplay\0").unwrap(),
                    default_root_window: *library.get(b"XDefaultRootWindow\0").unwrap(),
                    create_simple_window: *library.get(b"XCreateSimpleWindow\0").unwrap(),
                    map_window: *library.get(b"XMapWindow\0").unwrap(),
                    unmap_window: *library.get(b"XUnmapWindow\0").unwrap(),
                    set_input_focus: *library.get(b"XSetInputFocus\0").unwrap(),
                    sync: *library.get(b"XSync\0").unwrap(),
                    _library: library,
                }
            }
        }
    }

    /// Whether some client holds the keyboard, asked from a second connection
    /// the way another program (or the window manager) would find out.
    fn keyboard_taken(other: &x11rb::rust_connection::RustConnection, root: u32) -> bool {
        let reply = other
            .grab_keyboard(
                false,
                root,
                x11rb::CURRENT_TIME,
                GrabMode::ASYNC,
                GrabMode::ASYNC,
            )
            .unwrap()
            .reply()
            .unwrap();
        if reply.status == GrabStatus::SUCCESS {
            other.ungrab_keyboard(x11rb::CURRENT_TIME).unwrap();
            other.flush().unwrap();
            false
        } else {
            assert_eq!(reply.status, GrabStatus::ALREADY_GRABBED);
            true
        }
    }

    #[test]
    #[ignore = "needs a private X server: DISPLAY=:98 with Xvfb :98 running"]
    fn grab_follows_the_gate_and_the_focus() {
        let name = private_display();
        let client = Client::open();
        let (other, screen) = x11rb::connect(Some(&name)).unwrap();
        let other_root = other.setup().roots[screen].root;
        unsafe {
            let display = (client.open_display)(std::ptr::null());
            assert!(!display.is_null());
            let root = (client.default_root_window)(display);
            let window = (client.create_simple_window)(display, root, 0, 0, 64, 64, 0, 0, 0);
            (client.map_window)(display, window);
            (client.sync)(display, 0);
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);

            let mut grab = KeyboardGrab::from_raw(display, window).unwrap();
            grab.retry = Duration::ZERO;
            grab.regrab = Duration::ZERO;

            // The gate closed: nothing is held.
            grab.set_active(false);
            assert!(!keyboard_taken(&other, other_root));

            // The gate open on the focused window: the keyboard is taken, and
            // asking again sends nothing new.
            grab.set_active(true);
            assert!(grab.grabbed);
            assert!(keyboard_taken(&other, other_root));
            grab.set_active(true);
            assert!(keyboard_taken(&other, other_root));

            // The gate closes: released at once.
            grab.set_active(false);
            assert!(!grab.grabbed);
            assert!(!keyboard_taken(&other, other_root));

            // Focus elsewhere: the grab is refused rather than moving focus.
            (client.set_input_focus)(display, POINTER_ROOT, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            grab.set_active(true);
            assert!(!grab.grabbed);
            assert!(matches!(grab.refused, Some(Refusal::Unfocused(_))));
            assert!(!keyboard_taken(&other, other_root));

            // Another client holding the keyboard: refused, retried later.
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            other
                .grab_keyboard(
                    false,
                    other_root,
                    x11rb::CURRENT_TIME,
                    GrabMode::ASYNC,
                    GrabMode::ASYNC,
                )
                .unwrap()
                .reply()
                .unwrap();
            grab.set_active(true);
            assert!(!grab.grabbed);
            assert_eq!(grab.refused, Some(Refusal::Status(1)));
            other.ungrab_keyboard(x11rb::CURRENT_TIME).unwrap();
            other.flush().unwrap();
            grab.set_active(true);
            assert!(grab.grabbed);
            assert!(keyboard_taken(&other, other_root));

            // A refusal waits for its retry time instead of asking each frame.
            grab.set_active(false);
            grab.retry = Duration::from_secs(60);
            (client.set_input_focus)(display, POINTER_ROOT, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            grab.set_active(true);
            assert!(grab.refused.is_some());
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);
            grab.set_active(true);
            assert!(!grab.grabbed);
            // Closing the gate forgets the refusal; the next opening tries.
            grab.set_active(false);
            grab.set_active(true);
            assert!(grab.grabbed);
            grab.retry = Duration::ZERO;

            // Unmapping the window drops the grab on the server's side; the
            // next release is then a harmless no-op.
            (client.unmap_window)(display, window);
            (client.sync)(display, 0);
            assert!(!keyboard_taken(&other, other_root));
            grab.set_active(false);
            (client.map_window)(display, window);
            (client.sync)(display, 0);
            (client.set_input_focus)(display, window, REVERT_TO_PARENT, CURRENT_TIME);
            (client.sync)(display, 0);

            // Dropping the player releases a held grab.
            grab.set_active(true);
            assert!(keyboard_taken(&other, other_root));
            drop(grab);
            assert!(!keyboard_taken(&other, other_root));

            (client.close_display)(display);
        }
    }

    /// What the winit test saw.
    #[derive(Default)]
    struct Seen {
        /// (focused, step) for each `Focused` event.
        focus: Vec<(bool, u32)>,
        /// (key, pressed, synthetic, grabbed at the time).
        keys: Vec<(winit::keyboard::PhysicalKey, bool, bool, bool)>,
        taken_while_grabbed: Option<bool>,
        taken_after_release: Option<bool>,
    }

    struct App {
        other: x11rb::rust_connection::RustConnection,
        root: u32,
        window: Option<winit::window::Window>,
        grab: Option<KeyboardGrab>,
        step: u32,
        step_at: Instant,
        seen: Seen,
    }

    impl App {
        fn next(&mut self) {
            self.step += 1;
            self.step_at = Instant::now();
        }

        fn type_key(&self, keycode: u8) {
            use x11rb::protocol::xtest::ConnectionExt as _;
            for kind in [2, 3] {
                self.other
                    .xtest_fake_input(kind, keycode, 0, x11rb::NONE, 0, 0, 0)
                    .unwrap();
            }
            self.other.flush().unwrap();
        }
    }

    impl winit::application::ApplicationHandler for App {
        fn resumed(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            if self.window.is_none() {
                let attributes = winit::window::Window::default_attributes()
                    .with_inner_size(winit::dpi::PhysicalSize::new(200, 200));
                self.window = Some(event_loop.create_window(attributes).unwrap());
                self.next();
            }
        }

        fn window_event(
            &mut self,
            _: &winit::event_loop::ActiveEventLoop,
            _: winit::window::WindowId,
            event: winit::event::WindowEvent,
        ) {
            match event {
                winit::event::WindowEvent::Focused(focused) => {
                    self.seen.focus.push((focused, self.step));
                    // As the presenter does: a real focus loss ends the grab.
                    if !focused && let Some(grab) = self.grab.as_mut() {
                        grab.set_active(false);
                    }
                    if focused && self.step == 2 {
                        let window = self.window.as_ref().unwrap();
                        let mut grab = KeyboardGrab::new(window).unwrap();
                        grab.set_active(true);
                        assert!(grab.grabbed, "{:?}", grab.refused);
                        self.grab = Some(grab);
                        self.next();
                    }
                }
                winit::event::WindowEvent::KeyboardInput {
                    event,
                    is_synthetic,
                    ..
                } => {
                    let grabbed = self.grab.as_ref().is_some_and(|grab| grab.grabbed);
                    self.seen.keys.push((
                        event.physical_key,
                        event.state == winit::event::ElementState::Pressed,
                        is_synthetic,
                        grabbed,
                    ));
                }
                _ => {}
            }
        }

        fn about_to_wait(&mut self, event_loop: &winit::event_loop::ActiveEventLoop) {
            use x11rb::protocol::xproto::{GrabMode, InputFocus, ModMask};
            let waited = self.step_at.elapsed();
            let settle = Duration::from_millis(300);
            let handle = self
                .window
                .as_ref()
                .map(|window| window.window_handle().unwrap().as_raw());
            let window = || match handle {
                Some(RawWindowHandle::Xlib(handle)) => handle.window as u32,
                _ => unreachable!(),
            };
            match self.step {
                1 if waited > settle => {
                    self.other
                        .set_input_focus(InputFocus::PARENT, window(), x11rb::CURRENT_TIME)
                        .unwrap();
                    self.other.flush().unwrap();
                    self.next();
                }
                3 if waited > settle => {
                    self.seen.taken_while_grabbed = Some(keyboard_taken(&self.other, self.root));
                    // A window-manager binding on A, as Alt+Tab or Super are.
                    self.other
                        .grab_key(
                            false,
                            self.root,
                            ModMask::ANY,
                            38,
                            GrabMode::ASYNC,
                            GrabMode::ASYNC,
                        )
                        .unwrap()
                        .check()
                        .unwrap();
                    self.type_key(38);
                    self.next();
                }
                // Another window takes the focus, as a click elsewhere would.
                4 if waited > settle => {
                    self.other
                        .set_input_focus(
                            InputFocus::POINTER_ROOT,
                            InputFocus::POINTER_ROOT,
                            x11rb::CURRENT_TIME,
                        )
                        .unwrap();
                    self.other.flush().unwrap();
                    self.next();
                }
                5 if waited > settle => {
                    self.seen.taken_after_release = Some(keyboard_taken(&self.other, self.root));
                    self.other
                        .set_input_focus(InputFocus::PARENT, window(), x11rb::CURRENT_TIME)
                        .unwrap();
                    self.other.flush().unwrap();
                    self.next();
                }
                // Grab again with the focus back, then let go while focused, as
                // when the gate closes (the release shortcut, a menu).
                6 if waited > settle => {
                    let grab = self.grab.as_mut().unwrap();
                    grab.set_active(true);
                    assert!(grab.grabbed, "{:?}", grab.refused);
                    self.next();
                }
                7 if waited > settle => {
                    self.grab.as_mut().unwrap().set_active(false);
                    self.next();
                }
                8 if waited > settle => {
                    self.type_key(38);
                    self.type_key(56);
                    self.next();
                }
                9 if waited > settle => event_loop.exit(),
                _ if waited > Duration::from_secs(5) => panic!("stuck at step {}", self.step),
                _ => {}
            }
            event_loop.set_control_flow(winit::event_loop::ControlFlow::WaitUntil(
                Instant::now() + Duration::from_millis(20),
            ));
        }

        fn exiting(&mut self, _: &winit::event_loop::ActiveEventLoop) {
            self.grab = None;
            self.window = None;
        }
    }

    /// The grab must not cost winit its keys or its focus, and must keep a
    /// window-manager binding from taking a key.
    #[test]
    #[ignore = "needs a private X server: DISPLAY=:98 with Xvfb :98 running"]
    fn winit_keeps_its_keys_and_focus_under_the_grab() {
        use winit::keyboard::{KeyCode, PhysicalKey};
        use winit::platform::x11::EventLoopBuilderExtX11 as _;
        let name = private_display();
        let (other, screen) = x11rb::connect(Some(&name)).unwrap();
        let root = other.setup().roots[screen].root;
        let event_loop = winit::event_loop::EventLoop::builder()
            .with_x11()
            .with_any_thread(true)
            .build()
            .unwrap();
        let mut app = App {
            other,
            root,
            window: None,
            grab: None,
            step: 0,
            step_at: Instant::now(),
            seen: Seen::default(),
        };
        event_loop.run_app(&mut app).unwrap();
        let seen = app.seen;
        eprintln!("focus: {:?}", seen.focus);
        eprintln!("keys: {:?}", seen.keys);
        assert_eq!(seen.taken_while_grabbed, Some(true));
        assert_eq!(seen.taken_after_release, Some(false));
        let real = |code, grabbed| {
            seen.keys
                .iter()
                .filter(|(key, _, synthetic, during)| {
                    *key == PhysicalKey::Code(code) && !synthetic && *during == grabbed
                })
                .map(|(_, pressed, _, _)| *pressed)
                .collect::<Vec<_>>()
        };
        // Under the grab the bound key reaches winit; without it the binding
        // takes it, as the window manager would; other keys arrive either way.
        assert_eq!(real(KeyCode::KeyA, true), vec![true, false]);
        assert_eq!(real(KeyCode::KeyA, false), Vec::<bool>::new());
        assert_eq!(real(KeyCode::KeyB, false), vec![true, false]);
        // Taking or releasing the grab moves no focus winit reports (nothing
        // at steps 3, 4, 7 and 8), so the presenter never releases input on
        // its own grab. A real focus loss is reported under the grab (step 5)
        // and released it; the binding's grab at step 9 is reported, as it
        // always was.
        let at = |step| {
            seen.focus
                .iter()
                .filter(|(_, at)| *at == step)
                .map(|(focused, _)| *focused)
                .collect::<Vec<_>>()
        };
        assert_eq!(at(3), Vec::<bool>::new(), "{:?}", seen.focus);
        assert_eq!(at(4), Vec::<bool>::new(), "{:?}", seen.focus);
        assert_eq!(at(5), vec![false], "{:?}", seen.focus);
        assert_eq!(at(6), vec![true], "{:?}", seen.focus);
        assert_eq!(at(7), Vec::<bool>::new(), "{:?}", seen.focus);
        assert_eq!(at(8), Vec::<bool>::new(), "{:?}", seen.focus);
        assert!(at(9).contains(&false), "{:?}", seen.focus);
    }
}
