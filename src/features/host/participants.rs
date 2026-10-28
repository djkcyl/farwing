//! Participants of this host (`docs/protocol/v2-design.md` §4.2–4.3): how
//! each one is shown to the others, who holds the control token, and every
//! participant's pointer. Peers publish from here; the host overlay and the
//! host GUI read [`snapshot`].
//!
//! Control is first come, first served: one participant at a time holds the
//! token ([`take_control`]); the others wait until it is released — by its
//! holder, by leaving, or by losing the `control` capability.

mod overlay;

use crate::protocol::participants::{
    self as wire, Click, Participant, ParticipantPointer, Participants, Pointer,
};
use std::collections::BTreeMap;
use crate::i18n::t;
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::watch;

/// A click is repeated in pointer updates for this long.
const CLICK_HOLD: Duration = Duration::from_millis(500);

struct Hub {
    state: Mutex<State>,
    /// The latest host lifecycle event for every participant, by sequence.
    /// The latest host event: sequence, frame, and the participant it is
    /// not sent to (the one who caused it).
    event: Mutex<Option<(u64, Vec<u8>, Option<u64>)>>,
    /// Bumps on joins, leaves, profile and control-token changes.
    members: watch::Sender<u64>,
    /// Bumps on pointer changes.
    pointers: watch::Sender<u64>,
}

#[derive(Default)]
struct State {
    members: BTreeMap<u64, Member>,
    /// The participant holding the control token.
    controlling: Option<u64>,
    joined: usize,
}

struct Member {
    profile: Profile,
    color: u32,
    /// The palette colour given at joining, used when the participant asks
    /// for none.
    assigned: u32,
    joined_ms: i64,
    control: Arc<dyn Fn() -> bool + Send + Sync>,
    pointer: Option<Last>,
    /// Watches the picture (not audio only).
    watching: bool,
}

struct Last {
    pointer: Pointer,
    moved: Instant,
    click: Option<(Click, Instant)>,
    clicks: u32,
}

static HUB: LazyLock<Hub> = LazyLock::new(|| Hub {
    state: Mutex::default(),
    event: Mutex::new(None),
    members: watch::channel(0).0,
    pointers: watch::channel(0).0,
});

fn state() -> std::sync::MutexGuard<'static, State> {
    HUB.state.lock().unwrap_or_else(|p| p.into_inner())
}

fn bump(sender: &watch::Sender<u64>) {
    sender.send_modify(|revision| *revision = revision.wrapping_add(1));
}

/// How a participant is shown to the others.
#[derive(Clone, Debug, Default)]
pub(crate) struct Profile {
    pub nickname: String,
    pub device_name: String,
    /// `own`, `friend` or `assist`.
    pub relation: String,
}

/// A participant's place in the hub; leaving on drop.
pub(crate) struct Membership {
    key: u64,
}

impl Membership {
    pub(crate) fn key(&self) -> u64 {
        self.key
    }
}

/// Add a participant. `key` is its access participant key; `control`
/// reports whether it currently holds the `control` capability.
pub(crate) fn join(
    key: u64,
    profile: Profile,
    control: impl Fn() -> bool + Send + Sync + 'static,
    watching: bool,
) -> Membership {
    let mut state = state();
    let color = wire::PALETTE[state.joined % wire::PALETTE.len()];
    state.joined = state.joined.wrapping_add(1);
    state.members.insert(
        key,
        Member {
            profile,
            color,
            assigned: color,
            joined_ms: chrono::Utc::now().timestamp_millis(),
            control: Arc::new(control),
            pointer: None,
            watching,
        },
    );
    drop(state);
    bump(&HUB.members);
    // Runs until the last participant leaves.
    overlay::start();
    crate::platform::windows::host_lifecycle::start(lifecycle);
    Membership { key }
}

/// Tell participants about a suspend or session switch before it cuts them
/// off; they wait for the host instead of failing (§10).
fn lifecycle(event: crate::platform::windows::host_lifecycle::Event) {
    use crate::platform::windows::host_lifecycle::Event;
    use crate::protocol::host_event::kind;
    if state().members.is_empty() {
        return;
    }
    match event {
        Event::Suspending => broadcast_host_event(kind::SLEEPING),
        Event::SessionSwitched => broadcast_host_event(kind::SESSION_SWITCH),
        Event::Resumed => {}
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        let mut state = state();
        state.members.remove(&self.key);
        if state.controlling == Some(self.key) {
            set_holder(&mut state, None);
        }
        if state.members.is_empty() {
            state.joined = 0;
        }
        drop(state);
        bump(&HUB.members);
        bump(&HUB.pointers);
    }
}

