//! Account ownership and module boundaries.

pub mod api;
pub mod assist;
pub mod auth;
pub mod client;
pub(crate) mod device_change;
pub(crate) mod http;
pub mod login;
pub(crate) mod power;
pub(crate) mod records;
pub mod server;
pub(crate) mod server_changed;
pub(crate) mod session_restore;

pub(crate) mod feature_ability;
pub(crate) mod reporting;
