//! The macOS input engine: the host role is not supported on macOS, so the
//! engine never opens and every operation refuses. It keeps the ownership
//! contract's shape the shared backend compiles against.
use super::geometry::Geometry;
use super::wire::Event;
use anyhow::{Result, bail};

const UNSUPPORTED: &str = "macOS host role is not supported";

pub(crate) struct Engine;

impl Engine {
    pub fn new(
        _hardware: bool,
        _policy: super::wire::Policy,
        _origin: u32,
        _privileged: bool,
    ) -> Result<Self> {
        bail!(UNSUPPORTED)
    }
    pub fn backend(&self) -> &'static str {
        "unsupported"
    }
    pub fn take_sas(&mut self) -> bool {
        false
    }
    pub fn configure(&mut self, _configuration: super::config::Configuration) -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn mouse_policy(&self) -> super::config::MousePolicy {
        Default::default()
    }
    pub fn synchronize(&mut self, _geometry: Geometry) -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn apply(
        &mut self,
        _event: Event,
        _geometry: Geometry,
        _permitted: impl Fn() -> bool,
    ) -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn tick(&mut self) -> Result<()> {
        bail!(UNSUPPORTED)
    }
    pub fn release(&mut self) -> Result<()> {
        bail!(UNSUPPORTED)
    }
}
