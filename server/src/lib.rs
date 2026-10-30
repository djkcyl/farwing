//! Farwing Server: self-hosted control plane, realtime signaling and TURN
//! credentials for Farwing (Farwing Protocol v2, see
//! `docs/protocol/farwing-protocol.md` and `docs/protocol/v2-design.md`).

pub mod accounts;
pub mod acme;
pub mod assist;
pub mod config;
pub mod crypto;
pub mod db;
pub mod edge;
pub mod edge_node;
pub mod egress;
pub mod error;
pub mod forwarding;
pub mod http;
pub mod hub;
pub mod notify;
pub mod ice;
pub mod ratelimit;
pub mod records;
pub mod runtime;
pub mod server;
pub mod settings;
pub mod sfu;
pub mod setup;
pub mod stun_server;
pub mod tls;
pub mod totp;
pub mod turn_server;
pub mod util;
pub mod webdb;
pub mod ws;

pub use config::Config;
pub use server::{RunningServer, serve, start};
