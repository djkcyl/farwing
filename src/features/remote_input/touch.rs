//! Touchscreen forwarding state machine.
//!
//! winit reports one contact per event while the wire protocol carries full
//! contact-set snapshots, and its phases map onto the protocol's `kind` with
//! one asymmetry: only the first finger of a gesture opens with kind 1, every
//! later contact change is a kind 2 snapshot the host diffs against its own
//! active set (new ids inject DOWN, vanished ids inject UP).
//!
//! A gesture the host cannot take natively (an official host, or a screen
//! other than the host's current one: touch frames carry no screen) is
//! emulated instead: its first contact drives the left button, the way the
//! host itself emulates touch without a touchscreen.
use std::collections::BTreeMap;

use crate::features::host::input::wire::{MAX_TOUCH_POINTS, Point};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Phase {
    Start,
    Move,
    End,
    Cancel,
}

/// A contact on the video: the remote screen and its normalized position,
/// as pointer input maps it.
pub(crate) type Spot = (i32, f64, f64);

/// What one contact event becomes.
#[derive(Clone, PartialEq, Debug)]
pub(crate) enum Step {
    /// The local UI's contact; the event must reach egui untouched.
    Local,
    /// Consumed with nothing to send: a further finger of an emulated
    /// gesture, or one past the host's limit.
    Held,
    /// A native frame.
    Frame(u8, Vec<Point>),
    /// The emulated pointer moves to `at` (if known), then presses or
    /// releases the left button (if set).
    Pointer {
        at: Option<Spot>,
        button: Option<bool>,
    },
}

/// One tracked contact: normalized position plus pressure, the only property
/// winit carries besides the id. The wire's radius/angle stay zero.
#[derive(Clone, Copy, PartialEq, Debug)]
struct Contact {
    x: f64,
    y: f64,
    pressure: f64,
}

#[derive(Clone, Default)]
pub(crate) struct Forwarder {
    active: BTreeMap<u32, Contact>,
    /// The contact an emulated gesture follows.
    pointer: Option<u32>,
}