impl Membership {
    /// The colour the participant asked for in a HELLO profile, at joining
    /// or later in the session; 0 returns to the colour it was given.
    pub(crate) fn set_color(&self, color: u32) {
        if let Some(member) = state().members.get_mut(&self.key) {
            member.color = match color & 0x00FF_FFFF {
                0 => member.assigned,
                color => color,
            };
            tracing::debug!(participant = self.key, color = format_args!("{:06x}", member.color),
                "participant cursor colour");
        }
        bump(&HUB.members);
    }

    /// A `POINTER` frame from this participant.
    pub(crate) fn pointer(&self, pointer: Pointer) {
        let mut state = state();
        let Some(member) = state.members.get_mut(&self.key) else {
            return;
        };
        let now = Instant::now();
        if member.pointer.is_none() {
            tracing::info!(participant = self.key, "participant pointer started");
        }
        let last = member.pointer.get_or_insert(Last {
            pointer: Pointer::default(),
            moved: now,
            click: None,
            clicks: 0,
        });
        // A click wakes a resting pointer like a move does.
        if pointer.click.is_some()
            || (pointer.screen_id, pointer.x, pointer.y, pointer.visible)
                != (last.pointer.screen_id, last.pointer.x, last.pointer.y, last.pointer.visible)
        {
            last.moved = now;
        }
        if let Some(click) = pointer.click {
            last.click = Some((click, now));
            last.clicks = last.clicks.wrapping_add(1);
        }
        last.pointer = pointer;
        drop(state);
        bump(&HUB.pointers);
    }

    /// `PARTICIPANTS` as this participant sees it.
    pub(crate) fn participants_frame(&self) -> Vec<u8> {
        let state = state();
        wire::encode_participants(&Participants {
            participants: state
                .members
                .iter()
                .map(|(key, member)| Participant {
                    id: *key,
                    nickname: member.profile.nickname.clone(),
                    device_name: member.profile.device_name.clone(),
                    relation: member.profile.relation.clone(),
                    color: member.color,
                    control: (member.control)(),
                    you: *key == self.key,
                    joined_ms: member.joined_ms,
                    listening: !member.watching,
                })
                .collect(),
            controlling: state.controlling.unwrap_or(0),
        })
    }

    /// `CURSOR` with the other participants' pointers.
    pub(crate) fn pointers_frame(&self) -> Vec<u8> {
        let state = state();
        let now = Instant::now();
        let pointers = state
            .members
            .iter()
            .filter(|(key, _)| **key != self.key)
            .filter_map(|(key, member)| {
                let last = member.pointer.as_ref()?;
                let click = last
                    .click
                    .filter(|(_, at)| now.saturating_duration_since(*at) < CLICK_HOLD);
                Some(ParticipantPointer {
                    id: *key,
                    screen_id: last.pointer.screen_id,
                    x: last.pointer.x,
                    y: last.pointer.y,
                    visible: last.pointer.visible,
                    idle_ms: now
                        .saturating_duration_since(last.moved)
                        .as_millis()
                        .min(u128::from(u32::MAX)) as u32,
                    click: click.map(|(click, _)| click),
                    click_seq: last.clicks,
                })
            })
            .collect();
        crate::features::remote_cursor::encode_pointers(pointers)
    }
}

/// Revisions of the member list and of the pointers.
pub(crate) fn subscribe() -> (watch::Receiver<u64>, watch::Receiver<u64>) {
    (HUB.members.subscribe(), HUB.pointers.subscribe())
}

/// Announce a host lifecycle event (`HOST_EVENT`, `docs/protocol/v2-design.md`
/// §10) to every participant, e.g. before a restart ends their sessions.
pub(crate) fn broadcast_host_event(kind: i32) {
    publish_host_event(kind, "", None);
}

fn publish_host_event(kind: i32, actor: &str, except: Option<u64>) {
    let frame = crate::protocol::host_event::encode(kind, actor);
    {
        let mut event = HUB.event.lock().unwrap_or_else(|p| p.into_inner());
        let sequence = event.as_ref().map_or(1, |(sequence, _, _)| sequence + 1);
        *event = Some((sequence, frame, except));
    }
    bump(&HUB.members);
}

/// Participant `actor` changed the host's displays; the others are told
/// (`DISPLAY_CHANGING`) why their picture is about to change.
pub(crate) fn display_changed(actor: Option<u64>, nickname: &str) {
    publish_host_event(
        crate::protocol::host_event::kind::DISPLAY_CHANGING,
        nickname.trim(),
        actor,
    );
}

/// The latest host event newer than `seen`, and the participant it skips.
pub(crate) fn host_event_after(seen: u64) -> Option<(u64, Vec<u8>, Option<u64>)> {
    HUB.event
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .filter(|(sequence, _, _)| *sequence > seen)
        .cloned()
}

/// A participant's capabilities changed; republish the list. The holder
/// is re-checked by [`validate_holder`] (callers may hold the access lock,
/// which the capability check needs).
pub(crate) fn touch() {
    bump(&HUB.members);
}

