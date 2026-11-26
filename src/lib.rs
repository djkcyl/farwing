//! OpenUUYC: UU protocol interoperability and Windows application composition.

pub const APP_NAME: &str = "OpenUUYC";
pub(crate) const VIEWER_TITLE_PREFIX: &str = "OpenUUYC — ";

pub mod account;
pub mod application;
pub mod diagnostics;
pub mod features;
mod lifecycle;
pub mod media;
pub mod protocol;
pub mod session;
pub mod transport;

pub mod plugins;
mod ui;

mod platform;
