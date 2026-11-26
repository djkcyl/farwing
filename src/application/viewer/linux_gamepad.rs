//! Focus-owned controller sampling for a Linux viewer.
use crate::features::remote_input::RemoteInput;
use crate::platform::linux::gamepad::{MAX_GAMEPADS, Poller};
use crate::protocol::gamepad::{GamepadEvent, GamepadState};
use std::time::{Duration, Instant};

const SAMPLE: Duration = Duration::from_millis(8);
const DISCOVER: Duration = Duration::from_millis(250);
const RETRY: Duration = Duration::from_secs(3);
const RUMBLE_REFRESH: Duration = Duration::from_millis(100);
const RUMBLE_DURATION: Duration = Duration::from_millis(250);

struct Slot {
    instance_id: i32,
    armed: bool,
    motors: (u16, u16),
    rumble_at: Instant,
    rumble_failed: bool,
}

impl Slot {
    fn new(instance_id: i32, now: Instant) -> Self {
        Self {
            instance_id,
            armed: false,
            motors: (0, 0),
            rumble_at: now,
            rumble_failed: false,
        }
    }

    /// A button held while focusing another device must not become a remote DOWN.
    fn sample(&mut self, state: GamepadState) -> Option<GamepadState> {
        if !self.armed {
            if !state.at_rest() {
                return None;
            }
            self.armed = true;
        }
        Some(state)
    }
}

pub(super) struct GamepadCapture {
    input: RemoteInput,
    owner: u64,
    generation: u64,
    poller: Option<Poller>,
    slots: [Option<Slot>; MAX_GAMEPADS],
    next: Option<Instant>,
    error: Option<String>,
}

impl GamepadCapture {
    pub(super) fn new(input: RemoteInput, owner: u64) -> Self {
        let generation = input.gamepad_epoch();
        Self {
            input,
            owner,
            generation,
            poller: None,
            slots: std::array::from_fn(|_| None),
            next: None,
            error: None,
        }
    }

    /// A wake deadline only; sampling does not request an egui repaint.
    pub(super) fn deadline(&self) -> Option<Instant> {
        self.next
    }

    /// `active` is the presenter's complete InputGate, not just window focus.
    pub(super) fn tick(&mut self, active: bool) {
        if !active {
            self.pause();
            return;
        }
        let now = Instant::now();
        let generation = self.input.gamepad_epoch();
        if generation != self.generation {
            // RemoteInput already retired the old epoch. Do not enqueue its
            // disconnect into a new owner's epoch; only release local hardware.
            self.reset_local();
            self.generation = generation;
        }
        if self.next.is_some_and(|next| now < next) {
            return;
        }
        if self.poller.is_none() {
            match Poller::new() {
                Ok(poller) => self.poller = Some(poller),
                Err(error) => {
                    self.failed(format!("{error:#}"), now);
                    return;
                }
            }
        }
        let snapshots = match self.poller.as_mut().unwrap().poll() {
            Ok(snapshots) => snapshots,
            Err(error) => {
                self.release_slots();
                self.failed(format!("{error:#}"), now);
                return;
            }
        };
        self.error = None;
        self.next = Some(
            now + if snapshots.is_empty() {
                DISCOVER
            } else {
                SAMPLE
            },
        );

        for index in 0..MAX_GAMEPADS {
            let current = snapshots
                .iter()
                .find(|snapshot| usize::from(snapshot.index) == index);
            if self.slots[index].as_ref().is_some_and(|slot| {
                current.is_none_or(|snapshot| snapshot.instance_id != slot.instance_id)
            }) {
                self.release_slot(index);
            }
        }
        for snapshot in snapshots {
            let index = usize::from(snapshot.index);
            if index >= MAX_GAMEPADS {
                continue;
            }
            if self.slots[index].is_none() {
                if !self.input.gamepad(
                    self.owner,
                    GamepadEvent::Connect {
                        index: snapshot.index,
                    },
                ) {
                    continue;
                }
                self.generation = self.input.gamepad_epoch();
                if !self.input.gamepad(
                    self.owner,
                    GamepadEvent::Update {
                        index: snapshot.index,
                        state: GamepadState::default(),
                    },
                ) {
                    self.input.gamepad(
                        self.owner,
                        GamepadEvent::Disconnect {
                            index: snapshot.index,
                        },
                    );
                    continue;
                }
                self.slots[index] = Some(Slot::new(snapshot.instance_id, now));
                tracing::info!(index, controller = %snapshot.name, "game controller connected");
            }
            let state = self.slots[index].as_mut().unwrap().sample(snapshot.state);
            if let Some(state) = state
                && !self.input.gamepad(
                    self.owner,
                    GamepadEvent::Update {
                        index: snapshot.index,
                        state,
                    },
                )
            {
                // A recovery or owner change may have happened after sampling.
                // Re-enter through a fresh neutral gate instead of replaying DOWN.
                self.release_slots();
                self.reset_local();
                self.next = Some(now + DISCOVER);
                self.generation = self.input.gamepad_epoch();
                return;
            }
            self.feedback(index, now);
        }
        self.generation = self.input.gamepad_epoch();
    }