/// The participant holding the control token.
pub(crate) fn holder() -> Option<u64> {
    state().controlling
}

/// Whether `key` holds the control token.
pub(crate) fn holds_control(key: Option<u64>) -> bool {
    key.is_some() && state().controlling == key
}

fn set_holder(state: &mut State, key: Option<u64>) -> bool {
    if state.controlling == key {
        return false;
    }
    tracing::info!(previous = ?state.controlling, holder = ?key, "host control token changed");
    state.controlling = key;
    true
}

fn changed() {
    bump(&HUB.members);
    bump(&HUB.pointers);
}

/// Why the control token was not given.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// Not a member, or without the `control` capability.
    NotAllowed,
    /// Another participant holds it (its nickname).
    HeldBy(String),
    /// An automatic take (restoring the previous control state) while
    /// several participants are connected: they take control by hand.
    Shared,
}

/// Take the control token for participant `key`. It succeeds when nobody
/// holds it (or `key` already does) and the participant holds the
/// `control` capability. An `automatic` take, not asked for by the user,
/// succeeds only for the holder or a participant alone in the session.
pub(crate) fn take_control(key: u64, automatic: bool) -> Result<(), Refusal> {
    let control = state().members.get(&key).map(|m| m.control.clone());
    let Some(control) = control else {
        return Err(Refusal::NotAllowed);
    };
    // Outside the state lock: the capability check locks the access state.
    if !control() {
        return Err(Refusal::NotAllowed);
    }
    let mut state = state();
    match state.controlling {
        Some(holder) if holder == key => Ok(()),
        Some(holder) if state.members.contains_key(&holder) => {
            let name = state.members[&holder].profile.nickname.clone();
            Err(Refusal::HeldBy(if name.trim().is_empty() {
                t().features.host_other_participant.into()
            } else {
                name
            }))
        }
        _ if automatic && state.members.len() > 1 => Err(Refusal::Shared),
        _ => {
            let changed_now = set_holder(&mut state, Some(key));
            drop(state);
            if changed_now {
                changed();
            }
            Ok(())
        }
    }
}

/// Participant `key` gives the control token back. Returns whether it held it.
pub(crate) fn release_control(key: u64) -> bool {
    let mut state = state();
    if state.controlling != Some(key) {
        return false;
    }
    set_holder(&mut state, None);
    drop(state);
    changed();
    true
}

/// Release the token of a holder that left or lost the `control`
/// capability (blocked by the host owner, grant withdrawn).
pub(crate) fn validate_holder() {
    let (holder, control) = {
        let state = state();
        let Some(holder) = state.controlling else {
            return;
        };
        (holder, state.members.get(&holder).map(|m| m.control.clone()))
    };
    if control.is_some_and(|control| control()) {
        return;
    }
    let mut state = state();
    if state.controlling == Some(holder) {
        set_holder(&mut state, None);
        drop(state);
        changed();
    }
}

/// Whether participant `key` watches the shared viewer stream: several
/// participants are connected and `key` does not hold the control token.
/// The holder (or a participant on its own) keeps its own settings.
/// Whether participant `key` watches the shared stream: others watch the
/// picture too (those listening only do not count) and it does not hold the
/// control token.
pub(crate) fn shared_viewer(key: Option<u64>) -> bool {
    let state = state();
    state.members.values().filter(|m| m.watching).count() > 1
        && (key.is_none() || state.controlling != key)
}

/// Participant `key` watches the picture again, or only listens (audio
/// only).
pub(crate) fn set_watching(key: u64, watching: bool) {
    let mut state = state();
    let Some(member) = state.members.get_mut(&key) else {
        return;
    };
    if member.watching == watching {
        return;
    }
    member.watching = watching;
    drop(state);
    bump(&HUB.members);
}

/// One participant for the host overlay and the host GUI.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct MemberView {
    pub key: u64,
    pub nickname: String,
    pub device_name: String,
    pub relation: String,
    /// 0xRRGGBB.
    pub color: u32,
    pub control: bool,
    pub controlling: bool,
    pub pointer: Option<PointerView>,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct PointerView {
    pub screen_id: i32,
    /// Normalized position within the screen.
    pub x: f32,
    pub y: f32,
    pub visible: bool,
    /// Time since the pointer last moved.
    pub idle: Duration,
    /// The latest click and how long ago it happened.
    pub click: Option<(Click, Duration)>,
    /// Increments with every click.
    pub clicks: u32,
}

