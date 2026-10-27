//! Controller view of the other participants (`docs/protocol/v2-design.md`
//! §4.2–4.3): the list and control turn the host reports in `PARTICIPANTS`,
//! their pointers forwarded in `CURSOR`, and this participant's own pointer
//! sent to the host as `POINTER`.

use crate::protocol::participants::{self as wire, Click, ParticipantPointer, Participants};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// A pointer disappears after this long without moving.
pub(crate) const IDLE_FADE: Duration = Duration::from_secs(5);
const FADE_OUT: Duration = Duration::from_millis(800);
/// How long a click ring stays visible.
pub(crate) const CLICK_RING: Duration = Duration::from_millis(600);
/// Moves are sent at most this often; clicks and visibility changes at once.
const MOVE_INTERVAL: Duration = Duration::from_millis(16);

#[derive(Clone, Default)]
pub(crate) struct ParticipantsState(Arc<Mutex<State>>);

#[derive(Default)]
struct State {
    list: Option<Participants>,
    pointers: HashMap<u64, Remote>,
    clicks: Vec<Ring>,
    outgoing: Outgoing,
    /// Woken when the list or the other pointers change.
    listeners: Vec<Weak<dyn Fn() + Send + Sync>>,
}

struct Remote {
    pointer: ParticipantPointer,
    /// When the pointer last moved, from the host's `idle_ms`.
    moved: Instant,
}

struct Ring {
    screen_id: i32,
    x: f32,
    y: f32,
    button: i32,
    at: Instant,
}

#[derive(Default)]
struct Outgoing {
    seq: u32,
    last: Option<(Instant, i32, f32, f32, bool)>,
    /// A move was held back by the rate limit and is still owed.
    held: bool,
}

/// One participant in the panel.
#[derive(Clone, Debug)]
pub(crate) struct Member {
    pub id: u64,
    pub nickname: String,
    pub device_name: String,
    /// `own`, `friend` or `assist`.
    pub relation: String,
    /// 0xRRGGBB.
    pub color: u32,
    /// Holds the `control` capability.
    pub control: bool,
    /// Holds the control token: its keyboard and mouse reach the host.
    pub controlling: bool,
    pub you: bool,
}

/// Another participant's pointer to draw over the picture.
#[derive(Clone, Debug)]
pub(crate) struct PointerView {
    pub id: u64,
    pub nickname: String,
    pub color: u32,
    pub screen_id: i32,
    /// Normalized position within the screen.
    pub x: f32,
    pub y: f32,
    /// 0..=1, fading out after [`IDLE_FADE`].
    pub opacity: f32,
    /// This participant holds the control turn.
    pub controlling: bool,
}

/// A click feedback ring: blue for the left button, orange for the right.
#[derive(Clone, Debug)]
pub(crate) struct ClickView {
    pub screen_id: i32,
    pub x: f32,
    pub y: f32,
    /// One of `protocol::participants::button`.
    pub button: i32,
    /// 0 when the click happened, 1 when the ring is gone.
    pub progress: f32,
}

impl ParticipantsState {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Everyone in the session including this participant, in join order.
    pub(crate) fn members(&self) -> Vec<Member> {
        let state = self.lock();
        let Some(list) = &state.list else {
            return Vec::new();
        };
        list.participants
            .iter()
            .map(|p| Member {
                id: p.id,
                nickname: p.nickname.clone(),
                device_name: p.device_name.clone(),
                relation: p.relation.clone(),
                color: p.color,
                control: p.control,
                controlling: list.controlling != 0 && list.controlling == p.id,
                you: p.you,
            })
            .collect()
    }

    /// The participant holding the control token, if it is not this one.
    pub(crate) fn controlled_by_other(&self) -> Option<Member> {
        self.members()
            .into_iter()
            .find(|member| member.controlling && !member.you)
    }

    /// Other participants are connected too.
    pub(crate) fn others_present(&self) -> bool {
        self.lock()
            .list
            .as_ref()
            .is_some_and(|list| list.participants.len() > 1)
    }

    /// This participant holds the control token.
    pub(crate) fn holds_control(&self) -> bool {
        let state = self.lock();
        state.list.as_ref().is_some_and(|list| {
            list.controlling != 0
                && list
                    .participants
                    .iter()
                    .any(|p| p.you && p.id == list.controlling)
        })
    }

