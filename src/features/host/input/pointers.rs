//! Touch contacts and the pen of the participant in control, injected as
//! Windows synthetic pointers (`docs/protocol/touch-pen-brief.md` §2.3–§2.8,
//! §5.2–§5.6).
//!
//! Windows needs every active contact in every touch frame, cancels contacts
//! that are not refreshed for about a second, and refuses an UP at another
//! position than the last update. This keeps the table that satisfies those
//! rules, re-injects still contacts every 100 ms, and cancels everything when
//! the owner goes quiet.
use super::wire::{PenPhase, PenSample, TouchContact, TouchPhase};
use crate::platform::windows::input::pointer::{
    self, Device, PenAction, PenPoint, Phase, TOUCH_SLOTS, TouchPoint,
};
use anyhow::Result;
use std::{
    collections::{BTreeMap, BTreeSet},
    time::{Duration, Instant},
};
use windows::Win32::Foundation::RECT;

/// Still contacts and a hovering pen are injected again this often.
const REFRESH: Duration = Duration::from_millis(100);
/// Synthetic devices are destroyed after this long without touch or pen.
const IDLE: Duration = Duration::from_secs(30);

#[derive(Clone, Copy)]
struct Slot {
    point: TouchPoint,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PenState {
    Away,
    Hover,
    Contact,
}

/// Where frames go: the Windows synthetic devices, or a recorder in tests.
pub(super) trait Inject {
    fn touch(&mut self, frame: &[TouchPoint]) -> Result<()>;
    fn pen(&mut self, point: &PenPoint) -> Result<()>;
    /// Destroy the devices (desktop switch, idle).
    fn close(&mut self);
}

/// The synthetic touch and pen devices, created when first used.
#[derive(Default)]
pub(super) struct Devices {
    touch: Option<Device>,
    pen: Option<Device>,
}

impl Inject for Devices {
    fn touch(&mut self, frame: &[TouchPoint]) -> Result<()> {
        if self.touch.is_none() {
            self.touch = Some(Device::touch()?);
            tracing::info!("host synthetic touch device created");
        }
        self.touch.as_ref().expect("created above").touch_frame(frame)
    }
    fn pen(&mut self, point: &PenPoint) -> Result<()> {
        if self.pen.is_none() {
            self.pen = Some(Device::pen()?);
            tracing::info!("host synthetic pen device created");
        }
        self.pen.as_ref().expect("created above").pen_frame(point)
    }
    fn close(&mut self) {
        self.touch = None;
        self.pen = None;
    }
}

pub(super) struct Pointers<I: Inject = Devices> {
    inject: I,
    contacts: BTreeMap<u8, Slot>,
    /// Contacts refused (no free slot) until they end.
    ignored: BTreeSet<u8>,
    pen: PenState,
    pen_last: Option<PenPoint>,
    last_inject: Instant,
    last_used: Instant,
}

/// Map a normalized position to desktop pixels, like the mouse.
pub(super) fn pixel(rect: RECT, x: f64, y: f64) -> (i32, i32) {
    let px = (rect.left as f64 + (rect.right - rect.left) as f64 * x)
        .round()
        .clamp(rect.left as f64, (rect.right - 1) as f64);
    let py = (rect.top as f64 + (rect.bottom - rect.top) as f64 * y)
        .round()
        .clamp(rect.top as f64, (rect.bottom - 1) as f64);
    (px as i32, py as i32)
}

fn touch_point(rect: RECT, c: &TouchContact, id: u32, phase: Phase, primary: bool) -> TouchPoint {
    let (x, y) = pixel(rect, c.x, c.y);
    let w = (f64::from(c.width) * f64::from(rect.right - rect.left) / 2.0).round() as i32;
    let h = (f64::from(c.height) * f64::from(rect.bottom - rect.top) / 2.0).round() as i32;
    let area = if w > 0 && h > 0 {
        RECT {
            left: (x - w).max(rect.left),
            top: (y - h).max(rect.top),
            right: (x + w).min(rect.right),
            bottom: (y + h).min(rect.bottom),
        }
    } else {
        RECT::default()
    };
    TouchPoint {
        id,
        phase,
        primary,
        x,
        y,
        area,
        pressure: if c.pressure > 0.0 {
            ((f64::from(c.pressure) * 1024.0).round() as u32).clamp(1, 1024)
        } else {
            0
        },
        orientation: u32::from(c.orientation),
    }
}

fn pen_point(rect: RECT, s: &PenSample, action: PenAction) -> PenPoint {
    let (x, y) = pixel(rect, s.x, s.y);
    PenPoint {
        action,
        x,
        y,
        pressure: (f64::from(s.pressure) * 1024.0).round() as u32,
        rotation: u32::from(s.twist),
        tilt_x: i32::from(s.tilt_x),
        tilt_y: i32::from(s.tilt_y),
        barrel: s.barrel,
        inverted: s.eraser,
        erasing: s.eraser,
    }
}

impl Pointers {
    pub fn new() -> Self {
        Self::with(Devices::default())
    }