    fn feedback(&mut self, index: usize, now: Instant) {
        let Some(slot) = &mut self.slots[index] else {
            return;
        };
        let motors = self
            .input
            .gamepad_vibration(self.owner, index as u8)
            .unwrap_or((0, 0));
        if slot.rumble_failed
            || (motors == slot.motors && (motors == (0, 0) || now < slot.rumble_at))
        {
            return;
        }
        let poller = self.poller.as_mut().unwrap();
        if !poller.has_rumble(index as u8) {
            slot.rumble_failed = true;
            tracing::debug!(index, "game controller does not expose rumble");
            return;
        }
        match poller.rumble(index as u8, motors.0, motors.1, RUMBLE_DURATION) {
            Ok(()) => {
                slot.motors = motors;
                slot.rumble_at = now + RUMBLE_REFRESH;
            }
            Err(error) => {
                slot.rumble_failed = true;
                tracing::debug!(index, error = %error, "game controller rumble unavailable");
            }
        }
    }

    fn release_slot(&mut self, index: usize) {
        if self.slots[index].take().is_some() {
            self.input.gamepad(
                self.owner,
                GamepadEvent::Update {
                    index: index as u8,
                    state: GamepadState::default(),
                },
            );
            self.input
                .gamepad(self.owner, GamepadEvent::Disconnect { index: index as u8 });
        }
    }

    fn release_slots(&mut self) {
        for index in 0..MAX_GAMEPADS {
            self.release_slot(index);
        }
    }

    fn reset_local(&mut self) {
        if let Some(poller) = &mut self.poller {
            poller.stop_rumble();
        }
        self.poller = None;
        self.slots = std::array::from_fn(|_| None);
        self.next = None;
    }

    fn failed(&mut self, message: String, now: Instant) {
        if self.error.as_deref() != Some(message.as_str()) {
            tracing::warn!(error = %message, "game controller sampling unavailable; retrying later");
            self.error = Some(message);
        }
        self.reset_local();
        self.next = Some(now + RETRY);
    }

    pub(super) fn pause(&mut self) {
        self.release_slots();
        self.reset_local();
        self.generation = self.input.gamepad_epoch();
    }
}

impl Drop for GamepadCapture {
    fn drop(&mut self) {
        self.pause();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_control_must_return_to_neutral_before_activation() {
        let mut slot = Slot::new(10, Instant::now());
        let held = GamepadState {
            buttons: 0x1000,
            ..Default::default()
        };
        assert_eq!(slot.sample(held), None);
        assert_eq!(
            slot.sample(GamepadState {
                left_trigger: 255,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            slot.sample(GamepadState {
                right_thumb_x: i16::MAX,
                ..Default::default()
            }),
            None
        );
        assert_eq!(
            slot.sample(GamepadState::default()),
            Some(GamepadState::default())
        );
        assert_eq!(slot.sample(held), Some(held));
    }

    #[test]
    fn a_replacement_device_cannot_inherit_the_previous_neutral_gate() {
        let mut old = Slot::new(10, Instant::now());
        assert!(old.sample(GamepadState::default()).is_some());
        let mut replacement = Slot::new(11, Instant::now());
        assert_ne!(old.instance_id, replacement.instance_id);
        assert_eq!(
            replacement.sample(GamepadState {
                buttons: 1,
                ..Default::default()
            }),
            None
        );
        assert!(replacement.sample(GamepadState::default()).is_some());
    }
}
