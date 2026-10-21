//! Typed keyboard/mouse events (peer `INPUT`, Input v2), independent of GUI
//! frame cadence.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use crate::i18n::t;
use anyhow::{Result, bail};
use std::time::{Duration, Instant};
use tokio::sync::Notify;

pub(crate) mod polling;
mod recovery;
pub(crate) use recovery::{InputRecovery, InputWork};

use crate::platform::windows::input::keyboard;
pub(crate) use crate::protocol::input::SystemCommand;

const MAX_EVENTS: usize = 512;
/// While keys are held, the stream is never silent for longer than this.
const HEARTBEAT: Duration = Duration::from_millis(50);
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
    /// Both axes in 1/120 of a notch.
    Wheel {
        x: i32,
        y: i32,
    },
    /// Keyboard stream. `down` is false only for a key release.
    Key {
        key: KeyInput,
        down: bool,
    },
    Heartbeat,
    /// An encoded TOUCH or PEN frame (`docs/protocol/touch-pen-brief.md`).
    /// `moving`: only contacts that moved (a newer one may replace it);
    /// `release`: it ends a contact or takes the pen away.
    Pointer {
        frame: Vec<u8>,
        moving: bool,
        release: bool,
    },
}

/// Messages ordered with the keys: lock state and system commands relate to
/// the key stream position at which they were sent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum KeyInput {
    /// HID usage `(page << 16) | id` of the physical key.
    Usage {
        usage: u32,
        repeat: bool,
    },
    /// Local Caps/Num/Scroll Lock state; `seq` is stamped at submission.
    Locks {
        state: [bool; 3],
        seq: u32,
    },
    Command(SystemCommand),
}