    /// Synthetic injection is possible on this system.
    pub fn available() -> bool {
        pointer::available()
    }
}

impl<I: Inject> Pointers<I> {
    pub fn with(inject: I) -> Self {
        let now = Instant::now();
        Self {
            inject,
            contacts: BTreeMap::new(),
            ignored: BTreeSet::new(),
            pen: PenState::Away,
            pen_last: None,
            last_inject: now,
            last_used: now,
        }
    }

    /// Any contact down or the pen in range.
    pub fn active(&self) -> bool {
        !self.contacts.is_empty() || self.pen != PenState::Away
    }

    fn inject_touch(&mut self, frame: &[TouchPoint]) -> Result<()> {
        if frame.is_empty() {
            return Ok(());
        }
        let result = self.inject.touch(frame);
        self.last_inject = Instant::now();
        self.last_used = self.last_inject;
        if let Err(error) = &result {
            tracing::warn!(%error, contacts = frame.len(), first = ?frame[0], "host touch injection failed");
            // Windows cancels every contact on a failed frame.
            self.contacts.clear();
            self.ignored.clear();
        }
        result
    }

    fn inject_pen(&mut self, point: PenPoint) -> Result<()> {
        let result = self.inject.pen(&point);
        self.last_inject = Instant::now();
        self.last_used = self.last_inject;
        if let Err(error) = &result {
            tracing::warn!(%error, ?point, "host pen injection failed");
            self.pen = PenState::Away;
            self.pen_last = None;
        } else {
            self.pen_last = Some(point);
        }
        result
    }

    /// A TOUCH frame on the screen `rect`: every active contact of the owner.
    pub fn touch(&mut self, rect: RECT, contacts: &[TouchContact]) -> Result<()> {
        if contacts.is_empty() {
            return self.cancel_touch();
        }
        let present: BTreeSet<u8> = contacts.iter().map(|c| c.id).collect();
        // An UP must be at the last updated position: move it there first.
        let moved_up = contacts.iter().any(|c| {
            c.phase == TouchPhase::Up
                && self.contacts.get(&c.id).is_some_and(|slot| {
                    let (x, y) = pixel(rect, c.x, c.y);
                    (slot.point.x, slot.point.y) != (x, y)
                })
        });
        if moved_up {
            let mut frame = Vec::new();
            for (id, slot) in self.contacts.iter_mut() {
                if !present.contains(id) {
                    continue;
                }
                if let Some(c) = contacts.iter().find(|c| c.id == *id && c.phase == TouchPhase::Up) {
                    slot.point = touch_point(rect, c, slot.point.id, Phase::Update, slot.point.primary);
                }
                frame.push(TouchPoint {
                    phase: Phase::Update,
                    ..slot.point
                });
            }
            self.inject_touch(&frame)?;
        }
        let mut frame = Vec::new();
        // Contacts the controller no longer lists end as cancelled.
        let gone: Vec<u8> = self.contacts.keys().filter(|id| !present.contains(id)).copied().collect();
        for id in gone {
            let slot = self.contacts.remove(&id).expect("listed");
            frame.push(TouchPoint {
                phase: Phase::Cancel,
                ..slot.point
            });
        }
        for c in contacts {
            match (c.phase, self.contacts.get(&c.id).copied()) {
                (TouchPhase::Down | TouchPhase::Move, Some(slot)) => {
                    let point = touch_point(rect, c, slot.point.id, Phase::Update, slot.point.primary);
                    self.contacts.insert(c.id, Slot { point });
                    frame.push(point);
                }
                (TouchPhase::Down, None) => {
                    if self.ignored.contains(&c.id) || self.contacts.len() >= TOUCH_SLOTS as usize {
                        self.ignored.insert(c.id);
                        continue;
                    }
                    let used: BTreeSet<u32> = self.contacts.values().map(|s| s.point.id).collect();
                    let id = (0..TOUCH_SLOTS).find(|n| !used.contains(n)).expect("a free slot");
                    let point = touch_point(rect, c, id, Phase::Down, self.contacts.is_empty());
                    self.contacts.insert(c.id, Slot { point });
                    frame.push(point);
                }
                (TouchPhase::Up, Some(slot)) => {
                    self.contacts.remove(&c.id);
                    frame.push(TouchPoint {
                        phase: Phase::Up,
                        ..slot.point
                    });
                }
                (TouchPhase::Cancel, Some(slot)) => {
                    self.contacts.remove(&c.id);
                    frame.push(TouchPoint {
                        phase: Phase::Cancel,
                        ..slot.point
                    });
                }
                (TouchPhase::Up | TouchPhase::Cancel, None) => {
                    self.ignored.remove(&c.id);
                }
                (TouchPhase::Move, None) => {}
            }
        }
        self.inject_touch(&frame)
    }

