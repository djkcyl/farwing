//! Poll standard game controllers without sharing winit's event loop.
//!
//! SDL's controller mappings include Steam Input's virtual Xbox controller.
//! This backend only samples devices: the viewer's input owner decides when
//! a snapshot may be sent to a remote session.
use crate::protocol::gamepad::GamepadState;
use anyhow::{Context, Result, anyhow, bail};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::ptr::NonNull;
use std::rc::{Rc, Weak};
use std::time::Duration;

pub(crate) const MAX_GAMEPADS: usize = 4;
const INIT_GAMECONTROLLER: u32 = 0x0000_2000;
const QUERY: c_int = -1;
const IGNORE: c_int = 0;
const HINT_OVERRIDE: c_int = 2;
const BACKGROUND_HINT: &CStr = c"SDL_JOYSTICK_ALLOW_BACKGROUND_EVENTS";
// Joystick events and controller events, including Steam-handle updates. Save
// each switch separately: SDL's group query loses mixed enabled/disabled states.
const EVENT_TYPES: [u32; 20] = [
    0x600, 0x601, 0x602, 0x603, 0x604, 0x605, 0x606, 0x607, 0x650, 0x651, 0x652, 0x653, 0x654,
    0x655, 0x656, 0x657, 0x658, 0x659, 0x65a, 0x65b,
];

/// The standard Xbox button bits, independent of SDL's button enum order.
pub(crate) mod buttons {
    pub const DPAD_UP: u16 = 0x0001;
    pub const DPAD_DOWN: u16 = 0x0002;
    pub const DPAD_LEFT: u16 = 0x0004;
    pub const DPAD_RIGHT: u16 = 0x0008;
    pub const START: u16 = 0x0010;
    pub const BACK: u16 = 0x0020;
    pub const LEFT_THUMB: u16 = 0x0040;
    pub const RIGHT_THUMB: u16 = 0x0080;
    pub const LEFT_SHOULDER: u16 = 0x0100;
    pub const RIGHT_SHOULDER: u16 = 0x0200;
    pub const GUIDE: u16 = 0x0400;
    pub const A: u16 = 0x1000;
    pub const B: u16 = 0x2000;
    pub const X: u16 = 0x4000;
    pub const Y: u16 = 0x8000;
}

const BUTTON_MAP: [u16; 15] = [
    buttons::A,
    buttons::B,
    buttons::X,
    buttons::Y,
    buttons::BACK,
    buttons::GUIDE,
    buttons::START,
    buttons::LEFT_THUMB,
    buttons::RIGHT_THUMB,
    buttons::LEFT_SHOULDER,
    buttons::RIGHT_SHOULDER,
    buttons::DPAD_UP,
    buttons::DPAD_DOWN,
    buttons::DPAD_LEFT,
    buttons::DPAD_RIGHT,
];

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Snapshot {
    /// Stable while this physical device remains attached; reused after removal.
    pub index: u8,
    pub instance_id: i32,
    pub name: String,
    pub state: GamepadState,
}

struct Api {
    _library: libloading::Library,
    init_subsystem: unsafe extern "C" fn(u32) -> c_int,
    quit_subsystem: unsafe extern "C" fn(u32),
    get_error: unsafe extern "C" fn() -> *const c_char,
    get_hint: unsafe extern "C" fn(*const c_char) -> *const c_char,
    set_hint: unsafe extern "C" fn(*const c_char, *const c_char, c_int) -> c_int,
    reset_hint: Option<unsafe extern "C" fn(*const c_char) -> c_int>,
    event_state: unsafe extern "C" fn(u32, c_int) -> u8,
    update: unsafe extern "C" fn(),
    count: unsafe extern "C" fn() -> c_int,
    is_controller: unsafe extern "C" fn(c_int) -> c_int,
    device_instance: unsafe extern "C" fn(c_int) -> i32,
    open: unsafe extern "C" fn(c_int) -> *mut c_void,
    close: unsafe extern "C" fn(*mut c_void),
    attached: unsafe extern "C" fn(*mut c_void) -> c_int,
    name: unsafe extern "C" fn(*mut c_void) -> *const c_char,
    joystick: unsafe extern "C" fn(*mut c_void) -> *mut c_void,
    instance: unsafe extern "C" fn(*mut c_void) -> i32,
    axis: unsafe extern "C" fn(*mut c_void, c_int) -> i16,
    button: unsafe extern "C" fn(*mut c_void, c_int) -> u8,
    rumble: Option<unsafe extern "C" fn(*mut c_void, u16, u16, u32) -> c_int>,
    has_rumble: Option<unsafe extern "C" fn(*mut c_void) -> c_int>,
}

