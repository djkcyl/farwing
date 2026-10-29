//! Local and service execution share the same engine and ownership contract.
use super::{
    broker::Remote,
    engine::{Engine, Geometry},
    wire::Event,
};
use crate::i18n::t;
use anyhow::Result;

/// The service's input agent (`broker`), or this process's own unprivileged
/// engine when no service runs (no secure desktop, no Ctrl+Alt+Del).
pub(super) enum Backend {
    Local(Engine),
    Service(Remote),
}
impl Backend {
    /// The background host, like the portable host, injects through the
    /// service's input agent.
    pub fn new(require_service: bool, permitted: impl Fn() -> bool) -> Result<Self> {
        Ok(match Remote::connect(permitted)? {
            Some(remote) => Self::Service(remote),
            None => {
                anyhow::ensure!(
                    !require_service,
                    "{}",
                    t().features.input_service_not_restored
                );
                Self::Local(Engine::new(false, std::process::id(), false)?)
            }
        })
    }
    pub fn service(&self) -> bool {
        matches!(self, Self::Service(_))
    }
    pub fn healthy(&self) -> bool {
        match self {
            Self::Local(_) => true,
            Self::Service(remote) => remote.healthy(),
        }
    }
    pub fn backend(&self) -> &str {
        match self {
            Self::Local(e) => e.backend(),
            Self::Service(e) => e.backend(),
        }
    }
    /// `hold` is the owner's held-key timeout.
    pub fn apply(
        &mut self,
        events: Vec<Event>,
        geometry: Geometry,
        hold: std::time::Duration,
        permitted: impl Fn() -> bool,
    ) -> Result<()> {
        match self {
            Self::Local(e) => {
                e.hold_timeout(hold);
                for event in events {
                    e.apply(event, geometry.clone(), &permitted)?;
                    anyhow::ensure!(!e.take_sas(), "{}", t().features.input_sas_needs_service);
                }
                Ok(())
            }
            Self::Service(e) => e.apply(events, geometry, hold, permitted),
        }
    }
    pub fn release(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.release(),
            Self::Service(e) => e.release(),
        }
    }
    pub fn synchronize(&mut self, geometry: Geometry) -> Result<()> {
        match self {
            Self::Local(e) => e.synchronize(geometry),
            Self::Service(e) => e.synchronize(geometry),
        }
    }
    pub fn tick(&mut self) -> Result<()> {
        match self {
            Self::Local(e) => e.tick(),
            Self::Service(e) => e.tick(),
        }
    }
}