    /// This participant watches the shared viewer stream (1080p, 60 fps,
    /// cinema): several participants are connected and another one, or
    /// nobody, holds the control token. The host applies the same rule.
    pub(crate) fn shared_view(&self) -> bool {
        let state = self.lock();
        state.list.as_ref().is_some_and(|list| {
            list.participants.iter().filter(|p| !p.listening).count() > 1
                && !list
                    .participants
                    .iter()
                    .any(|p| p.you && p.id == list.controlling)
        })
    }

    /// The other participants' visible pointers, by id so that overlapping
    /// pointers keep their stacking from frame to frame.
    pub(crate) fn pointers(&self) -> Vec<PointerView> {
        let state = self.lock();
        let now = Instant::now();
        let (members, controlling) = state
            .list
            .as_ref()
            .map_or((&[][..], 0), |l| (&l.participants[..], l.controlling));
        let mut views = state
            .pointers
            .values()
            .filter(|remote| remote.pointer.visible)
            .filter_map(|remote| {
                let member = members
                    .iter()
                    .find(|m| m.id == remote.pointer.id && !m.you)?;
                let idle = now.saturating_duration_since(remote.moved);
                let opacity = if idle <= IDLE_FADE {
                    1.0
                } else {
                    1.0 - (idle - IDLE_FADE).as_secs_f32() / FADE_OUT.as_secs_f32()
                };
                (opacity > 0.0).then(|| PointerView {
                    id: member.id,
                    nickname: member.nickname.clone(),
                    color: member.color,
                    screen_id: remote.pointer.screen_id,
                    x: remote.pointer.x,
                    y: remote.pointer.y,
                    opacity: opacity.clamp(0.0, 1.0),
                    controlling: controlling == member.id,
                })
            })
            .collect::<Vec<_>>();
        views.sort_by_key(|view| view.id);
        views
    }

    /// Click rings of the other participants still animating.
    pub(crate) fn clicks(&self) -> Vec<ClickView> {
        let mut state = self.lock();
        let now = Instant::now();
        state
            .clicks
            .retain(|ring| now.saturating_duration_since(ring.at) < CLICK_RING);
        state
            .clicks
            .iter()
            .map(|ring| ClickView {
                screen_id: ring.screen_id,
                x: ring.x,
                y: ring.y,
                button: ring.button,
                progress: now.saturating_duration_since(ring.at).as_secs_f32()
                    / CLICK_RING.as_secs_f32(),
            })
            .collect()
    }

    /// When the overlay needs its next frame: at once while a click ring
    /// expands or a pointer fades out, when the fade starts while pointers
    /// rest, and `None` once nothing is left to animate. New pointers and
    /// list changes call the [`subscribe`](Self::subscribe) callbacks instead.
    pub(crate) fn next_frame(&self) -> Option<Duration> {
        let state = self.lock();
        let now = Instant::now();
        if state
            .clicks
            .iter()
            .any(|ring| now.saturating_duration_since(ring.at) < CLICK_RING)
        {
            return Some(Duration::ZERO);
        }
        state
            .pointers
            .values()
            .filter(|remote| remote.pointer.visible)
            .filter_map(|remote| {
                let idle = now.saturating_duration_since(remote.moved);
                (idle < IDLE_FADE + FADE_OUT).then(|| IDLE_FADE.saturating_sub(idle))
            })
            .min()
    }

    /// Call `callback` whenever the list or the other pointers change.
    /// It stays registered while the returned handle is alive.
    pub(crate) fn subscribe(
        &self,
        callback: impl Fn() + Send + Sync + 'static,
    ) -> Arc<dyn Fn() + Send + Sync> {
        let callback: Arc<dyn Fn() + Send + Sync> = Arc::new(callback);
        let mut state = self.lock();
        state
            .listeners
            .retain(|listener| listener.strong_count() != 0);
        state.listeners.push(Arc::downgrade(&callback));
        callback
    }

    fn notify(&self) {
        let callbacks: Vec<_> = self
            .lock()
            .listeners
            .iter()
            .filter_map(Weak::upgrade)
            .collect();
        for callback in callbacks {
            callback();
        }
    }

