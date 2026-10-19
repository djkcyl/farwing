//! Estimate active input delivery per physical device, never from network packets.
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const WINDOW_MS: u32 = 250;
const MAX_DEVICES: usize = 8;
const IDLE: Duration = Duration::from_millis(100);

struct Window {
    message_start: u32,
    observed_start: Instant,
    reports: u32,
}
struct Device {
    last_message: u32,
    last_observed: Instant,
    window: Option<Window>,
    high_windows: u8,
    warned: bool,
}

#[derive(Default)]
pub(crate) struct Monitor {
    devices: BTreeMap<usize, Device>,
}
impl Monitor {
    pub fn interrupt(&mut self) {
        for device in self.devices.values_mut() {
            device.window = None;
            device.high_windows = 0;
        }
    }
    pub fn remove(&mut self, id: usize) {
        self.devices.remove(&id);
    }
    /// Called only for nonzero physical relative motion accepted by the viewer.
    /// MSG.time measures queue production; Instant also rejects fast backlog
    /// draining. Neither is claimed to be a sensor or USB hardware timestamp.
    pub fn observe(&mut self, id: usize, message_ms: u32, now: Instant) -> Option<u32> {
        if id == 0 {
            return None;
        }
        if !self.devices.contains_key(&id) && self.devices.len() == MAX_DEVICES {
            let oldest = self
                .devices
                .iter()
                .min_by_key(|(_, d)| d.last_observed)
                .map(|(&id, _)| id);
            if let Some(oldest) = oldest {
                self.devices.remove(&oldest);
            }
        }
        let device = self.devices.entry(id).or_insert(Device {
            last_message: message_ms,
            last_observed: now,
            window: None,
            high_windows: 0,
            warned: false,
        });
        let gap = message_ms.wrapping_sub(device.last_message);
        if gap > IDLE.as_millis() as u32 || now.duration_since(device.last_observed) > IDLE {
            device.window = None;
            device.high_windows = 0;
        }
        device.last_message = message_ms;
        device.last_observed = now;
        if device.warned {
            return None;
        }
        let Some(window) = device.window.as_mut() else {
            device.window = Some(Window {
                message_start: message_ms,
                observed_start: now,
                reports: 0,
            });
            return None;
        };
        window.reports = window.reports.saturating_add(1);
        let source_ms = message_ms.wrapping_sub(window.message_start);
        if source_ms < WINDOW_MS {
            return None;
        }
        let observed_ms = now.duration_since(window.observed_start).as_secs_f64() * 1000.0;
        let elapsed_ms = f64::from(source_ms).max(observed_ms);
        let hz = f64::from(window.reports) * 1000.0 / elapsed_ms;
        // Account for coarse message timestamps and both window boundaries.
        // A nominal 2000-Hz mouse must not warn because of rounding/jitter.
        let lower_bound =
            f64::from(window.reports.saturating_sub(2)) * 1000.0 / (elapsed_ms + 16.0);
        let stable = source_ms <= WINDOW_MS * 2
            && observed_ms >= f64::from(source_ms) * 0.8
            && observed_ms <= f64::from(source_ms) * 1.25;
        device.high_windows = if stable && lower_bound > 2000.0 {
            device.high_windows.saturating_add(1)
        } else {
            0
        };
        device.window = Some(Window {
            message_start: message_ms,
            observed_start: now,
            reports: 0,
        });
        if device.high_windows >= 4 {
            device.warned = true;
            Some(((hz / 100.0).round() as u32).saturating_mul(100))
        } else {
            None
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct Warning {
    pub hz: u32,
    pub until: Instant,
}
