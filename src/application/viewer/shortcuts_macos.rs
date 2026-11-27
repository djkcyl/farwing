//! macOS keeps the system shortcuts of its own; handing the keyboard to the
//! remote desktop in full will need a CGEventTap later
//! (shortcut: inert until then, Cmd-Tab and friends stay local).
pub(super) struct Shortcuts;

impl Shortcuts {
    pub(super) fn new(_window: &winit::window::Window) -> Option<Self> {
        // Returning None keeps `Player.shortcuts` empty, exactly like a
        // Linux session whose compositor offers no inhibitor.
        None
    }

    pub(super) fn update(
        &mut self,
        _gate: super::input_gate::InputGate,
        _context: &egui::Context,
        _intercepts: bool,
    ) {
    }
}