impl Forwarder {
    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.active.is_empty() && self.pointer.is_none()
    }

    /// Turns one winit contact event into what goes out. `spot` is the
    /// contact on the video, or `None` when it falls outside; `native` says
    /// whether a gesture opened by this contact goes out as native touch.
    pub(crate) fn step(
        &mut self,
        phase: Phase,
        id: u32,
        pressure: f64,
        spot: Option<Spot>,
        native: bool,
    ) -> Step {
        // The host refuses a whole frame with a pressure outside 0..=1.
        let pressure = if pressure.is_finite() {
            pressure.clamp(0.0, 1.0)
        } else {
            0.0
        };
        if self.pointer == Some(id) {
            return match phase {
                // A contact that slid onto the local UI leaves the pointer
                // where it last was.
                Phase::Start | Phase::Move => match spot {
                    Some(at) => Step::Pointer {
                        at: Some(at),
                        button: None,
                    },
                    None => Step::Held,
                },
                Phase::End | Phase::Cancel => {
                    self.pointer = None;
                    Step::Pointer {
                        at: spot,
                        button: Some(false),
                    }
                }
            };
        }
        if !self.active.contains_key(&id) {
            // A new contact on the local UI never joins the gesture, and an
            // untracked one moving or lifting is the local UI's as well. After
            // a flush (mode switch, focus loss) the OS still delivers the
            // gesture's tail: contacts already lifted remotely must not come
            // back through a stray move.
            let (Phase::Start, Some(at)) = (phase, spot) else {
                return Step::Local;
            };
            if self.pointer.is_some() || self.active.len() >= MAX_TOUCH_POINTS {
                return Step::Held;
            }
            if self.active.is_empty() && !native {
                self.pointer = Some(id);
                return Step::Pointer {
                    at: Some(at),
                    button: Some(true),
                };
            }
            let kind = if self.active.is_empty() { 1 } else { 2 };
            self.active.insert(
                id,
                Contact {
                    x: at.1,
                    y: at.2,
                    pressure,
                },
            );
            return Step::Frame(kind, self.snapshot());
        }
        match phase {
            Phase::Start | Phase::Move => {
                let held = self.active.get_mut(&id).expect("tracked above");
                held.pressure = pressure;
                // Off the video the remote finger rests at its last position
                // until it returns or lifts.
                if let Some((_, x, y)) = spot {
                    (held.x, held.y) = (x, y);
                }
                Step::Frame(2, self.snapshot())
            }
            Phase::End => self.lift(id, 3),
            Phase::Cancel => self.lift(id, 4),
        }
    }

    /// Lifts everything, for ownership boundaries. Returns the native frame
    /// that lifts the remote contacts; an emulated pointer is released with
    /// the other held buttons.
    pub(crate) fn clear(&mut self) -> Option<(u8, Vec<Point>)> {
        self.pointer = None;
        if self.active.is_empty() {
            return None;
        }
        self.active.clear();
        Some((3, Vec::new()))
    }

    fn lift(&mut self, id: u32, last_kind: u8) -> Step {
        self.active.remove(&id);
        if self.active.is_empty() {
            Step::Frame(last_kind, Vec::new())
        } else {
            Step::Frame(2, self.snapshot())
        }
    }

    fn snapshot(&self) -> Vec<Point> {
        self.active
            .iter()
            .map(|(id, contact)| Point {
                id: *id,
                x: contact.x,
                y: contact.y,
                radius_x: 0.0,
                radius_y: 0.0,
                angle: 0.0,
                pressure: contact.pressure,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(kind: u8, contacts: &[(u32, f64, f64)]) -> Step {
        Step::Frame(
            kind,
            contacts
                .iter()
                .map(|(id, x, y)| Point {
                    id: *id,
                    x: *x,
                    y: *y,
                    radius_x: 0.0,
                    radius_y: 0.0,
                    angle: 0.0,
                    pressure: 0.0,
                })
                .collect(),
        )
    }

    fn native(f: &mut Forwarder, phase: Phase, id: u32, spot: Option<(f64, f64)>) -> Step {
        f.step(phase, id, 0.0, spot.map(|(x, y)| (0, x, y)), true)
    }

    #[test]
    fn first_finger_opens_second_joins() {
        let mut f = Forwarder::default();
        assert_eq!(
            native(&mut f, Phase::Start, 2, Some((0.25, 0.5))),
            frame(1, &[(2, 0.25, 0.5)])
        );
        assert_eq!(
            native(&mut f, Phase::Start, 5, Some((0.75, 0.5))),
            frame(2, &[(2, 0.25, 0.5), (5, 0.75, 0.5)])
        );
        assert_eq!(
            native(&mut f, Phase::Move, 2, Some((0.3, 0.6))),
            frame(2, &[(2, 0.3, 0.6), (5, 0.75, 0.5)])
        );
        // One finger lifts, the other stays: a snapshot without it.
        assert_eq!(
            native(&mut f, Phase::End, 2, Some((0.3, 0.6))),
            frame(2, &[(5, 0.75, 0.5)])
        );
        // The last lift reports it for the whole set.
        assert_eq!(native(&mut f, Phase::End, 5, None), frame(3, &[]));
        assert!(f.is_empty());
    }

    #[test]
    fn last_cancel_reports_cancel() {
        let mut f = Forwarder::default();
        native(&mut f, Phase::Start, 1, Some((0.5, 0.5)));
        assert_eq!(native(&mut f, Phase::Cancel, 1, None), frame(4, &[]));
    }

    #[test]
    fn local_ui_contacts_never_join() {
        let mut f = Forwarder::default();
        // Starts outside the video: local UI, not consumed.
        assert_eq!(native(&mut f, Phase::Start, 9, None), Step::Local);
        // Its later events are the local UI's as well.
        assert_eq!(
            native(&mut f, Phase::Move, 9, Some((0.5, 0.5))),
            Step::Local
        );
        assert_eq!(native(&mut f, Phase::End, 9, None), Step::Local);
    }

    #[test]
    fn tracked_contact_sliding_onto_ui_keeps_last_position() {
        let mut f = Forwarder::default();
        native(&mut f, Phase::Start, 1, Some((0.4, 0.4)));
        assert_eq!(
            native(&mut f, Phase::Move, 1, None),
            frame(2, &[(1, 0.4, 0.4)])
        );
        // Back on the video it tracks again.
        assert_eq!(
            native(&mut f, Phase::Move, 1, Some((0.6, 0.6))),
            frame(2, &[(1, 0.6, 0.6)])
        );
    }

    #[test]
    fn flush_suppresses_the_gesture_tail() {
        let mut f = Forwarder::default();
        native(&mut f, Phase::Start, 1, Some((0.4, 0.4)));
        assert_eq!(f.clear(), Some((3, Vec::new())));
        assert_eq!(f.clear(), None);
        // A stray move after the flush must not resurrect the contact.
        assert_eq!(
            native(&mut f, Phase::Move, 1, Some((0.5, 0.5))),
            Step::Local
        );
        assert_eq!(native(&mut f, Phase::End, 1, None), Step::Local);
    }

    #[test]
    fn contacts_past_the_host_limit_are_held() {
        let mut f = Forwarder::default();
        for id in 0..MAX_TOUCH_POINTS as u32 {
            assert!(matches!(
                native(&mut f, Phase::Start, id, Some((0.5, 0.5))),
                Step::Frame(..)
            ));
        }
        assert_eq!(
            native(&mut f, Phase::Start, 99, Some((0.5, 0.5))),
            Step::Held
        );
        assert_eq!(
            native(&mut f, Phase::Move, 99, Some((0.5, 0.5))),
            Step::Local
        );
    }

    #[test]
    fn pressure_stays_in_the_wire_range() {
        let mut f = Forwarder::default();
        let Step::Frame(_, points) = f.step(Phase::Start, 1, 3.5, Some((0, 0.5, 0.5)), true) else {
            panic!("expected a frame");
        };
        assert_eq!(points[0].pressure, 1.0);
        let Step::Frame(_, points) = f.step(Phase::Move, 1, f64::NAN, Some((0, 0.5, 0.5)), true)
        else {
            panic!("expected a frame");
        };
        assert_eq!(points[0].pressure, 0.0);
    }

    #[test]
    fn emulated_gesture_follows_its_first_contact() {
        let mut f = Forwarder::default();
        assert_eq!(
            f.step(Phase::Start, 1, 0.0, Some((2, 0.1, 0.2)), false),
            Step::Pointer {
                at: Some((2, 0.1, 0.2)),
                button: Some(true)
            }
        );
        // A second finger is consumed but not sent; its tail is the UI's.
        assert_eq!(
            f.step(Phase::Start, 2, 0.0, Some((2, 0.5, 0.5)), true),
            Step::Held
        );
        assert_eq!(
            f.step(Phase::Move, 1, 0.0, Some((2, 0.3, 0.4)), false),
            Step::Pointer {
                at: Some((2, 0.3, 0.4)),
                button: None
            }
        );
        assert_eq!(f.step(Phase::Move, 1, 0.0, None, false), Step::Held);
        assert_eq!(
            f.step(Phase::End, 1, 0.0, None, false),
            Step::Pointer {
                at: None,
                button: Some(false)
            }
        );
        assert!(f.is_empty());
    }
}
