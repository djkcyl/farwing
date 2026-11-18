//! Typed remote input on UU CONTROL, independent of GUI frame cadence.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::features::host::input::wire::Point;
use crate::protocol::gamepad::{GamepadEvent, GamepadState, GamepadVibration, MAX_GAMEPADS};
use crate::protocol::keyboard::KeyboardKey;
use anyhow::{Result, bail};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub(crate) mod polling;
mod recovery;
pub(crate) mod touch;
pub(crate) use recovery::{InputRecovery, InputWork};
pub(crate) use touch::{Phase, Spot};

const MAX_EVENTS: usize = 512;
const MOTION_JITTER: Duration = Duration::from_micros(100);
pub(crate) const BUTTONS: [u32; 5] = [1, 2, 16, 32, 64];

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) enum MouseThrottleRate {
    #[serde(rename = "125")]
    Hz125,
    #[serde(rename = "500")]
    Hz500,
    #[default]
    #[serde(rename = "1000")]
    Hz1000,
}

impl MouseThrottleRate {
    pub(crate) const ALL: [Self; 3] = [Self::Hz125, Self::Hz500, Self::Hz1000];

    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Hz125 => "125 Hz",
            Self::Hz500 => "500 Hz",
            Self::Hz1000 => "1000 Hz",
        }
    }

    fn interval(self) -> Duration {
        Duration::from_millis(match self {
            Self::Hz125 => 8,
            Self::Hz500 => 2,
            Self::Hz1000 => 1,
        })
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum MouseMode {
    #[default]
    View,
    Smart,
    Local,
    Remote,
}

#[derive(Clone, Debug)]
pub(crate) enum InputEvent {
    Correction {
        x: i32,
        y: i32,
        lease: u64,
        expires: std::time::Instant,
    },
    AssistButton {
        down: bool,
        lease: u64,
        expires: std::time::Instant,
    },
    Absolute {
        screen: i32,
        x: f64,
        y: f64,
    },
    Relative {
        x: i32,
        y: i32,
    },
    Button {
        button: u32,
        down: bool,
    },
    Wheel {
        delta: i32,
        horizontal: bool,
    },
    Key {
        key: KeyboardKey,
        down: bool,
        lock: Option<u8>,
        interrupt: bool,
    },
    Gamepad(GamepadEvent),
    /// Native touch, the only binary inhabitant: `kind` plus a full
    /// contact-set snapshot per the host wire, never JSON.
    Touch {
        kind: u8,
        points: Vec<Point>,
    },
    Heartbeat,
}

impl InputEvent {
    fn movement(&self) -> bool {
        matches!(
            self,
            Self::Absolute { .. } | Self::Relative { .. } | Self::Correction { .. }
        )
    }
    fn input_boundary(&self) -> bool {
        matches!(
            self,
            Self::Button { .. }
                | Self::Key { .. }
                | Self::Wheel { .. }
                | Self::AssistButton { .. }
                | Self::Gamepad(_)
                | Self::Touch { .. }
        )
    }
    pub fn send_timeout(&self) -> std::time::Duration {
        match self {
            Self::Correction { expires, .. }
            | Self::AssistButton {
                expires,
                down: true,
                ..
            } => expires
                .saturating_duration_since(std::time::Instant::now())
                .min(std::time::Duration::from_secs(1)),
            _ => std::time::Duration::from_secs(1),
        }
    }
    pub(crate) fn release(&self) -> bool {
        matches!(
            self,
            Self::Button { down: false, .. }
                | Self::Key { down: false, .. }
                | Self::AssistButton { down: false, .. }
                | Self::Touch { kind: 3 | 4, .. }
        ) || matches!(self, Self::Gamepad(event) if event.release())
    }
    pub(crate) fn encode(&self) -> Vec<u8> {
        // The host dispatches on the first byte: '{' opens the JSON dialect,
        // anything else is a protobuf control envelope. Touch is the only
        // inhabitant that takes the binary path.
        if let Self::Touch { kind, points } = self {
            return crate::features::stream_control::encode_touch_input(
                crate::features::host::input::wire::encode_touch(*kind, points),
            );
        }
        let value = match *self {
            Self::Absolute { screen, x, y } => serde_json::json!({
                "action":"mouse_move_absolute", "screen_id":screen, "abs_x":x, "abs_y":y }),
            Self::Relative { x, y } | Self::Correction { x, y, .. } => serde_json::json!({
                "action":"mouse_move_relative", "delta_x":x, "delta_y":y, "mousetype":2 }),
            Self::Button { button, down } => serde_json::json!({
                "action":if down { "mouse_press" } else { "mouse_release" }, "button":button }),
            Self::AssistButton { down, .. } => {
                serde_json::json!({"action":if down {"mouse_press"}else{"mouse_release"},"button":1})
            }
            Self::Wheel { delta, horizontal } => serde_json::json!({
                "action":"mouse_scroll", "delta_x":if horizontal {delta} else {0},
                "delta_y":if horizontal {0} else {delta} }),
            Self::Key {
                key,
                down,
                lock,
                interrupt,
            } => key.encode(down, lock, interrupt),
            Self::Gamepad(event) => event.json(),
            Self::Heartbeat => serde_json::json!({"heartbeat":"kbd_heartbeat"}),
            Self::Touch { .. } => unreachable!("touch encodes above"),
        };
        serde_json::to_vec(&value).expect("finite mouse events serialize")
    }
}

#[derive(Clone, Debug)]
pub(crate) struct QueuedInputEvent {
    pub epoch: u64,
    pub event: InputEvent,
}

pub(crate) struct CorrectionBasis {
    pub physical: [i64; 2],
    pub submitted_corrections: Option<[i64; 2]>,
}

type AssistOwner = (u64, Arc<dyn Fn() -> bool + Send + Sync>);

// Only the Linux viewer samples controllers so far (SDL2); the Windows
// controller has no collector yet, see README's 功能矩阵.
#[cfg_attr(windows, allow(dead_code))]
struct GamepadFeedback {
    owner: u64,
    epoch: u64,
    // UU sends strength changes, not an effect duration or keepalive.
    motors: (u16, u16),
}

#[derive(Default)]
struct State {
    assists: BTreeMap<u64, AssistOwner>,
    assist_button_owner: Option<u64>,
    assist_down: bool,
    physical_motion: [i64; 2],
    submitted_motion: [i64; 2],
    submitted_corrections: [i64; 2],
    in_flight_motion: bool,
    ready: bool,
    stopping: bool,
    mode: MouseMode,
    relative: bool,
    /// Set once a window failed to take the pointer. Relative motion is only
    /// meaningful while this client owns the pointer exclusively; without that
    /// grab the local pointer keeps moving on its own and the deltas drift away
    /// from wherever the remote one ended up. Stored as the refusal so the
    /// derived `Default` starts out permitting it.
    relative_denied: bool,
    owner: Option<u64>,
    drag_hold: Option<(u64, u64)>,
    drag_serial: u64,
    held: [bool; 5],
    // Marked before handing DOWN to transport. A concurrent stop must release
    // even an in-flight press, not just messages whose send() already returned.
    remote_held: [bool; 5],
    keys: BTreeMap<u16, Option<u8>>,
    remote_keys: BTreeMap<KeyboardKey, Option<u8>>,
    gamepads: BTreeMap<u8, GamepadState>,
    // Marked before handing CONNECT to transport, just like remote key presses.
    remote_gamepads: BTreeSet<u8>,
    confirmed_gamepads: BTreeSet<u8>,
    gamepad_feedback: BTreeMap<u8, GamepadFeedback>,
    // Transport acceptance of UP is not acknowledgement of host injection.
    // Keep keys retired at an ownership boundary for session reconciliation.
    release_checkpoint: BTreeSet<KeyboardKey>,
    keyboard_generation: u64,
    activation_generation: u64,
    waiting_for_neutral: bool,
    keyboard_platform: i32,
    accept_host_input: bool,
    queue: VecDeque<InputEvent>,
    touch: touch::Forwarder,
    in_flight: bool,
    in_flight_assist: Option<u64>,
    epoch: u64,
    cancellation: tokio_util::sync::CancellationToken,
    error: Option<String>,
    recovering: bool,
    transport_recovery: Option<recovery::RecoveryState>,
    throttle: Option<MouseThrottleRate>,
    next_motion_at: Option<Instant>,
    deadline_timer: Option<crate::platform::deadline::Timer>,
    polling_warning: Option<polling::Warning>,
    listeners: Vec<Weak<dyn Fn() + Send + Sync>>,
}

#[derive(Clone, Default)]
pub(crate) struct RemoteInput {
    state: Arc<Mutex<State>>,
    wake: Arc<Notify>,
    drained: Arc<Notify>,
}

impl RemoteInput {
    pub(crate) fn set_host_input_allowed(&self, allowed: bool) {
        self.lock().accept_host_input = allowed;
    }

    pub(crate) fn accepts_host_input(&self) -> bool {
        self.lock().accept_host_input
    }

    pub fn set_throttle(&self, enabled: bool, rate: MouseThrottleRate) -> Result<()> {
        let mut s = self.lock();
        let throttle = enabled.then_some(rate);
        if s.throttle == throttle {
            return Ok(());
        }
        if enabled && s.deadline_timer.is_none() {
            s.deadline_timer = Some(crate::platform::deadline::Timer::new(self.wake.clone())?);
        }
        if let Some(timer) = &mut s.deadline_timer {
            timer.disarm();
        }
        let retired = if enabled {
            None
        } else {
            s.deadline_timer.take()
        };
        s.throttle = throttle;
        s.next_motion_at = None;
        if enabled {
            s.polling_warning = None;
        }
        // Keep pending motion and button order when changing the user's policy.
        drop(s);
        // Joining a retiring callback must never hold the input queue lock.
        drop(retired);
        self.wake.notify_one();
        self.repaint();
        Ok(())
    }

    pub fn warn_high_polling(&self, hz: u32) {
        let mut s = self.lock();
        if s.throttle.is_some() || !s.ready || s.stopping || s.mode == MouseMode::View {
            return;
        }
        s.polling_warning = Some(polling::Warning {
            hz,
            until: Instant::now() + Duration::from_secs(10),
        });
        drop(s);
        self.repaint();
    }

    pub fn polling_warning(&self) -> Option<polling::Warning> {
        let s = self.lock();
        s.polling_warning.filter(|w| {
            s.throttle.is_none() && s.mode != MouseMode::View && Instant::now() < w.until
        })
    }

    pub fn set_keyboard_platform(&self, platform: i32) {
        self.lock().keyboard_platform = platform;
    }

    pub fn keyboard_generation(&self) -> u64 {
        self.lock().keyboard_generation
    }

    pub fn activation_generation(&self) -> u64 {
        self.lock().activation_generation
    }

    pub fn same_session(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }

    pub fn waiting_for_neutral(&self) -> bool {
        self.lock().waiting_for_neutral
    }

    /// A native input adapter confirms that both keyboard and mouse are up.
    /// A delayed callback from another activation cannot reopen input.
    pub fn confirm_neutral(&self, generation: u64) {
        let mut s = self.lock();
        if s.activation_generation == generation
            && s.waiting_for_neutral
            && s.mode != MouseMode::View
            && s.ready
            && !s.stopping
        {
            s.waiting_for_neutral = false;
            drop(s);
            self.repaint();
        }
    }

    pub fn keyboard_supported(&self) -> bool {
        matches!(self.lock().keyboard_platform, 1 | 4)
    }

    pub fn supports_ctrl_alt_del(&self) -> bool {
        self.lock().keyboard_platform == 1
    }

    /// Explicit viewer command, never injected into the local Windows desktop.
    pub fn send_ctrl_alt_del(&self, owner: u64) -> Result<()> {
        let mut s = self.lock();
        if s.keyboard_platform != 1 {
            bail!("仅支持向 Windows 设备发送 Ctrl+Alt+Del");
        }
        if !s.ready || s.stopping || s.mode == MouseMode::View || s.waiting_for_neutral {
            bail!("请先开启键鼠控制并松开按键");
        }
        // Retire physical input before the command, keeping any release owed
        // to transport. The entire finite sequence enters the same queue/epoch.
        Self::release_locked(&mut s);
        s.owner = Some(owner);
        for (key, down) in [
            (162, true),
            (164, true),
            (46, true),
            (46, false),
            (164, false),
            (162, false),
        ] {
            s.queue.push_back(InputEvent::Key {
                key: KeyboardKey::Windows(key),
                down,
                lock: None,
                // A finite menu command has no physical key/heartbeat owner.
                interrupt: false,
            });
        }
        // next() tracks potentially submitted presses in remote_keys, so
        // cancellation/failure still releases them without replaying Delete.
        drop(s);
        self.wake.notify_one();
        Ok(())
    }

    /// Native adapters supply local Windows VKs; the protocol owns target codes.
    pub fn key(&self, owner: u64, key: u16, down: bool, lock: Option<u8>) -> bool {
        let mut s = self.lock();
        let Some(remote_key) = KeyboardKey::from_windows_vk(s.keyboard_platform, key) else {
            return false;
        };
        if down {
            if !Self::claim(&mut s, owner) {
                return false;
            }
        } else if s.owner != Some(owner) || !s.keys.contains_key(&key) {
            return false;
        }
        // The Mac host toggles Caps Lock on every DOWN, including repeats.
        if down && remote_key == KeyboardKey::Mac(0x39) && s.keys.contains_key(&key) {
            return true;
        }
        let lock = lock.filter(|v| matches!(key, 20 | 144 | 145) && matches!(v, 0 | 128));
        let accepted = Self::push(
            &mut s,
            InputEvent::Key {
                key: remote_key,
                down,
                lock,
                interrupt: true,
            },
        );
        if accepted {
            if down {
                s.keys.insert(key, lock);
            } else {
                s.keys.remove(&key);
            }
        }
        drop(s);
        self.wake.notify_one();
        accepted
    }

    pub fn heartbeat(&self) {
        let mut s = self.lock();
        if s.ready
            && !s.stopping
            && s.mode != MouseMode::View
            && !s.keys.is_empty()
            && !s.queue.iter().any(|e| matches!(e, InputEvent::Heartbeat))
        {
            Self::push(&mut s, InputEvent::Heartbeat);
            drop(s);
            self.wake.notify_one();
        }
    }

    /// Full gamepad states share the keyboard/mouse owner and reliable epoch.
    // Only the Linux viewer samples controllers so far (SDL2); the Windows
    // controller has no collector yet, see README's 功能矩阵.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn gamepad(&self, owner: u64, event: GamepadEvent) -> bool {
        let index = event.index();
        if index >= MAX_GAMEPADS {
            return false;
        }
        let mut s = self.lock();
        let accepted = match event {
            GamepadEvent::Connect { .. } => {
                if !Self::claim(&mut s, owner) {
                    return false;
                }
                if s.gamepads.contains_key(&index) {
                    return true;
                }
                let accepted = Self::push(&mut s, InputEvent::Gamepad(event));
                if accepted {
                    s.gamepads.insert(index, GamepadState::default());
                }
                accepted
            }
            GamepadEvent::Update { state, .. } => {
                if s.owner != Some(owner)
                    || !s.gamepads.contains_key(&index)
                    || !Self::claim(&mut s, owner)
                {
                    return false;
                }
                if s.gamepads.get(&index) == Some(&state) {
                    return true;
                }
                let accepted = if let Some(InputEvent::Gamepad(GamepadEvent::Update {
                    index: previous,
                    state: previous_state,
                })) = s.queue.back_mut()
                    && *previous == index
                    && previous_state.coalesces_with(state)
                    && (*previous_state == GamepadState::default())
                        == (state == GamepadState::default())
                {
                    *previous_state = state;
                    true
                } else {
                    Self::push(&mut s, InputEvent::Gamepad(event))
                };
                if accepted {
                    s.gamepads.insert(index, state);
                }
                accepted
            }
            GamepadEvent::Disconnect { .. } => {
                // A collector can drop after pause_owner already retired it.
                // Never claim the current owner merely to repeat a disconnect.
                if s.owner != Some(owner) {
                    return s.owner.is_none() && !s.gamepads.contains_key(&index);
                }
                if s.gamepads.remove(&index).is_none() {
                    return true;
                }
                s.confirmed_gamepads.remove(&index);
                s.gamepad_feedback.remove(&index);
                s.queue.retain(|queued| !matches!(queued, InputEvent::Gamepad(value) if value.index() == index));
                if s.remote_gamepads.contains(&index) {
                    Self::queue_gamepad_release(&mut s, index);
                }
                true
            }
        };
        drop(s);
        if accepted {
            self.wake.notify_one();
        }
        accepted
    }

    /// Gamepad slots and vibration belong to one reliable epoch; a collector
    /// that sees it change starts over instead of replaying the old slots.
    pub(crate) fn gamepad_epoch(&self) -> u64 {
        self.lock().epoch
    }

    pub(crate) fn receive_gamepad_vibration(&self, value: GamepadVibration) {
        let epoch = self.gamepad_epoch();
        self.receive_gamepad_vibration_in_epoch(epoch, value);
    }

    /// Async receivers capture the epoch before dispatching a feedback callback.
    pub(crate) fn receive_gamepad_vibration_in_epoch(
        &self,
        epoch: u64,
        value: GamepadVibration,
    ) -> bool {
        let mut s = self.lock();
        let Some(owner) = s.owner else {
            return false;
        };
        if value.index >= MAX_GAMEPADS
            || s.epoch != epoch
            || !s.ready
            || s.stopping
            || s.mode == MouseMode::View
            || s.waiting_for_neutral
            || !s.gamepads.contains_key(&value.index)
            || !s.confirmed_gamepads.contains(&value.index)
        {
            return false;
        }
        s.gamepad_feedback.insert(
            value.index,
            GamepadFeedback {
                owner,
                epoch,
                motors: value.motors(),
            },
        );
        true
    }

    // Only the Linux viewer samples controllers so far (SDL2); the Windows
    // controller has no collector yet, see README's 功能矩阵.
    #[cfg_attr(windows, allow(dead_code))]
    pub(crate) fn gamepad_vibration(&self, owner: u64, index: u8) -> Option<(u16, u16)> {
        let s = self.lock();
        if index >= MAX_GAMEPADS
            || s.owner != Some(owner)
            || !s.ready
            || s.stopping
            || s.mode == MouseMode::View
            || s.waiting_for_neutral
            || !s.gamepads.contains_key(&index)
            || !s.confirmed_gamepads.contains(&index)
        {
            return None;
        }
        Some(
            s.gamepad_feedback
                .get(&index)
                .filter(|feedback| feedback.owner == owner && feedback.epoch == s.epoch)
                .map_or((0, 0), |feedback| feedback.motors),
        )
    }

    pub fn subscribe(
        &self,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
        let mut s = self.lock();
        s.listeners.retain(|listener| listener.strong_count() != 0);
        s.listeners.push(Arc::downgrade(&callback));
        callback
    }

    pub fn repaint(&self) {
        let callbacks: Vec<_> = self
            .lock()
            .listeners
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for callback in callbacks {
            callback();
        }
    }
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    pub fn mode(&self) -> MouseMode {
        self.lock().mode
    }
    pub fn relative_mode(&self) -> bool {
        self.lock().relative
    }
    pub fn relative_available(&self) -> bool {
        !self.lock().relative_denied
    }
    /// Reported by the window that tried to take the pointer. It stays off for
    /// the rest of the session: whatever holds the pointer is outside this
    /// process, and retrying would only swing the mode back and forth.
    /// Only an X11/Wayland pointer grab can be refused by another client;
    /// the Windows player's raw-input capture has no such failure.
    #[cfg_attr(windows, allow(dead_code))]
    pub fn set_relative_available(&self, available: bool) {
        let mut s = self.lock();
        if s.relative_denied != available {
            return;
        }
        s.relative_denied = !available;
        drop(s);
        self.repaint();
    }
    pub fn set_relative_mode(&self, relative: bool) {
        let mut s = self.lock();
        s.relative = relative;
        drop(s);
        self.repaint();
    }
    pub fn error(&self) -> Option<String> {
        self.lock().error.clone()
    }
    pub(crate) fn owner_holds_left(&self, owner: u64) -> bool {
        let s = self.lock();
        s.owner == Some(owner) && s.held[0]
    }
    pub fn owner_holds_buttons(&self, owner: u64) -> bool {
        let s = self.lock();
        s.owner == Some(owner) && s.held.iter().any(|held| *held)
    }

    pub fn owner_holds_key(&self, owner: u64, key: u16) -> bool {
        let s = self.lock();
        s.owner == Some(owner) && s.keys.contains_key(&key)
    }

    pub fn set_ready(&self, ready: bool) {
        let mut s = self.lock();
        let became_ready = ready && !s.ready && !s.stopping;
        if !ready {
            s.drag_hold = None;
            Self::advance_epoch(&mut s);
            s.mode = MouseMode::View;
            s.waiting_for_neutral = false;
            s.owner = None;
            s.held = [false; 5];
            s.keys.clear();
            // Transport is gone; the host's own engine release lifts touches.
            s.touch.clear();
            s.keyboard_generation = s.keyboard_generation.wrapping_add(1);
            s.queue.clear();
            s.in_flight = false;
            s.in_flight_motion = false;
            s.in_flight_assist = None;
        } else if !s.ready && !s.stopping {
            // A transport send can have accepted DOWN before a disconnect.
            // Retire that uncertainty with UP before any newly enabled input.
            Self::release_locked(&mut s);
        }
        s.ready = ready && !s.stopping;
        drop(s);
        if became_ready {
            self.reconcile_keyboard_releases();
        }
        self.wake.notify_one();
        self.drained.notify_waiters();
        self.repaint();
    }

    pub fn enable(&self, mode: MouseMode, relative: bool) -> Result<()> {
        let mut s = self.lock();
        if !s.ready || s.stopping {
            bail!("控制连接尚未就绪");
        }
        if s.transport_recovery.as_ref().is_some_and(|r| !r.confirmed) {
            bail!("键鼠通道正在恢复，请稍候");
        }
        if s.mode == mode {
            s.relative = relative;
            return Ok(());
        }
        let entering_control = s.mode == MouseMode::View && mode != MouseMode::View;
        let waiting = s.waiting_for_neutral;
        Self::release_locked(&mut s);
        s.mode = mode;
        if entering_control {
            s.activation_generation = s.activation_generation.wrapping_add(1);
        }
        s.waiting_for_neutral = mode != MouseMode::View && (entering_control || waiting);
        s.relative = relative;
        s.error = None;
        s.recovering = false;
        drop(s);
        self.wake.notify_one();
        self.repaint();
        Ok(())
    }

    fn advance_epoch(s: &mut State) {
        s.transport_recovery = None;
        s.next_motion_at = None;
        if let Some(timer) = &mut s.deadline_timer {
            timer.disarm();
        }
        s.epoch = s.epoch.wrapping_add(1);
        s.cancellation.cancel();
        s.cancellation = tokio_util::sync::CancellationToken::new();
        s.in_flight = false;
        s.in_flight_motion = false;
        s.in_flight_assist = None;
        s.gamepads.clear();
        s.confirmed_gamepads.clear();
        s.gamepad_feedback.clear();
    }

    fn queue_gamepad_release(s: &mut State, index: u8) {
        // This finite release pass bypasses ordinary producer backpressure.
        s.queue.push_back(InputEvent::Gamepad(GamepadEvent::Update {
            index,
            state: GamepadState::default(),
        }));
        s.queue
            .push_back(InputEvent::Gamepad(GamepadEvent::Disconnect { index }));
    }

    fn release_gamepads_locked(s: &mut State) {
        s.gamepads.clear();
        s.confirmed_gamepads.clear();
        s.gamepad_feedback.clear();
        s.queue
            .retain(|event| !matches!(event, InputEvent::Gamepad(_)));
        for index in s.remote_gamepads.iter().copied().collect::<Vec<_>>() {
            Self::queue_gamepad_release(s, index);
        }
    }

    fn release_locked(s: &mut State) {
        s.drag_hold = None;
        s.assists.clear();
        s.assist_button_owner = None;
        s.assist_down = false;
        Self::advance_epoch(s);
        // Cancel unsent presses/motion. Preserve release obligations for events
        // already given to transport. Never replay an obsolete click on resume.
        s.queue.clear();
        s.keyboard_generation = s.keyboard_generation.wrapping_add(1);
        s.release_checkpoint.extend(s.remote_keys.keys().copied());
        // Release ordinary keys before modifiers; never synthesize new downs.
        for modifier in [false, true] {
            for &key in s.remote_keys.keys() {
                if key.is_modifier() == modifier {
                    s.queue.push_back(InputEvent::Key {
                        key,
                        down: false,
                        lock: None,
                        interrupt: false,
                    });
                }
            }
        }
        s.keys.clear();
        for (index, button) in BUTTONS.into_iter().enumerate() {
            if s.remote_held[index] {
                s.queue.push_back(InputEvent::Button {
                    button,
                    down: false,
                });
            }
        }
        s.held = [false; 5];
        // Lift remote touches like held buttons; the frame is a release
        // obligation and bypasses backpressure, so a finger never stays
        // pressed after control is lost.
        if let Some((kind, points)) = s.touch.clear() {
            s.queue.push_back(InputEvent::Touch { kind, points });
        }
        Self::release_gamepads_locked(s);
        s.owner = None;
    }

    pub fn pause_owner(&self, owner: u64) {
        let mut s = self.lock();
        if s.owner != Some(owner) {
            return;
        }
        if s.drag_hold
            .is_some_and(|(held_owner, _)| held_owner == owner)
            && s.held[0]
        {
            // Only the original LMB press belongs to a file handoff. Ordinary
            // focus loss still retires keyboard and other button ownership.
            if !s.keys.is_empty() || s.held[1..].iter().any(|v| *v) {
                s.queue.clear();
                s.keyboard_generation = s.keyboard_generation.wrapping_add(1);
                let keys: Vec<_> = s.remote_keys.keys().copied().collect();
                s.release_checkpoint.extend(keys.iter().copied());
                for modifier in [false, true] {
                    for &key in &keys {
                        if key.is_modifier() == modifier {
                            s.queue.push_back(InputEvent::Key {
                                key,
                                down: false,
                                lock: None,
                                interrupt: false,
                            });
                        }
                    }
                }
                s.keys.clear();
                for (index, button) in BUTTONS.into_iter().enumerate().skip(1) {
                    if s.remote_held[index] {
                        s.queue.push_back(InputEvent::Button {
                            button,
                            down: false,
                        });
                    }
                    s.held[index] = false;
                }
            }
            // After the clear above, so the lift is never dropped with it.
            if let Some((kind, points)) = s.touch.clear() {
                s.queue.push_back(InputEvent::Touch { kind, points });
            }
            // A file drag keeps only its submitted left-button hold, never
            // a controller or vibration owned by the window losing focus.
            Self::release_gamepads_locked(&mut s);
        } else {
            Self::release_locked(&mut s);
        }
        drop(s);
        self.wake.notify_one();
    }

    pub(crate) fn hold_drag(&self, owner: u64) -> Result<DragHold> {
        let mut s = self.lock();
        anyhow::ensure!(
            s.ready
                && !s.stopping
                && s.mode != MouseMode::View
                && !s.relative
                && s.owner == Some(owner)
                && s.held[0]
                && s.remote_held[0]
                && s.drag_hold.is_none(),
            "原拖动按下已失效"
        );
        s.drag_serial = s
            .drag_serial
            .checked_add(1)
            .ok_or_else(|| anyhow::anyhow!("拖动代次已用尽"))?;
        let serial = s.drag_serial;
        s.drag_hold = Some((owner, serial));
        Ok(DragHold(Arc::new(DragHoldInner {
            input: self.clone(),
            owner,
            serial,
            active: std::sync::atomic::AtomicBool::new(true),
        })))
    }

    /// Retire input belonging to a display layout that is being replaced.
    pub(crate) fn pause_layout(&self) {
        let mut s = self.lock();
        Self::release_locked(&mut s);
        drop(s);
        self.wake.notify_one();
        self.repaint();
    }

    /// Reconcile release-only state across an OS input-desktop transition.
    /// This also covers Focused(false) retiring keys before WTS_SESSION_LOCK.
    /// Never replay a press or restore a stale Caps/Num/Scroll lock preference.
    pub fn reconcile_keyboard_releases(&self) {
        let mut s = self.lock();
        let retired = std::mem::take(&mut s.release_checkpoint);
        if s.ready {
            // Native Mac codes do not sort ordinary keys before modifiers.
            // Keep checkpoint repairs in the same release order as focus loss,
            // including when a modifier release is already queued.
            for modifier in [false, true] {
                for &key in &retired {
                    if key.is_modifier() != modifier
                        || s.queue.iter().any(|event| matches!(event, InputEvent::Key { key: queued, down: false, .. } if *queued == key))
                    {
                        continue;
                    }
                    s.remote_keys.entry(key).or_insert(None);
                    let event = InputEvent::Key {
                        key,
                        down: false,
                        lock: None,
                        interrupt: false,
                    };
                    let before_modifier = (!modifier).then(|| s.queue.iter().position(|event| {
                        matches!(event, InputEvent::Key { key, down: false, .. } if key.is_modifier())
                    })).flatten();
                    if let Some(index) = before_modifier {
                        s.queue.insert(index, event);
                    } else {
                        s.queue.push_back(event);
                    }
                }
            }
        } else {
            s.release_checkpoint = retired;
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn disable(&self) {
        let mut s = self.lock();
        s.mode = MouseMode::View;
        s.waiting_for_neutral = false;
        Self::release_locked(&mut s);
        if !s.ready {
            s.queue.clear();
        }
        drop(s);
        self.wake.notify_one();
        self.repaint();
    }

    pub fn fail(&self, error: String) {
        tracing::warn!(%error, "remote input stopped by local failure");
        self.disable();
        self.lock().error = Some(error);
        self.repaint();
    }

    fn claim(s: &mut State, owner: u64) -> bool {
        if !s.ready || s.stopping || s.mode == MouseMode::View || s.waiting_for_neutral {
            return false;
        }
        if s.owner != Some(owner) {
            Self::release_locked(s);
            s.owner = Some(owner);
        }
        true
    }

    fn push(s: &mut State, event: InputEvent) -> bool {
        if s.queue.len() >= MAX_EVENTS {
            tracing::warn!(
                queued = s.queue.len(),
                "remote input queue full; automatic recovery pending"
            );
            Self::begin_recovery_locked(s);
            return false;
        }
        s.queue.push_back(event);
        true
    }

    pub fn absolute(&self, owner: u64, screen: i32, x: f64, y: f64) {
        if screen < 0
            || !x.is_finite()
            || !y.is_finite()
            || !(0.0..1.0).contains(&x)
            || !(0.0..1.0).contains(&y)
        {
            return;
        }
        let mut s = self.lock();
        if s.relative || !Self::claim(&mut s, owner) {
            return;
        }
        if let Some(InputEvent::Absolute {
            screen: old_screen,
            x: old_x,
            y: old_y,
        }) = s.queue.back_mut()
            && *old_screen == screen
        {
            *old_x = x;
            *old_y = y;
        } else {
            Self::push(&mut s, InputEvent::Absolute { screen, x, y });
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn relative(&self, owner: u64, x: i32, y: i32) {
        if x == 0 && y == 0 {
            return;
        }
        let mut s = self.lock();
        if !s.relative || !Self::claim(&mut s, owner) {
            return;
        }
        s.physical_motion[0] = s.physical_motion[0].saturating_add(i64::from(x));
        s.physical_motion[1] = s.physical_motion[1].saturating_add(i64::from(y));
        if let Some(InputEvent::Relative { x: old_x, y: old_y }) = s.queue.back_mut()
            && let (Some(x), Some(y)) = (old_x.checked_add(x), old_y.checked_add(y))
        {
            *old_x = x;
            *old_y = y;
        } else {
            Self::push(&mut s, InputEvent::Relative { x, y });
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn button(&self, owner: u64, button: u32, down: bool) {
        let Some(index) = BUTTONS.iter().position(|b| *b == button) else {
            return;
        };
        let mut s = self.lock();
        if down {
            if !Self::claim(&mut s, owner) {
                return;
            }
        } else if s.owner != Some(owner) || !s.held[index] {
            return;
        }
        if index == 0 && down && s.assist_down {
            s.assist_down = false;
            s.assist_button_owner = None;
            s.queue
                .retain(|e| !matches!(e, InputEvent::AssistButton { .. }));
            if s.remote_held[0] {
                s.queue.push_back(InputEvent::Button {
                    button: 1,
                    down: false,
                });
            }
        }
        let combined_before = s.held[index] || (index == 0 && s.assist_down);
        s.held[index] = down;
        let combined_after = down || (index == 0 && s.assist_down);
        if combined_before == combined_after {
            return;
        }
        // Do not collapse rapid DOWN/UP transitions to a frame's final state.
        if Self::push(&mut s, InputEvent::Button { button, down }) {
            s.held[index] = down;
            tracing::trace!(button, down, "mouse button edge queued");
        }
        drop(s);
        self.wake.notify_one();
    }

    /// Forwards one touchscreen contact: as a native frame when `native`
    /// (decided by the gesture's first contact), otherwise as the emulated
    /// left button. Returns false when the event belongs to the local UI or
    /// control is unavailable, leaving it for egui to handle.
    pub fn touch(
        &self,
        owner: u64,
        phase: Phase,
        id: u32,
        pressure: f64,
        spot: Option<Spot>,
        native: bool,
    ) -> bool {
        let mut s = self.lock();
        let mut next = s.touch.clone();
        let mut step = next.step(phase, id, pressure, spot, native);
        if step == touch::Step::Local {
            return false;
        }
        if s.owner != Some(owner) {
            // Only a new contact takes control over. Claiming lifts the
            // previous owner's contacts, so step again on what is left.
            if phase != Phase::Start || !Self::claim(&mut s, owner) {
                return false;
            }
            next = s.touch.clone();
            step = next.step(phase, id, pressure, spot, native);
        } else if !matches!(
            step,
            touch::Step::Frame(3 | 4, _)
                | touch::Step::Pointer {
                    button: Some(false),
                    ..
                }
        ) && !Self::claim(&mut s, owner)
        {
            // Lifts are release obligations; everything else needs control.
            return false;
        }
        s.touch = next;
        match step {
            touch::Step::Local => return false,
            touch::Step::Held => {}
            touch::Step::Frame(kind, points) => {
                // Kind 2 snapshots supersede each other; every other kind is
                // an edge that must never be folded away. Lifts queue
                // unconditionally as release obligations, like key and
                // button UPs.
                if kind == 2
                    && let Some(InputEvent::Touch {
                        kind: 2,
                        points: old,
                    }) = s.queue.back_mut()
                {
                    *old = points;
                } else if matches!(kind, 3 | 4) {
                    s.queue.push_back(InputEvent::Touch { kind, points });
                } else {
                    Self::push(&mut s, InputEvent::Touch { kind, points });
                }
                drop(s);
                self.wake.notify_one();
            }
            touch::Step::Pointer { at, button } => {
                drop(s);
                if let Some((screen, x, y)) = at {
                    self.absolute(owner, screen, x, y);
                }
                if let Some(down) = button {
                    self.button(owner, 1, down);
                }
            }
        }
        true
    }

    pub fn wheel(&self, owner: u64, delta: i32, horizontal: bool) {
        if delta == 0 {
            return;
        }
        let mut s = self.lock();
        if !Self::claim(&mut s, owner) {
            return;
        }
        Self::push(&mut s, InputEvent::Wheel { delta, horizontal });
        drop(s);
        self.wake.notify_one();
    }

    pub async fn next(&self) -> InputWork {
        loop {
            let wake = self.wake.notified();
            tokio::pin!(wake);
            wake.as_mut().enable();
            let deadline;
            {
                let mut s = self.lock();
                if let Some(recovery) = Self::take_recovery_locked(&mut s) {
                    return InputWork::Recovery(recovery);
                }
                if s.queue
                    .front()
                    .is_some_and(|event| !Self::pending_current(&s, event))
                {
                    s.queue.pop_front();
                    continue;
                }
                let now = Instant::now();
                deadline = Self::motion_deadline(&s, now);
                if let Some(timer) = &mut s.deadline_timer {
                    if let Some(deadline) = deadline {
                        if let Err(error) = timer.arm(deadline) {
                            tracing::warn!(%error, "remote input stopped by polling timer failure");
                            Self::release_locked(&mut s);
                            s.mode = MouseMode::View;
                            s.error = Some(format!("鼠标节流计时失败，控制已停止：{error:#}"));
                            drop(s);
                            self.wake.notify_one();
                            self.repaint();
                            continue;
                        }
                    } else {
                        timer.disarm();
                    }
                }
                if deadline.is_none()
                    && let Some(event) = s.queue.pop_front()
                {
                    if let Some(rate) = s.throttle.filter(|_| event.movement()) {
                        let interval = rate.interval();
                        // Retain the cadence across small scheduling/send delays.
                        // Skip missed slots rather than accumulating catch-up work.
                        let slot = s
                            .next_motion_at
                            .filter(|at| *at + interval > now)
                            .unwrap_or(now);
                        s.next_motion_at = Some(slot + interval);
                    }
                    s.in_flight = true;
                    s.in_flight_motion = matches!(
                        event,
                        InputEvent::Relative { .. } | InputEvent::Correction { .. }
                    );
                    s.in_flight_assist = match event {
                        InputEvent::Correction { lease, .. }
                        | InputEvent::AssistButton { lease, .. } => Some(lease),
                        _ => None,
                    };
                    if let InputEvent::Button { button, down: true } = event
                        && let Some(index) = BUTTONS.iter().position(|b| *b == button)
                    {
                        s.remote_held[index] = true;
                    }
                    if matches!(event, InputEvent::AssistButton { down: true, .. }) {
                        s.remote_held[0] = true;
                    }
                    if let InputEvent::Key {
                        key,
                        down: true,
                        lock,
                        ..
                    } = event
                    {
                        s.remote_keys.insert(key, lock);
                        s.release_checkpoint.remove(&key);
                    }
                    if let InputEvent::Gamepad(GamepadEvent::Connect { index }) = event {
                        s.remote_gamepads.insert(index);
                        s.confirmed_gamepads.remove(&index);
                    }
                    return InputWork::Event(QueuedInputEvent {
                        epoch: s.epoch,
                        event,
                    });
                }
            }
            // Physical input, cancellation and the one-shot native timer all
            // use the same wakeup. No periodic work while the queue is empty.
            wake.await;
        }
    }

    fn motion_deadline(s: &State, now: Instant) -> Option<Instant> {
        let interval = s.throttle?.interval();
        if !s.queue.front().is_some_and(InputEvent::movement) {
            return None;
        }
        s.next_motion_at.and_then(|at| {
            // Send the position before a click/key/wheel early, never reorder
            // the edge ahead of its position. At most one slot can be borrowed;
            // advancing the same virtual clock automatically repays it.
            let at = if s.queue.get(1).is_some_and(InputEvent::input_boundary) {
                at.checked_sub(interval).unwrap_or(now)
            } else {
                // Accept a physical move near the tick instead of making a
                // source at the selected rate wait for tiny scheduling differences.
                // This allowance uses the same clock; it does not accumulate.
                if matches!(
                    s.queue.front(),
                    Some(InputEvent::Absolute { .. } | InputEvent::Relative { .. })
                ) && at.saturating_duration_since(now) <= MOTION_JITTER
                {
                    return None;
                }
                at
            };
            (at > now).then_some(at)
        })
    }

    fn pending_current(s: &State, event: &InputEvent) -> bool {
        match *event {
            InputEvent::Correction { lease, expires, .. } => {
                s.assists
                    .get(&lease)
                    .is_some_and(|(owner, guard)| s.owner == Some(*owner) && guard())
                    && std::time::Instant::now() <= expires
            }
            InputEvent::AssistButton {
                lease,
                down,
                expires,
            } => s.assists.get(&lease).is_some_and(|(owner, guard)| {
                s.owner == Some(*owner)
                    && (!down || (guard() && std::time::Instant::now() <= expires))
            }),
            InputEvent::Gamepad(GamepadEvent::Connect { index }) => s.gamepads.contains_key(&index),
            InputEvent::Gamepad(GamepadEvent::Update { index, state }) => {
                s.gamepads.contains_key(&index)
                    || (state == GamepadState::default() && s.remote_gamepads.contains(&index))
            }
            InputEvent::Gamepad(GamepadEvent::Disconnect { index }) => {
                s.remote_gamepads.contains(&index)
            }
            _ => true,
        }
    }
    pub fn is_current(&self, event: &QueuedInputEvent) -> bool {
        let s = self.lock();
        s.ready && event.epoch == s.epoch && Self::pending_current(&s, &event.event)
    }

    /// The sender may have waited for reliable-channel credit after next().
    /// Fold only consecutive, still-local motion into that event immediately
    /// before submission; never cross a click/key/wheel, screen or epoch.
    pub(crate) fn refresh_pending(&self, event: &mut QueuedInputEvent) -> bool {
        let mut s = self.lock();
        if !s.ready || event.epoch != s.epoch || !Self::pending_current(&s, &event.event) {
            return false;
        }
        loop {
            match (&mut event.event, s.queue.front()) {
                (
                    InputEvent::Absolute { screen, x, y },
                    Some(InputEvent::Absolute {
                        screen: next,
                        x: nx,
                        y: ny,
                    }),
                ) if screen == next => {
                    *x = *nx;
                    *y = *ny;
                }
                (InputEvent::Relative { x, y }, Some(InputEvent::Relative { x: nx, y: ny })) => {
                    let (Some(nx), Some(ny)) = (x.checked_add(*nx), y.checked_add(*ny)) else {
                        break;
                    };
                    *x = nx;
                    *y = ny;
                }
                (InputEvent::Heartbeat, Some(InputEvent::Heartbeat)) => {}
                (
                    InputEvent::Touch { kind, points },
                    Some(InputEvent::Touch {
                        kind: next_kind,
                        points: next_points,
                    }),
                ) if *kind == 2 && *next_kind == 2 => {
                    // Full snapshots: the newest frame alone decides the
                    // contact set; an intermediate one is redundant.
                    *points = next_points.clone();
                }
                (
                    InputEvent::Gamepad(GamepadEvent::Update { index, state }),
                    Some(InputEvent::Gamepad(GamepadEvent::Update {
                        index: next,
                        state: latest,
                    })),
                ) if *index == *next
                    && state.coalesces_with(*latest)
                    && (*state == GamepadState::default())
                        == (*latest == GamepadState::default()) =>
                {
                    *state = *latest;
                }
                _ => break,
            }
            s.queue.pop_front();
        }
        true
    }

    pub fn discard(&self, event: &QueuedInputEvent) {
        let mut s = self.lock();
        if event.epoch == s.epoch {
            s.in_flight = false;
            s.in_flight_motion = false;
            s.in_flight_assist = None;
        }
        drop(s);
        self.drained.notify_waiters();
    }

    pub async fn epoch_cancelled(&self, epoch: u64) {
        let token = {
            let state = self.lock();
            if state.epoch != epoch {
                return;
            }
            state.cancellation.clone()
        };
        token.cancelled().await;
    }
    pub fn motion(&self) -> [i64; 2] {
        self.lock().physical_motion
    }
    pub fn observation(&self) -> ([i64; 2], [i64; 2], [i64; 2], bool, bool) {
        let s = self.lock();
        (
            s.physical_motion,
            s.submitted_motion,
            s.submitted_corrections,
            s.in_flight_motion
                || s.queue.iter().any(|e| {
                    matches!(
                        e,
                        InputEvent::Relative { .. } | InputEvent::Correction { .. }
                    )
                }),
            s.in_flight_assist.is_some()
                || s.queue
                    .iter()
                    .any(|e| matches!(e, InputEvent::Correction { .. })),
        )
    }
    pub fn begin_assist(
        &self,
        owner: u64,
        lease: u64,
        guard: Arc<dyn Fn() -> bool + Send + Sync>,
    ) -> Result<()> {
        let mut s = self.lock();
        if !s.ready || s.stopping || s.mode == MouseMode::View || !s.relative {
            bail!("请开启相对鼠标控制");
        }
        if !guard() || !Self::claim(&mut s, owner) {
            bail!("控制不可用");
        }
        if s.assists.len() >= 4 && !s.assists.contains_key(&lease) {
            bail!("控制来源数量超限");
        }
        s.assists.insert(lease, (owner, guard));
        Ok(())
    }
    pub fn assist_pending(&self, lease: u64) -> bool {
        let s = self.lock();
        s.in_flight_assist==Some(lease)||s.queue.iter().any(|e|matches!(e,InputEvent::Correction{lease:id,..}|InputEvent::AssistButton{lease:id,..} if *id==lease))
    }
    pub fn assist_active(&self, owner: u64, lease: u64) -> bool {
        let s = self.lock();
        s.ready
            && !s.stopping
            && s.mode != MouseMode::View
            && s.relative
            && s.owner == Some(owner)
            && s.assists
                .get(&lease)
                .is_some_and(|(o, g)| *o == owner && g())
    }
    pub fn end_assist(&self, lease: u64) {
        let mut s = self.lock();
        if s.assists.remove(&lease).is_none() {
            return;
        }
        let owns_button = s.assist_button_owner == Some(lease);
        if owns_button {
            s.assist_button_owner = None;
            s.assist_down = false;
        }
        s.queue.retain(|e|!matches!(e,InputEvent::Correction{lease:id,..}|InputEvent::AssistButton{lease:id,..} if *id==lease));
        if owns_button && !s.held[0] && s.remote_held[0] {
            s.queue.push_back(InputEvent::Button {
                button: 1,
                down: false,
            });
        }
        drop(s);
        self.wake.notify_one();
    }
    pub fn assist_correction(
        &self,
        owner: u64,
        lease: u64,
        movement: [i32; 2],
        basis: CorrectionBasis,
        sampled_at: std::time::Instant,
        weight: [f32; 2],
    ) -> bool {
        let mut s = self.lock();
        if !s.ready
            || s.stopping
            || !s.relative
            || s.mode == MouseMode::View
            || s.owner != Some(owner)
            || s.assists.get(&lease).is_none_or(|(o, _)| *o != owner)
        {
            return false;
        }
        // The image predictor stops at request dispatch. Account once for prior
        // corrections committed while that request was being inferred. Pending
        // corrections that will be replaced below were never committed.
        let delta: [f64; 2] = std::array::from_fn(|i| {
            s.physical_motion[i].saturating_sub(basis.physical[i]) as f64
                + basis.submitted_corrections.map_or(0., |base| {
                    s.submitted_corrections[i].saturating_sub(base[i]) as f64
                })
        });
        if !weight
            .iter()
            .all(|v| v.is_finite() && (0.0..=1.0).contains(v))
        {
            return false;
        }
        let x = (f64::from(movement[0]) - delta[0] * f64::from(weight[0]))
            .round()
            .clamp(-512.0, 512.0) as i32;
        let y = (f64::from(movement[1]) - delta[1] * f64::from(weight[1]))
            .round()
            .clamp(-512.0, 512.0) as i32;
        // Keep only the newest correction; physical events retain their order.
        s.queue
            .retain(|e| !matches!(e,InputEvent::Correction{lease:id,..} if *id==lease));
        let accepted = Self::push(
            &mut s,
            InputEvent::Correction {
                x,
                y,
                lease,
                expires: sampled_at + std::time::Duration::from_millis(120),
            },
        );
        drop(s);
        self.wake.notify_one();
        accepted
    }
    pub fn assist_button(&self, owner: u64, lease: u64, down: bool) -> bool {
        let mut s = self.lock();
        if !s.ready
            || s.stopping
            || s.owner != Some(owner)
            || s.assists.get(&lease).is_none_or(|(o, _)| *o != owner)
        {
            return false;
        }
        if down && (s.held[0] || s.assist_button_owner.is_some_and(|id| id != lease)) {
            return true;
        }
        if !down && s.assist_button_owner != Some(lease) {
            return true;
        }
        if down {
            s.assist_button_owner = Some(lease);
        }
        let old = s.assist_down || s.held[0];
        s.assist_down = down;
        let new = down || s.held[0];
        let accepted = old == new
            || Self::push(
                &mut s,
                InputEvent::AssistButton {
                    down: new,
                    lease,
                    expires: std::time::Instant::now() + std::time::Duration::from_millis(100),
                },
            );
        drop(s);
        self.wake.notify_one();
        accepted
    }

    pub fn complete(&self, event: &QueuedInputEvent, result: Result<()>) {
        if result.is_ok()
            && let InputEvent::Relative { x, y } | InputEvent::Correction { x, y, .. } = event.event
        {
            let mut s = self.lock();
            s.submitted_motion[0] = s.submitted_motion[0].saturating_add(i64::from(x));
            s.submitted_motion[1] = s.submitted_motion[1].saturating_add(i64::from(y));
            if matches!(event.event, InputEvent::Correction { .. }) {
                s.submitted_corrections[0] =
                    s.submitted_corrections[0].saturating_add(i64::from(x));
                s.submitted_corrections[1] =
                    s.submitted_corrections[1].saturating_add(i64::from(y));
            }
        }
        if !self.is_current(event) {
            self.discard(event);
            return;
        }
        let mut s = self.lock();
        if event.epoch != s.epoch {
            return;
        }
        s.in_flight = false;
        s.in_flight_motion = false;
        s.in_flight_assist = None;
        if !Self::pending_current(&s, &event.event) {
            drop(s);
            self.drained.notify_waiters();
            return;
        }
        match result {
            Ok(()) => {
                if let InputEvent::Gamepad(event) = event.event {
                    match event {
                        GamepadEvent::Connect { index } => {
                            s.confirmed_gamepads.insert(index);
                        }
                        GamepadEvent::Disconnect { index } => {
                            s.remote_gamepads.remove(&index);
                            s.confirmed_gamepads.remove(&index);
                            s.gamepad_feedback.remove(&index);
                        }
                        GamepadEvent::Update { .. } => {}
                    }
                }
                if let InputEvent::Key {
                    key, down: false, ..
                } = event.event
                {
                    s.remote_keys.remove(&key);
                }
                if let InputEvent::AssistButton {
                    down: false, lease, ..
                } = event.event
                {
                    s.remote_held[0] = false;
                    if s.assist_button_owner == Some(lease) && !s.assist_down {
                        s.assist_button_owner = None;
                    }
                }
                if let InputEvent::Button { button, down } = event.event {
                    tracing::trace!(button, down, "mouse button edge submitted to transport");
                }
                if let InputEvent::Button {
                    button,
                    down: false,
                } = event.event
                    && let Some(index) = BUTTONS.iter().position(|b| *b == button)
                {
                    s.remote_held[index] = false;
                }
            }
            Err(error) => {
                s.mode = MouseMode::View;
                s.owner = None;
                s.held = [false; 5];
                if !s.recovering {
                    Self::release_locked(&mut s);
                    if let InputEvent::Button {
                        button,
                        down: false,
                    } = event.event
                    {
                        s.queue.retain(|event| !matches!(event, InputEvent::Button { button: queued, .. } if *queued == button));
                    }
                    s.recovering = true;
                    if let InputEvent::Key {
                        key, down: false, ..
                    } = event.event
                    {
                        s.queue.retain(
                            |e| !matches!(e, InputEvent::Key { key: queued, .. } if *queued == key),
                        );
                    }
                }
                s.error = Some(format!("键鼠发送失败，已停止控制：{error}"));
                // One best-effort release pass. Never retry presses/clicks, or
                // loop forever if the release itself cannot be sent.
            }
        }
        drop(s);
        self.drained.notify_waiters();
        if self.mode() == MouseMode::View {
            self.repaint();
        }
    }

    pub async fn close(&self) {
        {
            let mut s = self.lock();
            if s.stopping && !s.ready {
                return;
            }
            s.stopping = true;
            s.mode = MouseMode::View;
            Self::release_locked(&mut s);
            if !s.ready {
                s.queue.clear();
                s.in_flight = false;
                s.in_flight_motion = false;
                s.in_flight_assist = None;
            }
        }
        self.wake.notify_one();
        let drain = async {
            loop {
                let changed = self.drained.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                {
                    let s = self.lock();
                    if s.queue.is_empty() && !s.in_flight {
                        break;
                    }
                }
                changed.await;
            }
        };
        if tokio::time::timeout(std::time::Duration::from_secs(1), drain)
            .await
            .is_err()
        {
            tracing::warn!("mouse release drain timed out; remote receipt is unknown");
        }
        self.set_ready(false);
    }
}

/// A handoff may preserve an already-submitted press, never synthesize a new one.
#[derive(Clone)]
pub(crate) struct DragHold(Arc<DragHoldInner>);
struct DragHoldInner {
    input: RemoteInput,
    owner: u64,
    serial: u64,
    active: std::sync::atomic::AtomicBool,
}
impl DragHold {
    pub(crate) fn current(&self) -> bool {
        let s = self.0.input.lock();
        self.0.active.load(std::sync::atomic::Ordering::Acquire)
            && s.ready
            && !s.stopping
            && s.drag_hold == Some((self.0.owner, self.0.serial))
            && s.owner == Some(self.0.owner)
            && s.held[0]
            && s.remote_held[0]
    }
    pub(crate) fn cancel(&self) {
        self.0.cancel();
    }
    pub(crate) fn resume(&self, screen: i32, x: f64, y: f64, left: bool) -> bool {
        if screen < 0
            || !x.is_finite()
            || !y.is_finite()
            || !(0.0..=1.0).contains(&x)
            || !(0.0..=1.0).contains(&y)
        {
            return false;
        }
        let mut s = self.0.input.lock();
        if !s.ready
            || s.stopping
            || s.mode == MouseMode::View
            || s.drag_hold != Some((self.0.owner, self.0.serial))
            || s.owner != Some(self.0.owner)
            || !s.held[0]
            || !s.remote_held[0]
            || !self
                .0
                .active
                .swap(false, std::sync::atomic::Ordering::AcqRel)
        {
            return false;
        }
        s.drag_hold = None;
        let mut accepted = RemoteInput::push(&mut s, InputEvent::Absolute { screen, x, y });
        if accepted && !left {
            s.held[0] = false;
            accepted = RemoteInput::push(
                &mut s,
                InputEvent::Button {
                    button: 1,
                    down: false,
                },
            );
        }
        drop(s);
        self.0.input.wake.notify_one();
        self.0.input.repaint();
        accepted
    }
}
impl DragHoldInner {
    fn cancel(&self) {
        if !self.active.swap(false, std::sync::atomic::Ordering::AcqRel) {
            return;
        }
        let mut s = self.input.lock();
        if s.drag_hold == Some((self.owner, self.serial)) {
            s.drag_hold = None;
            RemoteInput::release_locked(&mut s);
        }
        drop(s);
        self.input.wake.notify_one();
        self.input.repaint();
    }
}
impl Drop for DragHoldInner {
    fn drop(&mut self) {
        self.cancel();
    }
}

#[cfg(test)]
mod gamepad_tests {
    use super::*;

    fn controlling() -> RemoteInput {
        let input = RemoteInput::default();
        input.set_ready(true);
        input.enable(MouseMode::Remote, false).unwrap();
        input.confirm_neutral(input.activation_generation());
        input
    }

    async fn event(input: &RemoteInput) -> QueuedInputEvent {
        match tokio::time::timeout(Duration::from_secs(1), input.next())
            .await
            .unwrap()
        {
            InputWork::Event(event) => event,
            InputWork::Recovery(_) => panic!("unexpected recovery"),
        }
    }

    async fn connected(input: &RemoteInput, owner: u64, index: u8) {
        assert!(input.gamepad(owner, GamepadEvent::Connect { index }));
        let next = event(input).await;
        assert!(
            matches!(next.event, InputEvent::Gamepad(GamepadEvent::Connect { index: value }) if value == index)
        );
        input.complete(&next, Ok(()));
    }

    async fn release_pair(input: &RemoteInput, index: u8) {
        let neutral = event(input).await;
        assert!(
            matches!(neutral.event, InputEvent::Gamepad(GamepadEvent::Update { index: value, state }) if value == index && state == GamepadState::default())
        );
        assert!(neutral.event.release());
        input.complete(&neutral, Ok(()));
        let disconnect = event(input).await;
        assert!(
            matches!(disconnect.event, InputEvent::Gamepad(GamepadEvent::Disconnect { index: value }) if value == index)
        );
        assert!(disconnect.event.release());
        input.complete(&disconnect, Ok(()));
    }

    #[tokio::test]
    async fn connection_precedes_state_and_updates_coalesce_before_submission() {
        let input = controlling();
        let pressed = GamepadState {
            buttons: 0x1000,
            left_thumb_x: 4000,
            ..Default::default()
        };
        assert!(!input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: pressed
            }
        ));
        assert!(!input.gamepad(
            1,
            GamepadEvent::Connect {
                index: MAX_GAMEPADS
            }
        ));
        assert!(input.gamepad(1, GamepadEvent::Connect { index: 0 }));
        for axis in 4000..4500 {
            let state = GamepadState {
                left_thumb_x: axis,
                ..pressed
            };
            assert!(input.gamepad(1, GamepadEvent::Update { index: 0, state }));
            assert!(input.gamepad(1, GamepadEvent::Update { index: 0, state }));
        }
        assert_eq!(input.lock().queue.len(), 2);
        let connect = event(&input).await;
        assert!(input.lock().remote_gamepads.contains(&0));
        assert!(!input.lock().confirmed_gamepads.contains(&0));
        input.complete(&connect, Ok(()));
        let update = event(&input).await;
        assert!(
            matches!(update.event, InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state.left_thumb_x == 4499)
        );
        input.complete(&update, Ok(()));
        assert!(input.lock().queue.is_empty());
    }

    #[tokio::test]
    async fn backpressure_refresh_does_not_cross_a_release_boundary_or_slot() {
        let input = controlling();
        connected(&input, 1, 0).await;
        let state = |axis| GamepadState {
            left_thumb_x: axis,
            ..Default::default()
        };
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: state(1)
            }
        ));
        let mut pending = event(&input).await;
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: state(2)
            }
        ));
        assert!(input.refresh_pending(&mut pending));
        assert!(
            matches!(pending.event, InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state.left_thumb_x == 2)
        );
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: GamepadState::default()
            }
        ));
        assert!(input.refresh_pending(&mut pending));
        assert!(!pending.event.release());
        input.complete(&pending, Ok(()));
        let neutral = event(&input).await;
        assert!(neutral.event.release());
        input.complete(&neutral, Ok(()));

        let held = GamepadState {
            buttons: 0x1000,
            ..Default::default()
        };
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: held
            }
        ));
        let mut pending = event(&input).await;
        let moved = GamepadState {
            left_thumb_x: 1000,
            ..held
        };
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: moved
            }
        ));
        assert!(input.refresh_pending(&mut pending));
        assert!(
            matches!(pending.event, InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state == moved)
        );
        let other_button = GamepadState {
            buttons: 0x2000,
            ..moved
        };
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: other_button
            }
        ));
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: GamepadState::default()
            }
        ));
        assert!(input.refresh_pending(&mut pending));
        assert!(
            matches!(pending.event, InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state == moved)
        );
        input.complete(&pending, Ok(()));
        let switched = event(&input).await;
        assert!(
            matches!(switched.event, InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state == other_button)
        );
        input.complete(&switched, Ok(()));
        let released = event(&input).await;
        assert!(released.event.release());
        input.complete(&released, Ok(()));

        connected(&input, 1, 1).await;
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: state(3)
            }
        ));
        let mut pending = event(&input).await;
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 1,
                state: state(4)
            }
        ));
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: state(5)
            }
        ));
        assert!(input.refresh_pending(&mut pending));
        assert!(
            matches!(pending.event, InputEvent::Gamepad(GamepadEvent::Update { index: 0, state }) if state.left_thumb_x == 3)
        );
        input.complete(&pending, Ok(()));
        for (index, axis) in [(1, 4), (0, 5)] {
            let next = event(&input).await;
            assert!(
                matches!(next.event, InputEvent::Gamepad(GamepadEvent::Update { index: value, state }) if value == index && state.left_thumb_x == axis)
            );
            input.complete(&next, Ok(()));
        }
    }

    #[tokio::test]
    async fn short_button_presses_and_trigger_edges_remain_ordered() {
        let input = controlling();
        connected(&input, 1, 0).await;
        let a = GamepadState {
            buttons: 0x1000,
            ..Default::default()
        };
        let b = GamepadState {
            buttons: 0x2000,
            ..Default::default()
        };
        let b_trigger = GamepadState {
            left_trigger: 20,
            ..b
        };
        for state in [
            a,
            GamepadState::default(),
            b,
            GamepadState {
                left_trigger: 10,
                ..b
            },
            b_trigger,
            b,
        ] {
            assert!(input.gamepad(1, GamepadEvent::Update { index: 0, state }));
        }
        for expected in [a, GamepadState::default(), b, b_trigger, b] {
            let next = event(&input).await;
            assert!(
                matches!(next.event, InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state == expected)
            );
            input.complete(&next, Ok(()));
        }
        assert!(input.lock().queue.is_empty());
    }

    #[tokio::test]
    async fn focus_loss_cancels_pending_connect_but_releases_in_flight_connect() {
        let input = controlling();
        assert!(input.gamepad(1, GamepadEvent::Connect { index: 0 }));
        input.pause_owner(1);
        assert!(input.lock().queue.is_empty());
        assert!(input.gamepad(1, GamepadEvent::Disconnect { index: 0 }));
        assert!(input.lock().owner.is_none());
        assert!(input.gamepad(1, GamepadEvent::Connect { index: 0 }));
        let in_flight = event(&input).await;
        input.pause_owner(1);
        assert!(!input.is_current(&in_flight));
        input.complete(&in_flight, Ok(()));
        assert!(input.lock().confirmed_gamepads.is_empty());
        release_pair(&input, 0).await;
        assert!(input.lock().remote_gamepads.is_empty());
        assert!(input.lock().queue.is_empty());
    }

    #[tokio::test]
    async fn reconnection_keeps_an_unfinished_disconnect_as_a_release_obligation() {
        let input = controlling();
        connected(&input, 1, 0).await;
        assert!(input.gamepad(1, GamepadEvent::Disconnect { index: 0 }));
        let neutral = event(&input).await;
        input.complete(&neutral, Ok(()));
        let in_flight_disconnect = event(&input).await;
        input.set_ready(false);
        input.set_ready(true);
        input.complete(&in_flight_disconnect, Ok(()));
        assert!(input.lock().remote_gamepads.contains(&0));
        release_pair(&input, 0).await;
        assert!(input.lock().remote_gamepads.is_empty());
    }

    #[tokio::test]
    async fn transport_failure_never_replays_the_previous_axis_state() {
        let input = controlling();
        connected(&input, 1, 0).await;
        let pressed = GamepadState {
            buttons: 0x1000,
            right_thumb_y: 20000,
            ..Default::default()
        };
        assert!(input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: pressed
            }
        ));
        let failed = event(&input).await;
        let recovery = input.pause_transport(&failed).unwrap();
        assert_eq!(recovery.releases.len(), 2);
        assert!(recovery.releases.iter().all(InputEvent::release));
        assert!(
            matches!(recovery.releases[0], InputEvent::Gamepad(GamepadEvent::Update { state, .. }) if state == GamepadState::default())
        );
        assert!(!input.gamepad(
            1,
            GamepadEvent::Update {
                index: 0,
                state: pressed
            }
        ));
        input.finish_transport_recovery(recovery.epoch, Ok(()));
        assert!(input.lock().remote_gamepads.is_empty());
        assert!(input.lock().gamepads.is_empty());
        assert!(input.lock().queue.is_empty());
    }

    #[tokio::test]
    async fn vibration_holds_until_replaced_and_old_owner_or_epoch_cannot_restore_it() {
        let input = controlling();
        connected(&input, 1, 0).await;
        let old_epoch = input.gamepad_epoch();
        let vibration = GamepadVibration {
            index: 0,
            left_motor: 255,
            right_motor: 128,
        };
        assert!(input.receive_gamepad_vibration_in_epoch(old_epoch, vibration));
        assert_eq!(input.gamepad_vibration(1, 0), Some((65535, 32896)));
        // The wire has no duration: a single nonzero message can describe a
        // sustained effect longer than the old one-second local timeout.
        tokio::time::sleep(Duration::from_millis(1100)).await;
        assert_eq!(input.gamepad_vibration(1, 0), Some((65535, 32896)));
        input.receive_gamepad_vibration(GamepadVibration {
            index: 0,
            left_motor: 0,
            right_motor: 0,
        });
        assert_eq!(input.gamepad_vibration(1, 0), Some((0, 0)));
        assert_eq!(input.gamepad_vibration(2, 0), None);
        input.receive_gamepad_vibration(vibration);
        assert_eq!(input.gamepad_vibration(1, 0), Some((65535, 32896)));
        input.pause_owner(1);
        assert_eq!(input.gamepad_vibration(1, 0), None);
        release_pair(&input, 0).await;
        connected(&input, 2, 0).await;
        assert!(!input.receive_gamepad_vibration_in_epoch(old_epoch, vibration));
        assert_eq!(input.gamepad_vibration(2, 0), Some((0, 0)));
        assert!(!input.gamepad(1, GamepadEvent::Disconnect { index: 0 }));
        assert!(input.lock().gamepads.contains_key(&0));
        assert!(!input.receive_gamepad_vibration_in_epoch(
            input.gamepad_epoch(),
            GamepadVibration {
                index: MAX_GAMEPADS,
                ..vibration
            }
        ));
    }

    #[tokio::test]
    async fn file_handoff_preserves_only_the_mouse_hold_not_the_controller() {
        let input = controlling();
        input.button(1, 1, true);
        let press = event(&input).await;
        input.complete(&press, Ok(()));
        let hold = input.hold_drag(1).unwrap();
        connected(&input, 1, 0).await;
        input.pause_owner(1);
        assert!(hold.current());
        assert!(input.owner_holds_left(1));
        assert!(input.lock().gamepads.is_empty());
        release_pair(&input, 0).await;
        assert!(hold.current());
        hold.cancel();
        assert!(!hold.current());
    }
}