    pub(crate) fn clear(&self) {
        {
            let mut state = self.lock();
            state.list = None;
            state.pointers.clear();
            state.clicks.clear();
            state.outgoing = Outgoing::default();
        }
        self.notify();
    }

    /// `PARTICIPANTS` from the host.
    pub(crate) fn receive_list(&self, list: Participants) {
        {
            let mut state = self.lock();
            state
                .pointers
                .retain(|id, _| list.participants.iter().any(|p| p.id == *id));
            state.list = Some(list);
        }
        self.notify();
    }

    /// The other participants' pointers, forwarded in `CURSOR`.
    pub(crate) fn receive_pointers(&self, pointers: Vec<ParticipantPointer>) {
        self.store_pointers(pointers);
        self.notify();
    }

    fn store_pointers(&self, pointers: Vec<ParticipantPointer>) {
        let mut state = self.lock();
        let now = Instant::now();
        let mut seen = HashMap::with_capacity(pointers.len());
        for pointer in pointers.into_iter().take(wire::MAX_PARTICIPANTS) {
            if !(pointer.x.is_finite()
                && pointer.y.is_finite()
                && (0.0..=1.0).contains(&pointer.x)
                && (0.0..=1.0).contains(&pointer.y))
            {
                continue;
            }
            let previous = state.pointers.get(&pointer.id);
            let new_click = pointer.click.filter(|click| {
                click.phase == wire::phase::DOWN
                    && previous.is_none_or(|p| p.pointer.click_seq != pointer.click_seq)
            });
            if let Some(click) = new_click {
                state.clicks.push(Ring {
                    screen_id: pointer.screen_id,
                    x: pointer.x,
                    y: pointer.y,
                    button: click.button,
                    at: now,
                });
            }
            let moved = now
                .checked_sub(Duration::from_millis(u64::from(pointer.idle_ms)))
                .unwrap_or(now);
            seen.insert(pointer.id, Remote { pointer, moved });
        }
        state.pointers = seen;
        if state.clicks.len() > 32 {
            let excess = state.clicks.len() - 32;
            state.clicks.drain(..excess);
        }
    }

    /// Encode this participant's pointer for the host, or `None` when it
    /// should not be sent yet (unchanged, or a move within the rate limit).
    /// `position` is the normalized position within `screen_id`; `None`
    /// once the pointer left the picture.
    pub(crate) fn pointer_frame(
        &self,
        screen_id: i32,
        position: Option<[f32; 2]>,
        click: Option<Click>,
    ) -> Option<Vec<u8>> {
        let mut state = self.lock();
        let now = Instant::now();
        let visible = position.is_some();
        let [x, y] = position.unwrap_or_default().map(|v| {
            if v.is_finite() {
                v.clamp(0.0, 1.0)
            } else {
                0.0
            }
        });
        if click.is_none()
            && let Some((at, last_screen, last_x, last_y, last_visible)) = state.outgoing.last
        {
            let unchanged =
                last_visible == visible && (!visible || (last_screen, last_x, last_y) == (screen_id, x, y));
            if unchanged {
                state.outgoing.held = false;
                return None;
            }
            if visible && last_visible && now.duration_since(at) < MOVE_INTERVAL {
                state.outgoing.held = true;
                return None;
            }
        }
        if click.is_none() && state.outgoing.last.is_none() && !visible {
            return None;
        }
        state.outgoing.seq = state.outgoing.seq.wrapping_add(1);
        state.outgoing.last = Some((now, screen_id, x, y, visible));
        state.outgoing.held = false;
        Some(wire::encode_pointer(&wire::Pointer {
            seq: state.outgoing.seq,
            screen_id,
            x,
            y,
            visible,
            click,
        }))
    }

    /// When a move held back by the rate limit may be sent. The caller runs
    /// another frame then, so the final position of a movement is not lost
    /// when the local pointer stops right after it.
    pub(crate) fn pointer_retry(&self) -> Option<Duration> {
        let state = self.lock();
        let (at, ..) = state.outgoing.last.filter(|_| state.outgoing.held)?;
        Some(MOVE_INTERVAL.saturating_sub(at.elapsed()))
    }

