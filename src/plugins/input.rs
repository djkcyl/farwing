//! Supplemental input worker. Physical input keeps the window's ownership.
use super::{
    hotkeys::ControlGate,
    process::{Shared, lock},
    sdk,
};
use crate::features::remote_input::{CorrectionBasis, RemoteInput};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::{Duration, Instant},
};
pub(super) struct Lease {
    alive: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Lease {
    pub fn start(
        input: RemoteInput,
        owner: u64,
        source: Arc<Shared>,
        gate: Arc<ControlGate>,
        control_id: u64,
    ) -> anyhow::Result<Self> {
        let alive = Arc::new(AtomicBool::new(true));
        let active = alive.clone();
        let worker = std::thread::Builder::new()
            .name("Plugin correction".into())
            .spawn(move || {
                let generation = source.generation.load(Ordering::Acquire);
                let mut state = (false, 0);
                let mut token = None;
                let mut sequence = 0;
                let mut handled_trigger = 0;
                let mut release_at = None;
                let mut ack_after_release = None;
                let mut last_frame = Instant::now();
                while active.load(Ordering::Acquire)
                    && source.ready.load(Ordering::Acquire)
                    && source.generation.load(Ordering::Acquire) == generation
                {
                    let now = Instant::now();
                    let mut next = gate.snapshot();
                    if gate.is_trigger() && next.1 == handled_trigger {
                        next.0 = false;
                    }
                    if state != next {
                        if let Some(t) = token.take() {
                            input.end_assist(t);
                        }
                        release_at = None;
                        ack_after_release = None;
                        state = next;
                        sequence = 0;
                        last_frame = now;
                        if state.0 {
                            let t = u64::from_le_bytes(
                                uuid::Uuid::new_v4().as_bytes()[..8]
                                    .try_into()
                                    .expect("uuid"),
                            );
                            let lease_gate = gate.clone();
                            let epoch = state.1;
                            let valid = Arc::new(move || {
                                lease_gate.snapshot() == (true, epoch) && unsafe {
                                    windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow()
                                }
                                .0
                                    as u64
                                    == owner
                            });
                            if input.begin_assist(owner, t, valid).is_ok() {
                                token = Some(t);
                            } else {
                                gate.reset_epoch(state.1);
                            }
                        }
                        source.repaint();
                    }
                    if let Some(t) = token {
                        if !input.assist_active(owner, t) {
                            gate.reset_epoch(state.1);
                            continue;
                        }
                        if release_at.is_some_and(|at| now >= at) {
                            input.assist_button(owner, t, false);
                            release_at = None;
                        }
                        let sample = lock(&source.input)
                            .get(&control_id)
                            .filter(|s| {
                                s.sequence != sequence
                                    && s.generation == generation
                                    && s.control_epoch == state.1
                            })
                            .cloned();
                        if let Some(sample) = sample {
                            sequence = sample.sequence;
                            last_frame = now;
                            if sample.at.elapsed() <= Duration::from_millis(120)
                                && gate.snapshot() == state
                            {
                                for command in sample.commands {
                                    if gate.snapshot() != state {
                                        break;
                                    }
                                    let accepted = match command {
                                        sdk::InputCommand::Relative { x, y } => input
                                            .assist_correction(
                                                owner,
                                                t,
                                                [x, y],
                                                CorrectionBasis {
                                                    physical: sample.motion,
                                                    submitted_corrections: None,
                                                },
                                                sample.at,
                                                [1.0; 2],
                                            ),
                                        sdk::InputCommand::Correction {
                                            x,
                                            y,
                                            weight,
                                            at_dispatch,
                                        } => input.assist_correction(
                                            owner,
                                            t,
                                            [x, y],
                                            CorrectionBasis {
                                                physical: if at_dispatch {
                                                    sample.dispatch_motion
                                                } else {
                                                    sample.motion
                                                },
                                                submitted_corrections: at_dispatch
                                                    .then_some(sample.dispatch_corrections),
                                            },
                                            sample.at,
                                            weight,
                                        ),
                                        sdk::InputCommand::Click { hold_ms } => {
                                            if release_at.is_some() {
                                                continue;
                                            }
                                            release_at = Some(
                                                now + Duration::from_millis(u64::from(hold_ms)),
                                            );
                                            input.assist_button(owner, t, true)
                                        }
                                    };
                                    if !accepted {
                                        gate.reset_epoch(state.1);
                                        break;
                                    }
                                }
                                if gate.is_trigger() {
                                    ack_after_release = Some(state.1);
                                }
                            }
                        }
                        if release_at.is_none()
                            && !input.assist_pending(t)
                            && let Some(epoch) = ack_after_release.take()
                        {
                            handled_trigger = epoch;
                        }
                        if now.duration_since(last_frame) > Duration::from_millis(750) {
                            gate.reset_epoch(state.1);
                        }
                    }
                    std::thread::park_timeout(Duration::from_millis(4));
                }
                if let Some(t) = token {
                    input.end_assist(t);
                }
                gate.reset();
                active.store(false, Ordering::Release);
                source.repaint();
            })?;
        Ok(Self {
            alive,
            worker: Some(worker),
        })
    }
    pub fn active(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
        if let Some(w) = self.worker.take() {
            w.thread().unpark();
            let _ = w.join();
        }
    }
}