#[cfg(test)]
mod touch_tests {
    use super::*;

    fn controlling() -> RemoteInput {
        let input = RemoteInput::default();
        input.set_ready(true);
        input.enable(MouseMode::Remote, false).unwrap();
        input.confirm_neutral(input.activation_generation());
        input
    }

    async fn event(input: &RemoteInput) -> QueuedInputEvent {
        match tokio::time::timeout(Duration::from_secs(1), input.next())
            .await
            .unwrap()
        {
            InputWork::Event(event) => event,
            InputWork::Recovery(_) => panic!("unexpected recovery"),
        }
    }

    fn contacts(points: &[Point]) -> Vec<(u32, f64, f64)> {
        points.iter().map(|p| (p.id, p.x, p.y)).collect()
    }

    /// Encoded bytes through the exact host receive path: protobuf envelope,
    /// then the inner touch frame.
    fn decode(bytes: &[u8]) -> (u8, Vec<(u32, f64, f64)>) {
        let inner = crate::features::stream_control::publisher::input_event(bytes)
            .expect("protobuf envelope decodes")
            .expect("is an input event");
        match crate::features::host::input::wire::touch(inner.as_slice())
            .expect("touch frame decodes")
            .expect("is a touch event")
        {
            crate::features::host::input::wire::Event::Touch { kind, points } => {
                (kind, contacts(&points))
            }
            _ => panic!("expected a touch event"),
        }
    }