    pub fn cancel_touch(&mut self) -> Result<()> {
        self.ignored.clear();
        if self.contacts.is_empty() {
            return Ok(());
        }
        let frame: Vec<TouchPoint> = std::mem::take(&mut self.contacts)
            .into_values()
            .map(|slot| TouchPoint {
                phase: Phase::Cancel,
                ..slot.point
            })
            .collect();
        self.inject_touch(&frame)
    }

    /// PEN samples on the screen `rect`, oldest first.
    pub fn pen(&mut self, rect: RECT, samples: &[PenSample]) -> Result<()> {
        for s in samples {
            let at = |action| pen_point(rect, s, action);
            match (s.phase, self.pen) {
                (PenPhase::Hover | PenPhase::Move, PenState::Away | PenState::Hover) => {
                    self.inject_pen(at(PenAction::Hover))?;
                    self.pen = PenState::Hover;
                }
                (PenPhase::Hover, PenState::Contact) => {
                    self.inject_pen(at(PenAction::Up))?;
                    self.inject_pen(at(PenAction::Hover))?;
                    self.pen = PenState::Hover;
                }
                (PenPhase::Down, PenState::Away | PenState::Hover) => {
                    self.inject_pen(at(PenAction::Down))?;
                    self.pen = PenState::Contact;
                }
                (PenPhase::Down | PenPhase::Move, PenState::Contact) => {
                    self.inject_pen(at(PenAction::Move))?;
                }
                (PenPhase::Up, PenState::Contact) => {
                    self.inject_pen(at(PenAction::Up))?;
                    self.pen = PenState::Hover;
                }
                (PenPhase::Leave, PenState::Contact) => {
                    self.inject_pen(at(PenAction::Up))?;
                    self.inject_pen(at(PenAction::Leave))?;
                    self.pen = PenState::Away;
                }
                (PenPhase::Cancel, PenState::Contact) => {
                    self.inject_pen(at(PenAction::Cancel))?;
                    self.inject_pen(at(PenAction::Leave))?;
                    self.pen = PenState::Away;
                }
                (PenPhase::Leave | PenPhase::Cancel, PenState::Hover) => {
                    self.inject_pen(at(PenAction::Leave))?;
                    self.pen = PenState::Away;
                }
                (PenPhase::Up | PenPhase::Leave | PenPhase::Cancel, _) => {}
            }
        }
        Ok(())
    }

    pub fn cancel_pen(&mut self) -> Result<()> {
        let Some(last) = self.pen_last else {
            self.pen = PenState::Away;
            return Ok(());
        };
        let state = std::mem::replace(&mut self.pen, PenState::Away);
        if state == PenState::Contact {
            self.inject_pen(PenPoint {
                action: PenAction::Cancel,
                ..last
            })?;
        }
        if state != PenState::Away {
            self.inject_pen(PenPoint {
                action: PenAction::Leave,
                ..last
            })?;
        }
        Ok(())
    }

