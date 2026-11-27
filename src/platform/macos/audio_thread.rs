//! Per-thread setup for the audio output worker. CoreAudio keeps no
//! per-thread runtime, so there is nothing to enter.
use std::convert::Infallible;

pub(crate) fn initialize() -> Result<(), Infallible> {
    Ok(())
}

pub(crate) fn description() -> String {
    "none (CoreAudio has no COM)".to_owned()
}