    fn touch(
        input: &RemoteInput,
        owner: u64,
        phase: Phase,
        id: u32,
        at: Option<(f64, f64)>,
    ) -> bool {
        input.touch(owner, phase, id, 0.5, at.map(|(x, y)| (0, x, y)), true)
    }

    #[tokio::test]
    async fn frames_travel_the_binary_channel_and_decode() {
        let input = controlling();
        assert!(touch(&input, 1, Phase::Start, 4, Some((0.25, 0.75))));
        let first = event(&input).await;
        assert!(!first.event.release());
        let bytes = first.event.encode();
        assert_ne!(bytes.first(), Some(&b'{'));
        assert_eq!(decode(&bytes), (1, vec![(4, 0.25, 0.75)]));
        // A second finger joins as a full snapshot with both contacts.
        assert!(touch(&input, 1, Phase::Start, 9, Some((0.5, 0.5))));
        let second = event(&input).await;
        assert_eq!(
            decode(&second.event.encode()),
            (2, vec![(4, 0.25, 0.75), (9, 0.5, 0.5)])
        );
        // One finger lifts into a snapshot without it, the last lift is kind 3.
        assert!(touch(&input, 1, Phase::End, 4, Some((0.3, 0.7))));
        let partial = event(&input).await;
        assert_eq!(decode(&partial.event.encode()), (2, vec![(9, 0.5, 0.5)]));
        assert!(touch(&input, 1, Phase::End, 9, None));
        let lift = event(&input).await;
        assert!(lift.event.release());
        assert_eq!(decode(&lift.event.encode()), (3, vec![]));
    }

