//! Host power actions (shutdown, restart), which only the controlled side
//! issues. macOS does not host that role in this port.
use anyhow::{Result, bail};

pub(crate) fn probe() -> Result<()> {
    bail!("macOS host role is not supported")
}

pub(crate) fn execute(_action: crate::features::host::power::Action) -> Result<()> {
    bail!("macOS host role is not supported")
}