impl InputEvent {
    fn key(usage: u32, down: bool, repeat: bool) -> Self {
        Self::Key {
            key: KeyInput::Usage { usage, repeat },
            down,
        }
    }
    fn release(usage: u32) -> Self {
        Self::key(usage, false, false)
    }
    fn usage(&self) -> Option<(u32, bool)> {
        match *self {
            Self::Key {
                key: KeyInput::Usage { usage, .. },
                down,
            } => Some((usage, down)),
            _ => None,
        }
    }
    /// A non-repeat press of Caps/Num/Scroll Lock.
    fn lock_press(&self) -> bool {
        matches!(*self, Self::Key { key: KeyInput::Usage { usage, repeat: false }, down: true }
            if keyboard::lock_index(usage).is_some())
    }
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
                | Self::Pointer { .. }
        )
    }
    /// Sent without waiting for input capacity: it ends something held.
    pub(crate) fn releases(&self) -> bool {
        matches!(
            self,
            Self::Button { down: false, .. }
                | Self::Key { down: false, .. }
                | Self::AssistButton { down: false, .. }
                | Self::Pointer { release: true, .. }
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
    /// Encode as a peer `INPUT` (or TOUCH / PEN) frame.
    pub(crate) fn encode(&self) -> Vec<u8> {
        use crate::protocol::input::{self as wire, InputKind};
        if let Self::Pointer { frame, .. } = self {
            return frame.clone();
        }
        crate::protocol::input::encode(match *self {
            Self::Absolute { screen, x, y } => InputKind::MouseAbsolute(wire::MouseAbsolute {
                screen_id: Some(screen),
                x,
                y,
            }),
            Self::Relative { x, y } | Self::Correction { x, y, .. } => {
                InputKind::MouseRelative(wire::MouseRelative { dx: x, dy: y })
            }
            Self::Button { button, down } => InputKind::MouseButton(wire::MouseButton {
                button: u32::from(button),
                down,
            }),
            Self::AssistButton { down, .. } => {
                InputKind::MouseButton(wire::MouseButton { button: 1, down })
            }
            Self::Wheel { x, y } => InputKind::MouseWheel(wire::MouseWheel { dx: x, dy: y }),
            Self::Key {
                key: KeyInput::Usage { usage, repeat },
                down,
            } => InputKind::Key(wire::Key {
                usage,
                down,
                repeat,
            }),
            Self::Key {
                key: KeyInput::Locks { state, seq },
                ..
            } => InputKind::LockState(wire::LockState {
                caps: state[0],
                num: state[1],
                scroll: state[2],
                seq,
            }),
            Self::Key {
                key: KeyInput::Command(command),
                ..
            } => InputKind::SystemCommand(command as i32),
            Self::Heartbeat => InputKind::Heartbeat(wire::Heartbeat {}),
            Self::Pointer { .. } => unreachable!("encoded above"),
        })
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
    owner: Option<u64>,
    drag_hold: Option<(u64, u64)>,
    drag_serial: u64,
    held: [bool; 5],
    // Marked before handing DOWN to transport. A concurrent stop must release
    // even an in-flight press, not just messages whose send() already returned.
    remote_held: [bool; 5],
    /// Held keys by HID usage.
    keys: BTreeSet<u32>,
    remote_keys: BTreeSet<u32>,
    // Transport acceptance of UP is not acknowledgement of host injection.
    // Keep keys retired at an ownership boundary for session reconciliation.
    release_checkpoint: BTreeSet<u32>,
    /// Lock-key presses accepted by transport on this channel (`LockState.seq`).
    lock_presses: u32,
    /// The lock state the host should have after the queued input; unknown
    /// until a `LockState` is sent in this activation.
    lock_model: Option<[bool; 3]>,
    /// Last INPUT frame handed to transport, the heartbeat clock.
    last_sent: Option<Instant>,
    /// Touch contacts are down on this screen (sent, not yet lifted).
    touch_screen: Option<i32>,
    /// The pen is in range: screen, last position, touching.
    pen: Option<(i32, f32, f32, bool)>,
    keyboard_generation: u64,
    activation_generation: u64,
    waiting_for_neutral: bool,
    keyboard_platform: farwing_protocol::Platform,
    accept_host_input: bool,
    queue: VecDeque<InputEvent>,
    in_flight: bool,
    in_flight_assist: Option<u64>,
    epoch: u64,
    cancellation: tokio_util::sync::CancellationToken,
    error: Option<String>,
    recovering: bool,
    transport_recovery: Option<recovery::RecoveryState>,
    throttle: Option<MouseThrottleRate>,
    next_motion_at: Option<Instant>,
    deadline_timer: Option<crate::platform::windows::input::deadline::Timer>,
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
            s.deadline_timer = Some(crate::platform::windows::input::deadline::Timer::new(
                self.wake.clone(),
            )?);
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

    pub fn set_keyboard_platform(&self, platform: farwing_protocol::Platform) {
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
        self.lock().keyboard_platform == farwing_protocol::Platform::Windows
    }

    /// System commands (Ctrl+Alt+Del, lock, ...) target Windows hosts.
    pub fn supports_system_commands(&self) -> bool {
        self.lock().keyboard_platform == farwing_protocol::Platform::Windows
    }

    /// Explicit viewer command, never injected into the local Windows desktop.
    pub fn system_command(&self, owner: u64, command: SystemCommand) -> Result<()> {
        let mut s = self.lock();
        if s.keyboard_platform != farwing_protocol::Platform::Windows {
            bail!("{}", t().features.remote_input_system_command_windows_only);
        }
        if !s.ready || s.stopping || s.mode == MouseMode::View || s.waiting_for_neutral {
            bail!("{}", t().features.remote_input_control_off_or_keys_held);
        }
        // Retire physical input before the command, keeping any release owed
        // to transport; the host also releases its holds before executing it.
        Self::release_locked(&mut s);
        s.owner = Some(owner);
        if !Self::push(
            &mut s,
            InputEvent::Key {
                key: KeyInput::Command(command),
                down: true,
            },
        ) {
            bail!("{}", t().features.remote_input_queue_full);
        }
        drop(s);
        self.wake.notify_one();
        Ok(())
    }

    /// Native adapters supply the HID usage of the physical key, and only for
    /// a verified Windows peer.
    pub fn key(&self, owner: u64, usage: u32, down: bool) -> bool {
        if keyboard::scan(usage).is_none() {
            return false;
        }
        let mut s = self.lock();
        if s.keyboard_platform != farwing_protocol::Platform::Windows {
            return false;
        }
        if down {
            if !Self::claim(&mut s, owner) {
                return false;
            }
        } else if s.owner != Some(owner) || !s.keys.contains(&usage) {
            return false;
        }
        let event = InputEvent::key(usage, down, down && s.keys.contains(&usage));
        let lock_press = event.lock_press();
        let accepted = Self::push(&mut s, event);
        if accepted {
            if down {
                s.keys.insert(usage);
            } else {
                s.keys.remove(&usage);
            }
            // Pressing a lock key toggles the host as well as this machine.
            if lock_press
                && let (Some(model), Some(index)) =
                    (s.lock_model.as_mut(), keyboard::lock_index(usage))
            {
                model[index] = !model[index];
            }
        }
        drop(s);
        self.wake.notify_one();
        accepted
    }

    /// Called with the local lock state by the thread owning the focused
    /// viewer. Sends `LockState` when control activates and when the local
    /// state changed other than through a forwarded lock key.
    pub fn sync_locks(&self, owner: u64, local: [bool; 3]) {
        let mut s = self.lock();
        if s.keyboard_platform != farwing_protocol::Platform::Windows
            || s.lock_model == Some(local)
            || !Self::claim(&mut s, owner)
        {
            return;
        }
        if Self::push(
            &mut s,
            InputEvent::Key {
                key: KeyInput::Locks {
                    state: local,
                    seq: 0,
                },
                down: true,
            },
        ) {
            s.lock_model = Some(local);
        }
        drop(s);
        self.wake.notify_one();
    }

    pub fn heartbeat(&self) {
        let mut s = self.lock();
        if s.ready
            && !s.stopping
            && s.mode != MouseMode::View
            && (!s.keys.is_empty() || s.touch_screen.is_some() || s.pen.is_some_and(|p| p.3))
            && s.last_sent.is_none_or(|at| at.elapsed() >= HEARTBEAT)
            && !s.queue.iter().any(|e| matches!(e, InputEvent::Heartbeat))
        {
            Self::push(&mut s, InputEvent::Heartbeat);
            drop(s);
            self.wake.notify_one();
        }
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

    pub fn owner_holds_key(&self, owner: u64, usage: u32) -> bool {
        let s = self.lock();
        s.owner == Some(owner) && s.keys.contains(&usage)
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
            s.touch_screen = None;
            s.pen = None;
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
            bail!("{}", t().features.remote_input_control_not_ready);
        }
        if s.transport_recovery.as_ref().is_some_and(|r| !r.confirmed) {
            bail!("{}", t().features.remote_input_channel_recovering);
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
        s.release_checkpoint.extend(s.remote_keys.iter().copied());
        // Release ordinary keys before modifiers; never synthesize new downs.
        for modifier in [false, true] {
            for &usage in &s.remote_keys {
                if keyboard::modifier(usage) == modifier {
                    s.queue.push_back(InputEvent::release(usage));
                }
            }
        }
        s.keys.clear();
        // Queued presses were dropped: send the lock state again.
        s.lock_model = None;
        for (index, button) in BUTTONS.into_iter().enumerate() {
            if s.remote_held[index] {
                s.queue.push_back(InputEvent::Button {
                    button,
                    down: false,
                });
            }
        }
        s.held = [false; 5];
        // Lift touch contacts and take the pen away on the host.
        Self::release_pointers(s);
        s.owner = None;
    }

    fn release_pointers(s: &mut State) {
        use crate::protocol::pointer::{self as wire, PenPhase};
        if let Some(screen) = s.touch_screen.take() {
            s.queue.push_back(InputEvent::Pointer {
                frame: wire::encode_touch(&wire::Touch {
                    screen_id: Some(screen),
                    contacts: Vec::new(),
                    time_us: 0,
                }),
                moving: false,
                release: true,
            });
        }
        if let Some((screen, x, y, _)) = s.pen.take() {
            s.queue.push_back(InputEvent::Pointer {
                frame: wire::encode_pen(&wire::Pen {
                    screen_id: Some(screen),
                    samples: vec![wire::PenSample {
                        phase: PenPhase::Cancel as i32,
                        x,
                        y,
                        ..Default::default()
                    }],
                    time_us: 0,
                }),
                moving: false,
                release: true,
            });
        }
    }

    /// Every active touch contact of this window on `screen`; contacts that
    /// end carry UP or CANCEL (`docs/protocol/touch-pen-brief.md` §5.2).
    pub fn touch(&self, owner: u64, screen: i32, contacts: Vec<crate::protocol::pointer::Contact>) {
        use crate::protocol::pointer::{self as wire, TouchPhase};
        let mut s = self.lock();
        if s.relative || !Self::claim(&mut s, owner) {
            return;
        }
        let phase = |c: &wire::Contact| TouchPhase::try_from(c.phase).unwrap_or(TouchPhase::Unspecified);
        let moving = contacts.iter().all(|c| phase(c) == TouchPhase::Move);
        let down = contacts
            .iter()
            .any(|c| matches!(phase(c), TouchPhase::Down | TouchPhase::Move));
        let release = !down;
        let frame = wire::encode_touch(&wire::Touch {
            screen_id: Some(screen),
            contacts,
            time_us: 0,
        });
        s.touch_screen = down.then_some(screen);
        // Only the newest of consecutive pure-move frames matters.
        if moving
            && let Some(InputEvent::Pointer {
                frame: old,
                moving: true,
                ..
            }) = s.queue.back_mut()
            && old.get(5) == frame.get(5)
        {
            *old = frame;
        } else {
            Self::push(&mut s, InputEvent::Pointer { frame, moving, release });
        }
        drop(s);
        self.wake.notify_one();
    }

    /// Pen samples (oldest first, at most 16) on `screen`.
    pub fn pen(&self, owner: u64, screen: i32, samples: Vec<crate::protocol::pointer::PenSample>) {
        use crate::protocol::pointer::{self as wire, PenPhase};
        let Some(last) = samples.last().cloned() else {
            return;
        };
        let mut s = self.lock();
        if s.relative || !Self::claim(&mut s, owner) {
            return;
        }
        let phase = PenPhase::try_from(last.phase).unwrap_or(PenPhase::Unspecified);
        let release = matches!(phase, PenPhase::Up | PenPhase::Leave | PenPhase::Cancel);
        s.pen = match phase {
            PenPhase::Leave | PenPhase::Cancel => None,
            _ => Some((
                screen,
                last.x,
                last.y,
                matches!(phase, PenPhase::Down | PenPhase::Move),
            )),
        };
        let frame = wire::encode_pen(&wire::Pen {
            screen_id: Some(screen),
            samples,
            time_us: 0,
        });
        Self::push(&mut s, InputEvent::Pointer {
            frame,
            moving: false,
            release,
        });
        drop(s);
        self.wake.notify_one();
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
                let keys: Vec<_> = s.remote_keys.iter().copied().collect();
                s.release_checkpoint.extend(keys.iter().copied());
                for modifier in [false, true] {
                    for &usage in &keys {
                        if keyboard::modifier(usage) == modifier {
                            s.queue.push_back(InputEvent::release(usage));
                        }
                    }
                }
                s.keys.clear();
                s.lock_model = None;
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
            "{}",
            t().features.remote_input_drag_released
        );
        s.drag_serial = s
            .drag_serial
            .checked_add(1)
            .ok_or_else(|| {
                anyhow::anyhow!("{}", t().features.remote_input_drag_serials_exhausted)
            })?;
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
            // Keep checkpoint repairs in the same release order as focus loss
            // (ordinary keys before modifiers), including when a modifier
            // release is already queued.
            for modifier in [false, true] {
                for &usage in &retired {
                    if keyboard::modifier(usage) != modifier
                        || s.queue
                            .iter()
                            .any(|event| event.usage() == Some((usage, false)))
                    {
                        continue;
                    }
                    s.remote_keys.insert(usage);
                    let event = InputEvent::release(usage);
                    let before_modifier = (!modifier)
                        .then(|| {
                            s.queue.iter().position(|event| {
                                event
                                    .usage()
                                    .is_some_and(|(key, down)| !down && keyboard::modifier(key))
                            })
                        })
                        .flatten();
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

    /// Both axes in one message, in 1/120 of a notch.
    pub fn wheel(&self, owner: u64, x: i32, y: i32) {
        if x == 0 && y == 0 {
            return;
        }
        let mut s = self.lock();
        if !Self::claim(&mut s, owner) {
            return;
        }
        Self::push(&mut s, InputEvent::Wheel { x, y });
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
                            s.error = Some((t().features.remote_input_throttle_timer_failed)(
                                &format!("{error:#}"),
                            ));
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
                    && let Some(mut event) = s.queue.pop_front()
                {
                    s.last_sent = Some(now);
                    // Tie the lock state to the presses already on the wire.
                    if let InputEvent::Key {
                        key: KeyInput::Locks { seq, .. },
                        ..
                    } = &mut event
                    {
                        *seq = s.lock_presses;
                    }
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
                    if let Some((usage, true)) = event.usage() {
                        s.remote_keys.insert(usage);
                        s.release_checkpoint.remove(&usage);
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
                // A newer frame of the same contacts replaces a pure move.
                (
                    InputEvent::Pointer {
                        frame,
                        moving: true,
                        ..
                    },
                    Some(InputEvent::Pointer {
                        frame: next,
                        moving: true,
                        ..
                    }),
                ) if frame.get(5) == next.get(5) => {
                    *frame = next.clone();
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
            bail!("{}", t().features.remote_input_relative_off);
        }
        if !guard() || !Self::claim(&mut s, owner) {
            bail!("{}", t().features.remote_input_control_unavailable);
        }
        if s.assists.len() >= 4 && !s.assists.contains_key(&lease) {
            bail!("{}", t().features.remote_input_too_many_sources);
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
        if result.is_ok() && event.event.lock_press() {
            // Counted even if the epoch changed meanwhile: it is on the wire.
            let mut s = self.lock();
            s.lock_presses = s.lock_presses.wrapping_add(1);
        }
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
                if let Some((usage, false)) = event.event.usage() {
                    s.remote_keys.remove(&usage);
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
                    if let Some((usage, false)) = event.event.usage() {
                        s.queue
                            .retain(|e| e.usage().is_none_or(|(queued, _)| queued != usage));
                    }
                }
                s.error = Some((t().features.remote_input_send_failed)(&error.to_string()));
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

    /// Wait until every queued event has been handed to transport (scripted
    /// input, `dev input`). False on timeout.
    pub(crate) async fn flushed(&self, timeout: std::time::Duration) -> bool {
        let drain = async {
            loop {
                let changed = self.drained.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                self.wake.notify_one();
                {
                    let s = self.lock();
                    if s.queue.is_empty() && !s.in_flight {
                        break;
                    }
                }
                changed.await;
            }
        };
        tokio::time::timeout(timeout, drain).await.is_ok()
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
mod tests {
    use super::*;

    const OWNER: u64 = 1;
    const A: u32 = keyboard::KEYBOARD | 0x04;
    const CAPS: u32 = keyboard::LOCKS[0];

    fn active() -> RemoteInput {
        let input = RemoteInput::default();
        input.set_keyboard_platform(farwing_protocol::Platform::Windows);
        input.set_ready(true);
        input.enable(MouseMode::Remote, false).unwrap();
        input.confirm_neutral(input.activation_generation());
        input
    }

    fn lock_states(input: &RemoteInput) -> Vec<[bool; 3]> {
        input
            .lock()
            .queue
            .iter()
            .filter_map(|event| match event {
                InputEvent::Key {
                    key: KeyInput::Locks { state, .. },
                    ..
                } => Some(*state),
                _ => None,
            })
            .collect()
    }

    async fn send(input: &RemoteInput) -> InputEvent {
        let InputWork::Event(event) = input.next().await else {
            panic!("unexpected recovery");
        };
        input.complete(&event, Ok(()));
        event.event
    }

    #[test]
    fn lock_state_only_for_changes_not_caused_by_forwarded_keys() {
        let input = active();
        // Activation sends the local state once.
        input.sync_locks(OWNER, [false, true, false]);
        input.sync_locks(OWNER, [false, true, false]);
        assert_eq!(lock_states(&input), [[false, true, false]]);
        // A forwarded Caps Lock toggles both machines.
        assert!(input.key(OWNER, CAPS, true));
        assert!(input.key(OWNER, CAPS, true)); // auto-repeat toggles nothing
        assert!(input.key(OWNER, CAPS, false));
        input.sync_locks(OWNER, [true, true, false]);
        assert_eq!(lock_states(&input).len(), 1);
        // Num Lock changed outside the forwarded stream.
        input.sync_locks(OWNER, [true, false, false]);
        assert_eq!(
            lock_states(&input),
            [[false, true, false], [true, false, false]]
        );
    }

    #[test]
    fn ownership_boundaries_resend_lock_state() {
        let input = active();
        input.sync_locks(OWNER, [false, false, false]);
        input.pause_owner(OWNER);
        input.sync_locks(OWNER, [false, false, false]);
        assert_eq!(lock_states(&input), [[false, false, false]]);
    }

    #[tokio::test]
    async fn lock_state_carries_presses_on_the_wire() {
        let input = active();
        input.sync_locks(OWNER, [false, false, false]);
        assert!(matches!(
            send(&input).await,
            InputEvent::Key {
                key: KeyInput::Locks { seq: 0, .. },
                ..
            }
        ));
        for down in [true, false] {
            assert!(input.key(OWNER, CAPS, down));
            send(&input).await;
        }
        input.sync_locks(OWNER, [false, false, false]);
        assert!(matches!(
            send(&input).await,
            InputEvent::Key {
                key: KeyInput::Locks {
                    state: [false, false, false],
                    seq: 1
                },
                ..
            }
        ));
    }

    #[tokio::test]
    async fn repeats_are_marked_and_releases_follow_commands() {
        let input = active();
        assert!(input.key(OWNER, A, true));
        assert!(input.key(OWNER, A, true));
        assert!(matches!(
            send(&input).await,
            InputEvent::Key {
                key: KeyInput::Usage { repeat: false, .. },
                down: true
            }
        ));
        assert!(matches!(
            send(&input).await,
            InputEvent::Key {
                key: KeyInput::Usage { repeat: true, .. },
                down: true
            }
        ));
        // A command first releases what the host may still hold.
        input.system_command(OWNER, SystemCommand::Lock).unwrap();
        assert_eq!(send(&input).await.usage(), Some((A, false)));
        assert!(matches!(
            send(&input).await,
            InputEvent::Key {
                key: KeyInput::Command(SystemCommand::Lock),
                down: true
            }
        ));
        // Keys from a non-physical page are rejected.
        assert!(!input.key(OWNER, 0x41, true));
    }

    #[tokio::test]
    async fn heartbeats_only_while_keys_are_held_and_quiet() {
        let input = active();
        input.heartbeat();
        assert!(input.lock().queue.is_empty());
        assert!(input.key(OWNER, A, true));
        send(&input).await;
        // The key frame itself kept the stream alive.
        input.heartbeat();
        assert!(input.lock().queue.is_empty());
        input.lock().last_sent = Some(Instant::now() - HEARTBEAT);
        input.heartbeat();
        input.heartbeat();
        assert!(matches!(send(&input).await, InputEvent::Heartbeat));
        assert!(input.lock().queue.is_empty());
    }

    #[test]
    fn wheel_carries_both_axes() {
        let input = active();
        input.wheel(OWNER, -120, 240);
        input.wheel(OWNER, 0, 0);
        let s = input.lock();
        assert_eq!(s.queue.len(), 1);
        assert!(matches!(s.queue[0], InputEvent::Wheel { x: -120, y: 240 }));
    }
}
