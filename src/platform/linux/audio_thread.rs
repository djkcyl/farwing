//! Per-thread setup for the audio output worker. Windows enters the COM
//! multithreaded apartment here; PulseAudio and ALSA keep no per-thread
//! runtime, so on Linux there is nothing to enter.
use std::convert::Infallible;

pub(crate) fn initialize() -> Result<(), Infallible> {
    Ok(())
}

pub(crate) fn description() -> String {
    "none (Linux has no COM)".to_owned()
}
