//! macOS rides the same presenter loop as Linux; gamepad capture has no
//! macOS backend yet, so every entry point is inert.
use crate::features::remote_input::RemoteInput;
use std::time::Instant;

pub(super) struct GamepadCapture;

impl GamepadCapture {
    pub(super) fn new(_input: RemoteInput, _owner: u64) -> Self {
        Self
    }

    pub(super) fn deadline(&self) -> Option<Instant> {
        None
    }

    pub(super) fn tick(&mut self, _active: bool) {}

    pub(super) fn pause(&mut self) {}
}