    #[tokio::test]
    async fn snapshots_coalesce_before_submission() {
        let input = controlling();
        touch(&input, 1, Phase::Start, 4, Some((0.1, 0.1)));
        let press = event(&input).await;
        input.complete(&press, Ok(()));
        // The first move frame is in flight; later snapshots fold into it.
        touch(&input, 1, Phase::Move, 4, Some((0.2, 0.2)));
        let mut pending = event(&input).await;
        for i in 0..50 {
            assert!(touch(
                &input,
                1,
                Phase::Move,
                4,
                Some((0.2 + f64::from(i) * 0.001, 0.2))
            ));
        }
        assert_eq!(input.lock().queue.len(), 1);
        assert!(input.refresh_pending(&mut pending));
        assert!(input.lock().queue.is_empty());
        // The wire carries f32; 0.249 rounds the way f32 rounds it.
        assert_eq!(
            decode(&pending.event.encode()),
            (2, vec![(4, f64::from(0.249_f32), f64::from(0.2_f32))])
        );
        input.complete(&pending, Ok(()));
        // Edges never fold: the lift stays a frame of its own.
        touch(&input, 1, Phase::End, 4, None);
        assert_eq!(input.lock().queue.len(), 1);
    }

    #[tokio::test]
    async fn losing_control_lifts_held_touches() {
        let input = controlling();
        touch(&input, 1, Phase::Start, 4, Some((0.5, 0.5)));
        let press = event(&input).await;
        input.complete(&press, Ok(()));
        input.enable(MouseMode::View, false).unwrap();
        let lift = event(&input).await;
        assert!(lift.event.release());
        assert_eq!(decode(&lift.event.encode()), (3, vec![]));
        // The gesture tail neither forwards nor revives the contact.
        assert!(!touch(&input, 1, Phase::Move, 4, Some((0.6, 0.6))));
        assert!(!touch(&input, 1, Phase::End, 4, None));
    }

