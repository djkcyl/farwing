//! Features ownership and module boundaries.

pub(crate) mod chat;
#[cfg(windows)]
pub(crate) mod voice;
#[cfg(windows)]
pub(crate) mod clipboard;
#[cfg(windows)]
pub(crate) mod drag_drop;
#[cfg(windows)]
pub(crate) mod file_transfer;
#[cfg(windows)]
pub(crate) mod host;
pub(crate) mod network_control;
pub(crate) mod participants;
pub(crate) mod port_mapping;
pub(crate) mod remote_cursor;
pub(crate) mod remote_input;
#[cfg(windows)]
pub(crate) mod remote_upgrade;
pub mod stream_control;
pub(crate) mod viewing_settings;