    /// Cancel every contact and take the pen away.
    pub fn cancel(&mut self) -> Result<()> {
        let touch = self.cancel_touch();
        let pen = self.cancel_pen();
        touch.and(pen)
    }

    /// Keep still contacts alive, cancel everything when the owner is quiet
    /// longer than `threshold`, and drop idle devices.
    pub fn tick(&mut self, quiet: Duration, threshold: Duration) -> Result<()> {
        if self.active() && quiet > threshold {
            tracing::debug!(quiet_ms = quiet.as_millis() as u64, "touch/pen owner quiet; cancelling contacts");
            return self.cancel();
        }
        if self.active() && self.last_inject.elapsed() >= REFRESH {
            if !self.contacts.is_empty() {
                let frame: Vec<TouchPoint> = self
                    .contacts
                    .values()
                    .map(|slot| TouchPoint {
                        phase: Phase::Update,
                        ..slot.point
                    })
                    .collect();
                self.inject_touch(&frame)?;
            }
            if let Some(last) = self.pen_last {
                let action = match self.pen {
                    PenState::Contact => PenAction::Move,
                    PenState::Hover => PenAction::Hover,
                    PenState::Away => return Ok(()),
                };
                self.inject_pen(PenPoint { action, ..last })?;
            }
        }
        if !self.active() && self.last_used.elapsed() >= IDLE {
            self.close_devices();
        }
        Ok(())
    }

