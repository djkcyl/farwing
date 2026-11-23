//! Session ownership and module boundaries.

pub mod controller;
pub(crate) mod device_session;
#[cfg(windows)]
pub(crate) mod host_client;
#[cfg(windows)]
pub(crate) mod presence;
pub(crate) mod reconnect;
#[cfg(windows)]
pub(crate) mod startup;

pub(crate) mod negotiation;