/// Everyone connected, in join order.
pub(crate) fn snapshot() -> Vec<MemberView> {
    let state = state();
    let now = Instant::now();
    state
        .members
        .iter()
        .map(|(key, member)| MemberView {
            key: *key,
            nickname: member.profile.nickname.clone(),
            device_name: member.profile.device_name.clone(),
            relation: member.profile.relation.clone(),
            color: member.color,
            control: (member.control)(),
            controlling: state.controlling == Some(*key),
            pointer: member.pointer.as_ref().map(|last| PointerView {
                screen_id: last.pointer.screen_id,
                x: last.pointer.x,
                y: last.pointer.y,
                visible: last.pointer.visible,
                idle: now.saturating_duration_since(last.moved),
                click: last
                    .click
                    .map(|(click, at)| (click, now.saturating_duration_since(at))),
                clicks: last.clicks,
            }),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::participants::{button, phase};
    use prost::Message as _;

    /// The hub is process-wide; tests that join must not overlap.
    static SERIAL: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn control_token_is_first_come_until_released() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let alice_control = allowed.clone();
        let alice = join(9101, Profile {
            nickname: "alice".into(),
            ..Default::default()
        }, move || alice_control.load(std::sync::atomic::Ordering::Relaxed), true);
        let bob = join(9102, Profile {
            nickname: "bob".into(),
            ..Default::default()
        }, || true, true);
        let viewer = join(9103, Profile::default(), || false, true);
        // Nobody holds it: everyone watches the shared stream.
        assert!(shared_viewer(Some(9101)) && shared_viewer(Some(9102)));
        // With others present, restoring control is not taking it.
        assert_eq!(take_control(9101, true), Err(Refusal::Shared));
        assert!(take_control(9101, false).is_ok());
        assert!(take_control(9101, false).is_ok(), "taking again is idempotent");
        assert!(take_control(9101, true).is_ok(), "the holder restores it");
        assert_eq!(take_control(9102, false), Err(Refusal::HeldBy("alice".into())));
        assert_eq!(take_control(9103, false), Err(Refusal::NotAllowed), "no control capability");
        assert!(!shared_viewer(Some(9101)) && shared_viewer(Some(9102)));
        assert!(holds_control(Some(9101)) && !holds_control(Some(9102)));
        // Only the holder can release it.
        assert!(!release_control(9102));
        assert!(release_control(9101));
        assert!(take_control(9102, false).is_ok());
        assert_eq!(take_control(9101, false), Err(Refusal::HeldBy("bob".into())));
        // Leaving releases it.
        drop(bob);
        assert_eq!(holder(), None);
        assert!(take_control(9101, false).is_ok());
        // Losing the capability releases it.
        allowed.store(false, std::sync::atomic::Ordering::Relaxed);
        validate_holder();
        assert_eq!(holder(), None);
        drop(viewer);
        // Alone, a participant keeps its own settings and restores control.
        assert!(!shared_viewer(Some(9101)));
        allowed.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(take_control(9101, true).is_ok());
        drop(alice);
    }

    #[test]
    fn those_only_listening_do_not_share_the_stream() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let viewer = join(9201, Profile::default(), || true, true);
        let listener = join(9202, Profile::default(), || true, false);
        // Watching alone: its own settings.
        assert!(!shared_viewer(Some(9201)));
        set_watching(9202, true);
        assert!(shared_viewer(Some(9201)) && shared_viewer(Some(9202)));
        set_watching(9202, false);
        assert!(!shared_viewer(Some(9201)));
        drop(listener);
        drop(viewer);
    }

    #[test]
    fn members_see_each_other_and_their_own_flag() {
        let _serial = SERIAL.lock().unwrap_or_else(|p| p.into_inner());
        let a = join(9001, Profile {
            nickname: "alice".into(),
            ..Default::default()
        }, || true, true);
        let b = join(9002, Profile {
            nickname: "bob".into(),
            ..Default::default()
        }, || false, true);
        assert!(take_control(9001, false).is_ok());
        let frame = b.participants_frame();
        let body = crate::protocol::peer::body_of(crate::protocol::peer::FrameType::Participants, &frame).unwrap();
        let list = Participants::decode(body).unwrap();
        let ours: Vec<_> = list.participants.iter().filter(|p| p.id >= 9001).collect();
        assert_eq!(ours.len(), 2);
        assert!(ours.iter().any(|p| p.id == 9002 && p.you && !p.control));
        assert!(ours.iter().any(|p| p.id == 9001 && !p.you && p.control));
        assert_eq!(list.controlling, 9001);

        a.pointer(Pointer {
            seq: 1,
            screen_id: 0,
            x: 0.5,
            y: 0.5,
            visible: true,
            click: Some(Click {
                button: button::LEFT,
                phase: phase::DOWN,
            }),
        });
        let view = snapshot().into_iter().find(|m| m.key == 9001).unwrap();
        assert!(view.controlling);
        assert_eq!(view.pointer.unwrap().clicks, 1);
        drop(a);
        assert!(snapshot().iter().all(|m| m.key != 9001));
        drop(b);
    }
}
