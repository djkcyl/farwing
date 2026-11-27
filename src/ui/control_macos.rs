//! macOS single-instance activation stub. The real distributed-notification
//! channel is a later phase; both entry points just report unwired.
use anyhow::{Result, bail};

pub(crate) fn activate() -> Result<()> {
    bail!("macOS control: not yet wired")
}

pub(crate) fn deliver_link(_uri: &str) -> Result<()> {
    bail!("macOS control: not yet wired")
}
