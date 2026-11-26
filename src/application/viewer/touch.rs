//! Touchscreen contacts from winit, shared by every presenter.
//!
//! Each presenter intercepts `WindowEvent::Touch` after its fullscreen
//! controls and before egui, maps the contact onto the picture the way it maps
//! the pointer, and hands it here. What goes out (native frames or the
//! emulated button) is decided by `StreamControlHandle::touch`.

use crate::features::remote_input::{Phase, Spot};
use crate::features::stream_control::StreamControlHandle;

/// Forwards one contact. `spot` is where it lands on the video, or `None`
/// when it falls outside or a local UI owns input. Returns true when remote
/// input consumed it; false leaves it to egui.
pub(super) fn forward(
    control: &StreamControlHandle,
    owner: u64,
    touch: &winit::event::Touch,
    spot: Option<Spot>,
) -> bool {
    let phase = match touch.phase {
        winit::event::TouchPhase::Started => Phase::Start,
        winit::event::TouchPhase::Moved => Phase::Move,
        winit::event::TouchPhase::Ended => Phase::End,
        winit::event::TouchPhase::Cancelled => Phase::Cancel,
    };
    // winit ids are the platform's pointer, touch or slot ids, all 32-bit.
    let id = u32::try_from(touch.id).unwrap_or(u32::MAX);
    let pressure = touch.force.map_or(0.0, |force| force.normalized());
    control.touch(owner, phase, id, pressure, spot)
}
