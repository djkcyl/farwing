//! Farwing Server: self-hosted control plane, realtime signaling and TURN
//! credentials for Farwing (Farwing Protocol v2, see
//! `docs/protocol/farwing-protocol.md` and `docs/protocol/v2-design.md`).

pub mod acme;
pub mod admin;
pub mod assist;
pub mod config;
pub mod crypto;
pub mod db;
pub mod error;
pub mod http;
pub mod hub;
pub mod ice;
pub mod ratelimit;
pub mod server;
pub mod settings;
pub mod stun_server;
pub mod tls;
pub mod totp;
pub mod turn_server;
pub mod util;
pub mod webdb;
pub mod ws;

pub use config::Config;
pub use server::{RunningServer, serve, start};