impl Api {
    fn load() -> Result<Self> {
        // No bundled SDL or video subsystem is needed. SteamOS supplies SDL2,
        // and SDL2-compatible distributions expose the same soname.
        let library = unsafe { libloading::Library::new("libSDL2-2.0.so.0") }
            .or_else(|_| unsafe { libloading::Library::new("libSDL2.so") })
            .context("SDL2 game controller support is unavailable")?;
        unsafe {
            Ok(Self {
                init_subsystem: *library.get(b"SDL_InitSubSystem\0")?,
                quit_subsystem: *library.get(b"SDL_QuitSubSystem\0")?,
                get_error: *library.get(b"SDL_GetError\0")?,
                get_hint: *library.get(b"SDL_GetHint\0")?,
                set_hint: *library.get(b"SDL_SetHintWithPriority\0")?,
                reset_hint: library.get(b"SDL_ResetHint\0").ok().map(|symbol| *symbol),
                event_state: *library.get(b"SDL_EventState\0")?,
                update: *library.get(b"SDL_GameControllerUpdate\0")?,
                count: *library.get(b"SDL_NumJoysticks\0")?,
                is_controller: *library.get(b"SDL_IsGameController\0")?,
                device_instance: *library.get(b"SDL_JoystickGetDeviceInstanceID\0")?,
                open: *library.get(b"SDL_GameControllerOpen\0")?,
                close: *library.get(b"SDL_GameControllerClose\0")?,
                attached: *library.get(b"SDL_GameControllerGetAttached\0")?,
                name: *library.get(b"SDL_GameControllerName\0")?,
                joystick: *library.get(b"SDL_GameControllerGetJoystick\0")?,
                instance: *library.get(b"SDL_JoystickInstanceID\0")?,
                axis: *library.get(b"SDL_GameControllerGetAxis\0")?,
                button: *library.get(b"SDL_GameControllerGetButton\0")?,
                rumble: library
                    .get(b"SDL_GameControllerRumble\0")
                    .ok()
                    .map(|symbol| *symbol),
                has_rumble: library
                    .get(b"SDL_GameControllerHasRumble\0")
                    .ok()
                    .map(|symbol| *symbol),
                _library: library,
            })
        }
    }

    fn error(&self) -> String {
        let error = unsafe { (self.get_error)() };
        if error.is_null() {
            "unknown SDL2 error".into()
        } else {
            unsafe { CStr::from_ptr(error) }
                .to_string_lossy()
                .into_owned()
        }
    }

    fn stop(&self, handle: NonNull<c_void>) {
        if let Some(rumble) = self.rumble {
            unsafe { rumble(handle.as_ptr(), 0, 0, 0) };
        }
    }
}

thread_local! {
    static SUBSYSTEM: RefCell<Weak<Subsystem>> = const { RefCell::new(Weak::new()) };
}

/// SDL's event switches and hints are global, even when two viewers briefly
/// overlap during focus transfer. Keep one matching initialization lease and
/// restore the original switches only after the final UI-thread reader exits.
struct Subsystem {
    api: Api,
    previous_events: [u8; EVENT_TYPES.len()],
    previous_background_hint: Option<CString>,
    next_owner: Cell<u64>,
    rumble_owners: RefCell<HashMap<i32, u64>>,
}

