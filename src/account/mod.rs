//! Account ownership and module boundaries.

pub mod api;
pub mod assist;
pub mod auth;
pub mod client;
pub(crate) mod device_change;
pub mod login;
pub(crate) mod membership;
pub(crate) mod nrd_http;
pub(crate) mod power;
pub(crate) mod session_restore;

pub(crate) mod feature_ability;
pub(crate) mod reporting;
