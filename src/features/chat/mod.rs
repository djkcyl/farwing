//! Chat in the device center (`docs/protocol/chat-design.md` §12.1).
#[cfg(windows)]
pub(crate) mod cache;
pub(crate) mod emoji;
pub(crate) mod list;
pub(crate) mod markdown;
pub(crate) mod service;
pub(crate) mod state;

pub(crate) use service::{ChatService, ImageKey};
#[cfg(windows)]
pub(crate) use service::{MicTest, ShareTarget};