impl Subsystem {
    fn acquire() -> Result<Rc<Self>> {
        SUBSYSTEM.with(|shared| {
            if let Some(subsystem) = shared.borrow().upgrade() {
                return Ok(subsystem);
            }
            let api = Api::load()?;
            let previous_background_hint = unsafe {
                let value = (api.get_hint)(BACKGROUND_HINT.as_ptr());
                (!value.is_null()).then(|| CStr::from_ptr(value).to_owned())
            };
            if unsafe { (api.init_subsystem)(INIT_GAMECONTROLLER) } < 0 {
                bail!(
                    "Could not initialize SDL2 game controllers: {}",
                    api.error()
                );
            }
            // SDL has no window here, while winit owns the real viewer. Input
            // ownership is enforced by the viewer, rather than SDL focus.
            let previous_events = unsafe {
                (api.set_hint)(BACKGROUND_HINT.as_ptr(), c"1".as_ptr(), HINT_OVERRIDE);
                std::array::from_fn(|index| {
                    let previous = (api.event_state)(EVENT_TYPES[index], QUERY);
                    (api.event_state)(EVENT_TYPES[index], IGNORE);
                    previous
                })
            };
            let subsystem = Rc::new(Self {
                api,
                previous_events,
                previous_background_hint,
                next_owner: Cell::new(1),
                rumble_owners: RefCell::new(HashMap::new()),
            });
            *shared.borrow_mut() = Rc::downgrade(&subsystem);
            Ok(subsystem)
        })
    }

    fn reader_owner(&self) -> u64 {
        let owner = self.next_owner.get();
        self.next_owner
            .set(owner.checked_add(1).expect("SDL reader owner exhausted"));
        owner
    }

    fn stop_owned(&self, device: &Device, owner: u64) {
        let mut owners = self.rumble_owners.borrow_mut();
        if owners.get(&device.instance_id) == Some(&owner) {
            self.api.stop(device.handle);
            owners.remove(&device.instance_id);
        }
    }

    fn close(&self, device: Device, owner: u64) {
        self.stop_owned(&device, owner);
        unsafe { (self.api.close)(device.handle.as_ptr()) };
    }
}

impl Drop for Subsystem {
    fn drop(&mut self) {
        unsafe {
            for (event, state) in EVENT_TYPES.into_iter().zip(self.previous_events) {
                (self.api.event_state)(event, c_int::from(state));
            }
            if let Some(value) = &self.previous_background_hint {
                (self.api.set_hint)(BACKGROUND_HINT.as_ptr(), value.as_ptr(), HINT_OVERRIDE);
            } else if let Some(reset) = self.api.reset_hint {
                reset(BACKGROUND_HINT.as_ptr());
            } else {
                (self.api.set_hint)(BACKGROUND_HINT.as_ptr(), c"0".as_ptr(), HINT_OVERRIDE);
            }
            // GAMECONTROLLER owns its matching JOYSTICK/EVENTS references.
            // Never SDL_Quit(), which would tear down another subsystem's state.
            (self.api.quit_subsystem)(INIT_GAMECONTROLLER);
        }
    }
}

struct Device {
    handle: NonNull<c_void>,
    instance_id: i32,
    name: String,
}

/// One viewer-owned reader. Construct, sample, and drop it on the same thread.
///
/// `poll` calls SDL_GameControllerUpdate, which also detects hotplugged devices.
/// It never pumps or consumes SDL events, nor initializes an SDL video window.
/// The caller must stop/drop this reader when its viewer loses input ownership.
pub(crate) struct Poller {
    subsystem: Rc<Subsystem>,
    owner: u64,
    devices: [Option<Device>; MAX_GAMEPADS],
    _thread: PhantomData<Rc<()>>,
}

impl Poller {
    pub fn new() -> Result<Self> {
        let subsystem = Subsystem::acquire()?;
        let owner = subsystem.reader_owner();
        Ok(Self {
            subsystem,
            owner,
            devices: std::array::from_fn(|_| None),
            _thread: PhantomData,
        })
    }