    /// The screen this participant's pointer was last reported visible on.
    /// With one window per screen, only the window that reported it hides it.
    pub(crate) fn pointer_screen(&self) -> Option<i32> {
        self.lock()
            .outgoing
            .last
            .and_then(|(_, screen, _, _, visible)| visible.then_some(screen))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::participants::{Participant, button, phase};

    fn list() -> Participants {
        Participants {
            participants: vec![
                Participant {
                    id: 1,
                    nickname: "me".into(),
                    you: true,
                    ..Default::default()
                },
                Participant {
                    id: 2,
                    nickname: "bob".into(),
                    color: 0xF97316,
                    ..Default::default()
                },
            ],
            controlling: 2,
        }
    }

    #[test]
    fn pointers_skip_self_and_report_clicks_once() {
        let state = ParticipantsState::default();
        state.receive_list(list());
        let click = Some(Click {
            button: button::LEFT,
            phase: phase::DOWN,
        });
        let pointers = vec![
            ParticipantPointer {
                id: 1,
                visible: true,
                ..Default::default()
            },
            ParticipantPointer {
                id: 2,
                x: 0.5,
                y: 0.5,
                visible: true,
                click,
                click_seq: 1,
                ..Default::default()
            },
        ];
        state.receive_pointers(pointers.clone());
        state.receive_pointers(pointers);
        let views = state.pointers();
        assert_eq!(views.len(), 1);
        assert_eq!(views[0].nickname, "bob");
        assert!(views[0].controlling);
        assert_eq!(state.clicks().len(), 1);
        assert_eq!(state.controlled_by_other().map(|m| m.id), Some(2));
    }

    #[test]
    fn outgoing_moves_are_rate_limited_but_clicks_are_not() {
        let state = ParticipantsState::default();
        assert!(state.pointer_frame(0, None, None).is_none());
        assert!(state.pointer_frame(0, Some([0.1, 0.1]), None).is_some());
        assert!(state.pointer_frame(0, Some([0.2, 0.2]), None).is_none());
        let click = Some(Click {
            button: button::RIGHT,
            phase: phase::DOWN,
        });
        assert!(state.pointer_frame(0, Some([0.2, 0.2]), click).is_some());
        assert!(state.pointer_frame(0, None, None).is_some());
        assert!(state.pointer_frame(0, None, None).is_none());
    }

    #[test]
    fn held_moves_ask_for_a_retry_and_the_shown_screen_is_tracked() {
        let state = ParticipantsState::default();
        assert_eq!(state.pointer_screen(), None);
        assert!(state.pointer_frame(2, Some([0.1, 0.1]), None).is_some());
        assert_eq!(state.pointer_screen(), Some(2));
        assert!(state.pointer_retry().is_none());
        assert!(state.pointer_frame(2, Some([0.3, 0.3]), None).is_none());
        assert!(state.pointer_retry().is_some_and(|d| d <= MOVE_INTERVAL));
        // Returning to the sent position settles the owed move.
        assert!(state.pointer_frame(2, Some([0.1, 0.1]), None).is_none());
        assert!(state.pointer_retry().is_none());
        assert!(state.pointer_frame(2, None, None).is_some());
        assert_eq!(state.pointer_screen(), None);
    }

    #[test]
    fn listeners_wake_on_changes_and_frames_follow_animation() {
        let state = ParticipantsState::default();
        let woken = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = Arc::clone(&woken);
        let handle = state.subscribe(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        });
        state.receive_list(list());
        assert_eq!(state.next_frame(), None);
        state.receive_pointers(vec![ParticipantPointer {
            id: 2,
            x: 0.5,
            y: 0.5,
            visible: true,
            idle_ms: 1000,
            ..Default::default()
        }]);
        assert_eq!(woken.load(std::sync::atomic::Ordering::Relaxed), 2);
        // Resting: the next frame is due when the fade begins.
        assert!(
            state
                .next_frame()
                .is_some_and(|d| d > Duration::ZERO && d <= IDLE_FADE)
        );
        state.receive_pointers(vec![ParticipantPointer {
            id: 2,
            visible: true,
            click: Some(Click {
                button: button::LEFT,
                phase: phase::DOWN,
            }),
            click_seq: 1,
            ..Default::default()
        }]);
        assert_eq!(state.next_frame(), Some(Duration::ZERO));
        drop(handle);
        state.clear();
        assert_eq!(woken.load(std::sync::atomic::Ordering::Relaxed), 3);
        assert_eq!(state.next_frame(), None);
    }
}