    #[tokio::test]
    async fn another_window_takes_over_without_reviving_contacts() {
        let input = controlling();
        touch(&input, 1, Phase::Start, 4, Some((0.5, 0.5)));
        let press = event(&input).await;
        input.complete(&press, Ok(()));
        // A move from a window that does not own input is refused.
        assert!(!touch(&input, 2, Phase::Move, 4, Some((0.6, 0.6))));
        // A new contact there takes over: window 1's finger lifts first,
        // then the new gesture opens with only its own contact.
        assert!(touch(&input, 2, Phase::Start, 7, Some((0.25, 0.25))));
        let lift = event(&input).await;
        assert_eq!(decode(&lift.event.encode()), (3, vec![]));
        input.complete(&lift, Ok(()));
        let open = event(&input).await;
        assert_eq!(decode(&open.event.encode()), (1, vec![(7, 0.25, 0.25)]));
    }

    #[tokio::test]
    async fn emulated_gesture_drives_the_left_button() {
        let input = controlling();
        assert!(input.touch(1, Phase::Start, 4, 0.5, Some((3, 0.25, 0.5)), false));
        let moved = event(&input).await;
        assert!(matches!(
            moved.event,
            InputEvent::Absolute {
                screen: 3,
                x: 0.25,
                y: 0.5
            }
        ));
        input.complete(&moved, Ok(()));
        let press = event(&input).await;
        assert!(matches!(
            press.event,
            InputEvent::Button {
                button: 1,
                down: true
            }
        ));
        input.complete(&press, Ok(()));
        // A second finger is consumed without reaching the remote.
        assert!(input.touch(1, Phase::Start, 5, 0.5, Some((3, 0.75, 0.5)), true));
        assert!(input.lock().queue.is_empty());
        assert!(input.touch(1, Phase::End, 4, 0.0, None, false));
        let release = event(&input).await;
        assert!(matches!(
            release.event,
            InputEvent::Button {
                button: 1,
                down: false
            }
        ));
    }

    #[tokio::test]
    async fn local_ui_contacts_are_not_consumed() {
        let input = controlling();
        assert!(!touch(&input, 1, Phase::Start, 4, None));
        assert!(!touch(&input, 1, Phase::Move, 4, Some((0.5, 0.5))));
        assert_eq!(input.lock().queue.len(), 0);
    }
}