    /// Destroy the devices (desktop switch, idle); contacts must be gone.
    pub fn close_devices(&mut self) {
        self.contacts.clear();
        self.ignored.clear();
        self.pen = PenState::Away;
        self.pen_last = None;
        self.inject.close();
    }
}

impl<I: Inject> Drop for Pointers<I> {
    fn drop(&mut self) {
        let _ = self.cancel();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Recorder {
        frames: Vec<Vec<(u32, Phase, i32, i32, bool)>>,
        pens: Vec<(PenAction, i32, i32, u32)>,
        closed: usize,
    }
    impl Inject for Recorder {
        fn touch(&mut self, frame: &[TouchPoint]) -> Result<()> {
            self.frames
                .push(frame.iter().map(|p| (p.id, p.phase, p.x, p.y, p.primary)).collect());
            Ok(())
        }
        fn pen(&mut self, point: &PenPoint) -> Result<()> {
            self.pens.push((point.action, point.x, point.y, point.pressure));
            Ok(())
        }
        fn close(&mut self) {
            self.closed += 1;
        }
    }

    const SCREEN: RECT = RECT {
        left: 0,
        top: 0,
        right: 1000,
        bottom: 500,
    };

    fn c(id: u8, phase: TouchPhase, x: f64, y: f64) -> TouchContact {
        TouchContact {
            id,
            phase,
            x,
            y,
            pressure: 0.0,
            width: 0.0,
            height: 0.0,
            orientation: 0,
        }
    }

    fn take(p: &mut Pointers<Recorder>) -> Vec<Vec<(u32, Phase, i32, i32, bool)>> {
        std::mem::take(&mut p.inject.frames)
    }

    #[test]
    fn contacts_get_slots_and_every_frame_lists_all() {
        let mut p = Pointers::with(Recorder::default());
        p.touch(SCREEN, &[c(7, TouchPhase::Down, 0.5, 0.5)]).unwrap();
        p.touch(SCREEN, &[c(7, TouchPhase::Move, 0.6, 0.5), c(9, TouchPhase::Down, 0.1, 0.2)])
            .unwrap();
        assert_eq!(
            take(&mut p),
            vec![
                vec![(0, Phase::Down, 500, 250, true)],
                vec![(0, Phase::Update, 600, 250, true), (1, Phase::Down, 100, 100, false)],
            ]
        );
        // An UP elsewhere first moves there, then lifts at that position.
        p.touch(SCREEN, &[c(7, TouchPhase::Up, 0.7, 0.5), c(9, TouchPhase::Move, 0.1, 0.2)])
            .unwrap();
        assert_eq!(
            take(&mut p),
            vec![
                vec![(0, Phase::Update, 700, 250, true), (1, Phase::Update, 100, 100, false)],
                vec![(0, Phase::Up, 700, 250, true), (1, Phase::Update, 100, 100, false)],
            ]
        );
        // A contact missing from the frame is cancelled; slot 0 is free again.
        p.touch(SCREEN, &[c(3, TouchPhase::Down, 0.0, 0.0)]).unwrap();
        assert_eq!(
            take(&mut p),
            vec![vec![(1, Phase::Cancel, 100, 100, false), (0, Phase::Down, 0, 0, true)]]
        );
        // An empty frame cancels everything.
        p.touch(SCREEN, &[]).unwrap();
        assert_eq!(take(&mut p), vec![vec![(0, Phase::Cancel, 0, 0, true)]]);
        assert!(!p.active());
    }

    #[test]
    fn eleventh_contact_is_ignored_until_it_ends() {
        let mut p = Pointers::with(Recorder::default());
        let downs: Vec<_> = (1..=10).map(|id| c(id, TouchPhase::Down, 0.5, 0.5)).collect();
        p.touch(SCREEN, &downs).unwrap();
        let mut frame: Vec<_> = (1..=10).map(|id| c(id, TouchPhase::Move, 0.5, 0.5)).collect();
        frame.push(c(11, TouchPhase::Down, 0.5, 0.5));
        p.touch(SCREEN, &frame).unwrap();
        let frames = take(&mut p);
        assert_eq!(frames[1].len(), 10);
        assert!(frames[1].iter().all(|f| f.1 == Phase::Update));
    }

    #[test]
    fn pen_states() {
        let mut p = Pointers::with(Recorder::default());
        let s = |phase: PenPhase, x: f64, pressure: f32| PenSample {
            phase,
            x,
            y: 0.5,
            pressure,
            tilt_x: 0,
            tilt_y: 0,
            twist: 0,
            barrel: false,
            eraser: false,
        };
        p.pen(
            SCREEN,
            &[
                s(PenPhase::Hover, 0.1, 0.0),
                s(PenPhase::Down, 0.1, 0.5),
                s(PenPhase::Move, 0.2, 1.0),
                // Hover while touching lifts first.
                s(PenPhase::Hover, 0.3, 0.0),
                s(PenPhase::Leave, 0.3, 0.0),
                // Up and Move while away are ignored or hover.
                s(PenPhase::Up, 0.3, 0.0),
            ],
        )
        .unwrap();
        let actions: Vec<_> = p.inject.pens.iter().map(|e| (e.0, e.1, e.3)).collect();
        assert_eq!(
            actions,
            vec![
                (PenAction::Hover, 100, 0),
                (PenAction::Down, 100, 512),
                (PenAction::Move, 200, 1024),
                (PenAction::Up, 300, 0),
                (PenAction::Hover, 300, 0),
                (PenAction::Leave, 300, 0),
            ]
        );
        assert!(!p.active());
        // Cancel while touching: cancel, then out of range.
        p.inject.pens.clear();
        p.pen(SCREEN, &[s(PenPhase::Down, 0.5, 0.5)]).unwrap();
        p.cancel().unwrap();
        let actions: Vec<_> = p.inject.pens.iter().map(|e| e.0).collect();
        assert_eq!(actions, vec![PenAction::Down, PenAction::Cancel, PenAction::Leave]);
    }

    #[test]
    fn quiet_owner_cancels_and_still_contacts_refresh() {
        let mut p = Pointers::with(Recorder::default());
        p.touch(SCREEN, &[c(1, TouchPhase::Down, 0.5, 0.5)]).unwrap();
        take(&mut p);
        // Not quiet yet, but due for a refresh.
        p.last_inject = Instant::now() - Duration::from_millis(150);
        p.tick(Duration::from_millis(10), Duration::from_millis(150)).unwrap();
        assert_eq!(take(&mut p), vec![vec![(0, Phase::Update, 500, 250, true)]]);
        // Quiet longer than the threshold: cancelled.
        p.tick(Duration::from_millis(400), Duration::from_millis(150)).unwrap();
        assert_eq!(take(&mut p), vec![vec![(0, Phase::Cancel, 500, 250, true)]]);
        assert!(!p.active());
        // Idle devices are closed.
        p.last_used = Instant::now() - IDLE;
        p.tick(Duration::from_secs(60), Duration::from_millis(150)).unwrap();
        assert_eq!(p.inject.closed, 1);
    }
}
