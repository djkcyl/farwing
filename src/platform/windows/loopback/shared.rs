//! One loopback capture per playback endpoint, shared by every participant
//! of every session: each block goes to all subscribers, which keep their own
//! clocks and encoders (`docs/protocol/v2-design.md` §4.2).
use super::{Capture, Endpoint, Samples};
use anyhow::Result;
use std::sync::{Arc, Mutex, MutexGuard, Weak};

struct Hub {
    endpoint: String,
    /// The process tree left out (the device center's calls, §9.4).
    excluded: Option<u32>,
    subscribers: Mutex<Vec<(u64, Samples)>>,
    capture: Mutex<Option<Source>>,
}

/// What a hub captures.
enum Source {
    Endpoint(Capture),
    /// Everything the processes but one tree play.
    Processes(crate::platform::windows::process_loopback::Capture),
}

impl Source {
    fn poll(&mut self) -> Result<()> {
        match self {
            Self::Endpoint(capture) => capture.poll(),
            Self::Processes(capture) if capture.alive() => Ok(()),
            Self::Processes(_) => anyhow::bail!("{}", crate::i18n::t().system.loopback_closed),
        }
    }

    fn producing(&self) -> bool {
        match self {
            Self::Endpoint(capture) => capture.producing(),
            Self::Processes(capture) => capture.alive(),
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|p| p.into_inner())
}

static HUBS: Mutex<Vec<Weak<Hub>>> = Mutex::new(Vec::new());
static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);

impl Hub {
    fn healthy(&self) -> bool {
        lock(&self.capture).as_mut().is_some_and(|c| c.poll().is_ok())
    }
}

/// A participant's share of an endpoint's capture; leaves on drop, and the
/// last one closes the capture.
pub(crate) struct Subscription {
    id: u64,
    hub: Arc<Hub>,
}

/// Receive `samples` from `endpoint`'s capture, opening it if this is the
/// first subscriber (or the shared one failed); with `excluded`, what every
/// process but that tree plays instead (process loopback, §9.4).
pub(crate) fn subscribe(endpoint: &Endpoint, excluded: Option<u32>, samples: Samples) -> Result<Subscription> {
    let id = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let mut hubs = lock(&HUBS);
    hubs.retain(|hub| hub.strong_count() > 0);
    if let Some(hub) = hubs
        .iter()
        .filter_map(Weak::upgrade)
        .find(|hub| hub.endpoint == endpoint.id && hub.excluded == excluded && hub.healthy())
    {
        lock(&hub.subscribers).push((id, samples));
        tracing::debug!(subscribers = lock(&hub.subscribers).len(), "desktop audio capture shared");
        return Ok(Subscription { id, hub });
    }
    let hub = Arc::new(Hub {
        endpoint: endpoint.id.clone(),
        excluded,
        subscribers: Mutex::new(vec![(id, samples)]),
        capture: Mutex::new(None),
    });
    let weak = Arc::downgrade(&hub);
    let deliver: Samples = Arc::new(move |block, at| {
        let Some(hub) = weak.upgrade() else {
            return;
        };
        // Callbacks are cheap queue pushes; clone the list out of the lock.
        let subscribers: Vec<Samples> = lock(&hub.subscribers).iter().map(|(_, s)| s.clone()).collect();
        for subscriber in subscribers {
            subscriber(block, at);
        }
    });
    let capture = match excluded {
        Some(pid) => {
            use crate::platform::windows::process_loopback::{Capture as Processes, Target};
            tracing::info!(pid, "desktop audio leaves out the device center");
            Source::Processes(Processes::start(Target::AllBut(pid), move |block| {
                deliver(*block, std::time::Instant::now())
            })?)
        }
        None => Source::Endpoint(super::open(endpoint, deliver)?),
    };
    *lock(&hub.capture) = Some(capture);
    hubs.push(Arc::downgrade(&hub));
    Ok(Subscription { id, hub })
}

impl Subscription {
    pub(crate) fn producing(&self) -> bool {
        lock(&self.hub.capture).as_ref().is_some_and(Source::producing)
    }

    /// The shared capture's failure, seen by every subscriber.
    pub(crate) fn poll(&mut self) -> Result<()> {
        match lock(&self.hub.capture).as_mut() {
            Some(capture) => capture.poll(),
            None => anyhow::bail!("{}", crate::i18n::t().system.loopback_closed),
        }
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        // Serialized with `subscribe`, so nobody joins a closing capture.
        let _hubs = lock(&HUBS);
        let empty = {
            let mut subscribers = lock(&self.hub.subscribers);
            subscribers.retain(|(id, _)| *id != self.id);
            subscribers.is_empty()
        };
        if empty {
            // Close now rather than when the last reference goes: a new
            // subscriber opens a fresh capture.
            let capture = lock(&self.hub.capture).take();
            drop(capture);
        }
    }
}