    /// Current connected devices. Removed indices disappear from this snapshot;
    /// the session layer must send neutral/disconnect before reusing an index.
    pub fn poll(&mut self) -> Result<Vec<Snapshot>> {
        unsafe { (self.subsystem.api.update)() };
        for device in &mut self.devices {
            if device.as_ref().is_some_and(
                |device| unsafe { (self.subsystem.api.attached)(device.handle.as_ptr()) } == 0,
            ) && let Some(device) = device.take()
            {
                self.subsystem.close(device, self.owner);
            }
        }
        self.open_new_devices()?;
        let snapshots = self
            .devices
            .iter()
            .enumerate()
            .filter_map(|(index, device)| {
                let device = device.as_ref()?;
                Some(Snapshot {
                    index: index as u8,
                    instance_id: device.instance_id,
                    name: device.name.clone(),
                    state: self.state(device.handle),
                })
            })
            .collect();
        Ok(snapshots)
    }

    fn open_new_devices(&mut self) -> Result<()> {
        let count = unsafe { (self.subsystem.api.count)() };
        if count < 0 {
            bail!(
                "Could not enumerate SDL2 game controllers: {}",
                self.subsystem.api.error()
            );
        }
        let mut first_error = None;
        for index in 0..count {
            let Some(slot) = self.devices.iter().position(Option::is_none) else {
                break;
            };
            if unsafe { (self.subsystem.api.is_controller)(index) } == 0 {
                continue;
            }
            let instance = unsafe { (self.subsystem.api.device_instance)(index) };
            if instance < 0
                || self
                    .devices
                    .iter()
                    .flatten()
                    .any(|device| device.instance_id == instance)
            {
                continue;
            }
            let handle = unsafe { (self.subsystem.api.open)(index) };
            let Some(handle) = NonNull::new(handle) else {
                first_error.get_or_insert_with(|| self.subsystem.api.error());
                continue;
            };
            // Device indices can shift during hotplug; use the opened handle's
            // instance ID rather than trusting the earlier enumeration index.
            let joystick = unsafe { (self.subsystem.api.joystick)(handle.as_ptr()) };
            let instance_id = if joystick.is_null() {
                -1
            } else {
                unsafe { (self.subsystem.api.instance)(joystick) }
            };
            if instance_id < 0
                || self
                    .devices
                    .iter()
                    .flatten()
                    .any(|device| device.instance_id == instance_id)
            {
                unsafe { (self.subsystem.api.close)(handle.as_ptr()) };
                continue;
            }
            let name = unsafe { (self.subsystem.api.name)(handle.as_ptr()) };
            let name = if name.is_null() {
                "Game controller".to_owned()
            } else {
                unsafe { CStr::from_ptr(name) }
                    .to_string_lossy()
                    .into_owned()
            };
            self.devices[slot] = Some(Device {
                handle,
                instance_id,
                name,
            });
        }
        if self.devices.iter().all(Option::is_none)
            && let Some(error) = first_error
        {
            bail!("SDL2 detected game controllers but could not open them: {error}");
        }
        Ok(())
    }

    fn state(&self, handle: NonNull<c_void>) -> GamepadState {
        let axes = std::array::from_fn(|index| unsafe {
            (self.subsystem.api.axis)(handle.as_ptr(), index as c_int)
        });
        let buttons = std::array::from_fn(|index| unsafe {
            (self.subsystem.api.button)(handle.as_ptr(), index as c_int) != 0
        });
        state_from_sdl(axes, buttons)
    }

    pub fn has_rumble(&self, index: u8) -> bool {
        self.devices
            .get(usize::from(index))
            .and_then(Option::as_ref)
            .is_some_and(|device| {
                self.subsystem.api.rumble.is_some()
                    && self
                        .subsystem
                        .api
                        .has_rumble
                        .is_none_or(|has_rumble| unsafe { has_rumble(device.handle.as_ptr()) != 0 })
            })
    }

