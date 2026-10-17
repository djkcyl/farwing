//! When a viewer window takes the local keyboard and pointer for the remote.
//!
//! Every presenter asks the same question before it captures the pointer or
//! keeps the desktop's own shortcuts away from it, so the answer lives here:
//! a platform only supplies the facts it alone can observe (focus, its menus,
//! whether a picture has arrived) and the mechanism that acts on the result.

use crate::features::remote_input::RemoteInput;

/// The window-level facts the gate needs. Each presenter fills these from its
/// own window and menus; everything egui knows is read from the context.
#[derive(Clone, Copy, Debug)]
pub(super) struct InputGate {
    /// The window is the one with the keyboard.
    pub focused: bool,
    pub minimized: bool,
    /// The remote is being controlled rather than only watched.
    pub controlling: bool,
    /// A file drag owns the pointer.
    pub dragging: bool,
    /// One of the presenter's own menus, tabs or transitions is up.
    pub menu_open: bool,
    /// A picture has arrived, so there is something to aim input at.
    pub has_video: bool,
}

impl InputGate {
    /// Whether the window owns local input right now.
    pub(super) fn holds(self, context: &egui::Context) -> bool {
        self.focused && self.holds_unless_unfocused(context)
    }

    /// The gate with focus left out, for mechanisms the desktop already scopes
    /// to the focused window (a Wayland shortcut inhibitor).
    pub(super) fn holds_unless_unfocused(self, context: &egui::Context) -> bool {
        !self.minimized
            && self.controlling
            && !self.dragging
            && !self.menu_open
            && !context.any_popup_open()
            && !context.memory(|memory| memory.top_modal_layer().is_some())
            && !context.text_edit_focused()
            && self.has_video
    }
}

/// Whether keys a window owns should be kept from the local desktop's own
/// shortcuts: the device's "intercept shortcuts" setting, a remote that takes
/// keyboard input, and no shortcut recorder or text field claiming the keys.
pub(super) fn intercepts_shortcuts(input: &RemoteInput, setting: bool) -> bool {
    setting && input.keyboard_supported() && !crate::application::viewer_shortcuts::suspended()
}
