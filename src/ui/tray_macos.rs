//! macOS menu-bar tray stub. The real NSStatusItem integration is a later
//! phase; creation fails and the caller keeps ordinary close behaviour.
use anyhow::{Result, bail};

pub(super) struct Tray;

impl Tray {
    pub fn new() -> Result<Self> {
        bail!("macOS tray: not yet wired")
    }
    pub(super) fn set_controlled(&self, _controlled: bool) {}
}