    /// Low-frequency is the left motor, high-frequency the right. A zero
    /// strength stops both motors; effects are bounded even if the peer vanishes.
    pub fn rumble(
        &mut self,
        index: u8,
        left_motor: u16,
        right_motor: u16,
        duration: Duration,
    ) -> Result<()> {
        let device = self
            .devices
            .get(usize::from(index))
            .and_then(Option::as_ref)
            .ok_or_else(|| anyhow!("Game controller {index} is not connected"))?;
        let rumble = self
            .subsystem
            .api
            .rumble
            .ok_or_else(|| anyhow!("This SDL2 version does not support controller rumble"))?;
        let duration_ms = duration.as_millis().min(5_000) as u32;
        let (left_motor, right_motor) = if duration_ms == 0 {
            (0, 0)
        } else {
            (left_motor, right_motor)
        };
        // A focus transfer may leave two readers holding the same SDL device.
        // A reader that never started (or no longer owns) its effect must not
        // cancel the current owner's motors when it receives zero feedback.
        if left_motor == 0
            && right_motor == 0
            && self
                .subsystem
                .rumble_owners
                .borrow()
                .get(&device.instance_id)
                != Some(&self.owner)
        {
            return Ok(());
        }
        if unsafe { rumble(device.handle.as_ptr(), left_motor, right_motor, duration_ms) } < 0 {
            bail!(
                "Game controller {index} rumble is unavailable: {}",
                self.subsystem.api.error()
            );
        }
        let mut owners = self.subsystem.rumble_owners.borrow_mut();
        if left_motor == 0 && right_motor == 0 {
            owners.remove(&device.instance_id);
        } else {
            owners.insert(device.instance_id, self.owner);
        }
        Ok(())
    }

    pub fn stop_rumble(&mut self) {
        for device in self.devices.iter().flatten() {
            self.subsystem.stop_owned(device, self.owner);
        }
    }
}

impl Drop for Poller {
    fn drop(&mut self) {
        for device in &mut self.devices {
            if let Some(device) = device.take() {
                self.subsystem.close(device, self.owner);
            }
        }
    }
}

fn trigger(value: i16) -> u8 {
    ((i32::from(value).max(0) * 255 + 16_383) / 32_767) as u8
}

fn state_from_sdl(axes: [i16; 6], pressed: [bool; 15]) -> GamepadState {
    GamepadState {
        buttons: pressed
            .into_iter()
            .zip(BUTTON_MAP)
            .filter_map(|(pressed, mask)| pressed.then_some(mask))
            .fold(0, |state, mask| state | mask),
        left_trigger: trigger(axes[4]),
        right_trigger: trigger(axes[5]),
        left_thumb_x: axes[0],
        left_thumb_y: axes[1].saturating_neg(),
        right_thumb_x: axes[2],
        right_thumb_y: axes[3].saturating_neg(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_buttons_keep_their_xbox_bits() {
        for (index, expected) in BUTTON_MAP.into_iter().enumerate() {
            let mut pressed = [false; 15];
            pressed[index] = true;
            assert_eq!(state_from_sdl([0; 6], pressed).buttons, expected);
        }
        assert_eq!(state_from_sdl([0; 6], [true; 15]).buttons, 0xf7ff);
    }

    #[test]
    fn neutral_and_axis_extremes_use_xbox_coordinates() {
        assert_eq!(state_from_sdl([0; 6], [false; 15]), GamepadState::default());
        let state = state_from_sdl(
            [i16::MIN, i16::MIN, i16::MAX, i16::MAX, i16::MIN, i16::MAX],
            [false; 15],
        );
        assert_eq!(state.left_thumb_x, i16::MIN);
        assert_eq!(state.left_thumb_y, i16::MAX);
        assert_eq!(state.right_thumb_x, i16::MAX);
        assert_eq!(state.right_thumb_y, -i16::MAX);
        assert_eq!(state.left_trigger, 0);
        assert_eq!(state.right_trigger, u8::MAX);
    }

    #[test]
    fn trigger_conversion_is_monotonic_and_clamps_negative_noise() {
        assert_eq!(trigger(-1), 0);
        assert_eq!(trigger(0), 0);
        assert_eq!(trigger(16_384), 128);
        assert_eq!(trigger(i16::MAX), 255);
        let mut previous = 0;
        for value in 0..=i16::MAX {
            let current = trigger(value);
            assert!(current >= previous);
            previous = current;
        }
    }
}
