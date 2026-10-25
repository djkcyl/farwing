//! Shared encoders (`docs/protocol/v2-design.md` §4.2): participants that
//! watch the same screen with the same parameters share one encoder.
//!
//! Every participant keeps its own capture thread, transport, packetizer and
//! RTP clock. Within a group one thread (the leader) captures and encodes;
//! the others (followers) receive the encoded frames, rebase them onto their
//! own RTP clock and send them. Each member votes its allowance every
//! iteration; the leader encodes at the minimum of the active votes. When the
//! leader leaves, the earliest remaining member takes over.
//!
//! Multi-viewer forwarding (`docs/protocol/deployment-design.md` §5.4): a
//! participant whose picture goes through the server's forwarder is marked
//! forwarded and takes no frames itself; the uplink to the forwarder watches
//! the group as an observer, which takes frames but never leads. A group of
//! observers only has ended.
use crate::features::host::encoder::{Encoded, FrameTiming};
use crate::features::host::format::{Backend, Format};
use crate::platform::windows::cursor_shape::Snapshot;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Distinct encoders per process.
pub(super) const MAX_ENCODERS: usize = 4;
/// Encoded frames buffered per follower; a follower that falls further
/// behind loses frames and waits for a keyframe.
const FOLLOWER_QUEUE: usize = 3;

/// What makes two participants' video identical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Key {
    /// Output device name.
    pub screen: String,
    pub adapter: u64,
    pub backend: Backend,
    pub format: Format,
    /// HDR as requested (the source decides whether it is used).
    pub hdr: bool,
    /// Encoder size bound (the controller's scale limit).
    pub maximum: (u32, u32),
    /// Frame-rate tier before the weak-network strategy and decoder cap.
    pub fps: u32,
    pub cursor: bool,
    pub cinema: bool,
    /// Quality tier (automatic resolved) and, for custom quality, its bitrate.
    pub quality: i32,
    pub bitrate: u32,
    /// Non-zero: this member encodes alone for now (a lossy keyframe-only
    /// viewer), each isolated member with its own number.
    pub isolated: u64,
    /// The forwarder's low layer (`deployment-design.md` §5.5): a smaller,
    /// slower stream for viewers whose downlinks cannot carry the main one.
    pub low: bool,
}

/// A member's allowance for the next frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Vote {
    /// Bandwidth the member's transport can take now (bit/s).
    pub peak: u32,
    /// Congestion-window reduction ratio of the member's transport.
    pub ratio: f64,
    /// Frame rate after the member's weak-network strategy and decoder cap;
    /// 0: no limit of its own (the forwarder uplink).
    pub fps: u32,
}

/// What the group encodes for: the minimum over the votes, ignoring members
/// whose transport is paused (below `minimum`) unless all are.
pub(super) fn aggregate(votes: impl IntoIterator<Item = Vote>, minimum: u32) -> Option<Vote> {
    let votes: Vec<Vote> = votes.into_iter().collect();
    let usable: Vec<Vote> = votes.iter().copied().filter(|v| v.peak >= minimum).collect();
    let pool = if usable.is_empty() { votes } else { usable };
    let first = *pool.first()?;
    let all = pool.iter().fold(first, |all, vote| Vote {
        peak: all.peak.min(vote.peak),
        ratio: all.ratio.max(vote.ratio),
        fps: all.fps,
    });
    Some(Vote {
        fps: pool.iter().map(|v| v.fps).filter(|f| *f > 0).min().unwrap_or(0),
        ..all
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Choice {
    /// A group with the same key.
    Exact(usize),
    /// The encoder limit is reached: the closest group on the same screen.
    Closest(usize),
    Create,
    /// The limit is reached and no group can stand in (other screens or
    /// codecs only): a new encoder anyway.
    OverLimit,
}

fn area(size: (u32, u32)) -> u64 {
    u64::from(size.0) * u64::from(size.1)
}

/// Where a participant with `wanted` (approximate output `size`) goes.
pub(super) fn choose(groups: &[(Key, (u32, u32))], wanted: &Key, size: (u32, u32), limit: usize) -> Choice {
    if let Some(index) = groups.iter().position(|(key, _)| key == wanted) {
        return Choice::Exact(index);
    }
    if groups.len() < limit {
        return Choice::Create;
    }
    // Same screen and stream format; prefer the same encoder, then the
    // largest picture not larger than wanted, then the nearest size, then
    // the nearest frame rate (several quality tiers often share one size).
    groups
        .iter()
        .enumerate()
        .filter(|(_, (key, _))| {
            key.screen == wanted.screen && key.format == wanted.format && key.low == wanted.low
        })
        .min_by_key(|(_, (key, group_size))| {
            let same_encoder = key.adapter == wanted.adapter && key.backend == wanted.backend;
            let larger = area(*group_size) > area(size);
            let distance = area(*group_size).abs_diff(area(size));
            (!same_encoder, larger, distance, key.fps.abs_diff(wanted.fps))
        })
        .map_or(Choice::OverLimit, |(index, _)| Choice::Closest(index))
}

/// One encoded frame for the followers. The capture instant, not the
/// leader's RTP timestamp, travels: each follower has its own RTP clock.
pub(super) struct Shared {
    sequence: u64,
    pub data: Arc<[u8]>,
    pub keyframe: bool,
    pub captured: Instant,
    pub is_new: bool,
    pub timing: Option<FrameTiming>,
    pub format: Format,
    pub color: crate::media::video_color::VideoColorSpace,
    /// The leader's frame identity; followers forward it unchanged.
    pub meta: Option<crate::media::frame_meta::FrameMeta>,
}
impl Shared {
    /// `frame` as encoded on a clock that started at `started`.
    pub(super) fn new(frame: &Encoded, started: Instant) -> Self {
        let captured = started + Duration::from_nanos(frame.timestamp_100ns.max(0) as u64 * 100);
        Self {
            sequence: 0,
            data: Arc::from(frame.data.as_slice()),
            keyframe: frame.keyframe,
            captured,
            is_new: frame.is_new,
            timing: frame.timing,
            format: frame.format,
            color: frame.color,
            meta: frame.meta,
        }
    }
    /// The frame on a member clock that started at `started`; `None` for a
    /// frame captured before that clock existed.
    pub(super) fn encoded(&self, started: Instant) -> Option<Encoded> {
        let since = self.captured.checked_duration_since(started)?;
        Some(Encoded {
            data: self.data.to_vec(),
            keyframe: self.keyframe,
            timestamp_100ns: (since.as_nanos() / 100) as i64,
            is_new: self.is_new,
            timing: self.timing,
            format: self.format,
            color: self.color,
            meta: self.meta,
            ltr: Default::default(),
        })
    }
}

/// What the leader reports about the group's stream, for the followers'
/// diagnostics and status pushes.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Output {
    pub implementation: i32,
    pub maximum_size: (u32, u32),
    pub capture: String,
    pub screen: crate::features::host::capture::Screen,
    pub quality: i32,
    pub fps: u32,
    pub format: Format,
    pub budget_size: (u32, u32),
    pub backend: Backend,
    pub video: crate::features::host::ActiveEncoding,
}

struct Member {
    /// Takes frames for the forwarder uplink; never leads.
    observer: bool,
    /// While this member's picture goes through the forwarder: its
    /// participant and track.
    forwarded: Option<(u64, usize)>,
    vote: Option<Vote>,
    keyframe: bool,
    outbox: SyncSender<Arc<Shared>>,
    /// Long-term references this member's viewer holds, and when it said so.
    held: Option<(crate::features::host::recovery::Held, Instant)>,
    /// An unanswered recovery request of this member's viewer.
    recovery: Option<crate::features::host::recovery::Request>,
}

struct Inner {
    /// Approximate output size, for the closest-group choice.
    size: (u32, u32),
    leader: Option<u64>,
    /// By member number, i.e. by joining order.
    members: BTreeMap<u64, Member>,
    sequence: u64,
    /// Never reset, so followers notice a new leader's first description.
    output_revision: u64,
    output: Option<(u64, Output)>,
    cursor: Option<Snapshot>,
}

pub(super) struct Group {
    key: Key,
    /// Unique per process, for the uplink's track assignment.
    serial: u64,
    inner: Mutex<Inner>,
}
impl Group {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[derive(Default)]
pub(super) struct Registry {
    groups: Mutex<Vec<Arc<Group>>>,
    next: AtomicU64,
}

/// A group with forwarded members, for the forwarder uplink.
#[derive(Clone)]
pub(super) struct Forwarded {
    pub group: Arc<Group>,
    /// `(participant, track)` of each forwarded member.
    pub members: Vec<(u64, usize)>,
}

impl Group {
    pub(super) fn serial(&self) -> u64 {
        self.serial
    }
    pub(super) fn key(&self) -> &Key {
        &self.key
    }
}

pub(super) static REGISTRY: LazyLock<Arc<Registry>> = LazyLock::new(Arc::default);

impl Registry {
    fn groups(&self) -> MutexGuard<'_, Vec<Arc<Group>>> {
        self.groups.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Join the group for `key` (or the closest one at the encoder limit),
    /// or start one and lead it.
    pub(super) fn join(self: &Arc<Self>, key: Key, size: (u32, u32), limit: usize) -> Membership {
        self.join_with(key, size, limit, false)
    }

    /// A forwarded participant: share the closest group of the same screen
    /// and format (all forwarded viewers get one stream), else lead one.
    pub(super) fn join_forwarded(self: &Arc<Self>, key: Key, size: (u32, u32)) -> Membership {
        let mut membership = self.join_with(key, size, 0, true);
        membership.provisional = false;
        membership
    }

    /// A participant whose own settings have not arrived yet: share the
    /// closest group of the same screen and format rather than open an
    /// encoder for parameters about to change.
    pub(super) fn join_provisional(self: &Arc<Self>, key: Key, size: (u32, u32)) -> Membership {
        self.join_with(key, size, 0, true)
    }

    fn join_with(self: &Arc<Self>, key: Key, size: (u32, u32), limit: usize, provisional: bool) -> Membership {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (outbox, inbox) = sync_channel(FOLLOWER_QUEUE);
        let member = Member {
            observer: false,
            forwarded: None,
            vote: None,
            keyframe: false,
            outbox,
            held: None,
            recovery: None,
        };
        let mut groups = self.groups();
        let existing: Vec<(Key, (u32, u32))> = groups
            .iter()
            .map(|group| (group.key.clone(), group.lock().size))
            .collect();
        let choice = match choose(&existing, &key, size, limit) {
            Choice::OverLimit if provisional => Choice::Create,
            choice => choice,
        };
        let shares = matches!(choice, Choice::Closest(_)) && provisional;
        let group = match choice {
            Choice::Exact(index) | Choice::Closest(index) => {
                let group = groups[index].clone();
                let mut inner = group.lock();
                // The newcomer asks for its keyframe once it takes frames:
                // one made now would reach nobody.
                inner.members.insert(id, member);
                drop(inner);
                if matches!(choice, Choice::Exact(_)) {
                    tracing::info!(
                        screen = %key.screen,
                        quality = key.quality,
                        fps = key.fps,
                        "participant joined a shared encoder as a follower"
                    );
                } else if shares {
                    tracing::info!(
                        screen = %key.screen,
                        sharing = ?group.key,
                        "participant shares an encoder until its own settings arrive"
                    );
                } else {
                    tracing::info!(
                        screen = %key.screen,
                        wanted = ?key,
                        sharing = ?group.key,
                        "encoder limit reached; participant shares the closest encoder"
                    );
                }
                group
            }
            Choice::Create | Choice::OverLimit => {
                if choice == Choice::OverLimit {
                    tracing::warn!(
                        screen = %key.screen,
                        encoders = groups.len(),
                        "encoder limit reached but no encoder can be shared; opening another"
                    );
                } else {
                    tracing::info!(
                        ?key,
                        encoders = groups.len() + 1,
                        "participant leads a new shared encoder group"
                    );
                }
                let group = Arc::new(Group {
                    key: key.clone(),
                    serial: id,
                    inner: Mutex::new(Inner {
                        size,
                        leader: Some(id),
                        members: BTreeMap::from([(id, member)]),
                        sequence: 0,
                        output_revision: 0,
                        output: None,
                        cursor: None,
                    }),
                });
                groups.push(group.clone());
                group
            }
        };
        drop(groups);
        Membership {
            registry: self.clone(),
            group,
            id,
            wanted: key,
            provisional: shares,
            inbox,
            last_sequence: None,
            output_seen: 0,
        }
    }

    /// Groups with forwarded members, in creation order.
    pub(super) fn forwarded(&self) -> Vec<Forwarded> {
        self.groups()
            .iter()
            .filter_map(|group| {
                let members: Vec<(u64, usize)> =
                    group.lock().members.values().filter_map(|m| m.forwarded).collect();
                (!members.is_empty()).then(|| Forwarded {
                    group: group.clone(),
                    members,
                })
            })
            .collect()
    }

    /// Watch `group` for the forwarder: take frames, never lead.
    pub(super) fn observe(self: &Arc<Self>, group: &Arc<Group>) -> Membership {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (outbox, inbox) = sync_channel(FOLLOWER_QUEUE);
        let mut inner = group.lock();
        if let Some(leader) = inner.leader.and_then(|leader| inner.members.get_mut(&leader)) {
            leader.keyframe = true;
        }
        inner.members.insert(
            id,
            Member {
                observer: true,
                forwarded: None,
                vote: None,
                keyframe: false,
                outbox,
                held: None,
                recovery: None,
            },
        );
        drop(inner);
        Membership {
            registry: self.clone(),
            group: group.clone(),
            id,
            wanted: group.key.clone(),
            provisional: false,
            inbox,
            last_sequence: None,
            output_seen: 0,
        }
    }

    fn leave(&self, group: &Arc<Group>, id: u64) {
        let mut groups = self.groups();
        let mut inner = group.lock();
        inner.members.remove(&id);
        if inner.leader == Some(id) {
            inner.leader = inner
                .members
                .iter()
                .find(|(_, member)| !member.observer)
                .map(|(id, _)| *id);
            inner.output = None;
            inner.cursor = None;
            if let Some(leader) = inner.leader {
                tracing::info!(
                    screen = %group.key.screen,
                    members = inner.members.len(),
                    leader,
                    "shared encoder group has a new leader"
                );
            }
        }
        // Observers alone do not keep a group: nobody would encode for it.
        let empty = inner.members.values().all(|member| member.observer);
        drop(inner);
        if empty {
            groups.retain(|other| !Arc::ptr_eq(other, group));
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.groups().len()
    }
}

/// A participant's place in a group; leaves on drop.
pub(super) struct Membership {
    registry: Arc<Registry>,
    group: Arc<Group>,
    id: u64,
    /// The key this participant asked for (the group's may differ at the
    /// encoder limit).
    pub wanted: Key,
    /// Shares the closest group only until this participant's own settings
    /// arrive.
    pub provisional: bool,
    inbox: Receiver<Arc<Shared>>,
    last_sequence: Option<u64>,
    output_seen: u64,
}

/// A frame received by a follower.
pub(super) struct Received {
    pub frame: Arc<Shared>,
    /// Frames were lost before this one (the follower fell behind or the
    /// leader changed mid-stream): decoding must restart at a keyframe.
    pub gap: bool,
}

impl Membership {
    pub(super) fn is_leader(&self) -> bool {
        self.group.lock().leader == Some(self.id)
    }
    /// The key of the group this member is in.
    pub(super) fn group_key(&self) -> &Key {
        &self.group.key
    }
    /// Whether this member's picture goes through the forwarder (its
    /// participant and track), for the uplink.
    pub(super) fn set_forwarded(&self, forwarded: Option<(u64, usize)>) {
        let mut inner = self.group.lock();
        let Some(member) = inner.members.get_mut(&self.id) else { return };
        let newly = forwarded.is_some() && member.forwarded != forwarded;
        member.forwarded = forwarded;
        drop(inner);
        if newly && self.group.key.low {
            // The low layer's uplink track takes this group's frames now
            // rather than on the next tick. (Other groups wait for it: the
            // members of a group that changes format move over within it,
            // and its track goes to the new group.)
            crate::features::host::forwarding::wake();
        }
    }
    /// Observer: the group ended (nobody left to encode).
    pub(super) fn ended(&self) -> bool {
        self.group.lock().leader.is_none()
    }
    #[cfg(test)]
    pub(super) fn group_serial(&self) -> u64 {
        self.group.serial
    }
    /// This member's allowance; `None` while it cannot take frames.
    pub(super) fn vote(&self, vote: Option<Vote>) {
        if let Some(member) = self.group.lock().members.get_mut(&self.id) {
            member.vote = vote;
        }
    }
    /// Ask the leader for a keyframe.
    pub(super) fn request_keyframe(&self) {
        let mut inner = self.group.lock();
        if let Some(leader) = inner.leader
            && let Some(member) = inner.members.get_mut(&leader)
        {
            member.keyframe = true;
        }
    }
    /// Leader: whether any member asked for a keyframe since the last call.
    pub(super) fn take_keyframe(&self) -> bool {
        let mut inner = self.group.lock();
        let mut wanted = false;
        for member in inner.members.values_mut() {
            wanted |= std::mem::take(&mut member.keyframe);
        }
        wanted
    }
    /// This member's viewer feedback, for the leader's reference decisions
    /// (`docs/protocol/rfi-ltr-brief.md` §4.2).
    pub(super) fn report(
        &self,
        held: Option<crate::features::host::recovery::Held>,
        request: Option<crate::features::host::recovery::Request>,
    ) {
        let mut inner = self.group.lock();
        if let Some(member) = inner.members.get_mut(&self.id) {
            if let Some(held) = held {
                member.held = Some((held, Instant::now()));
            }
            if request.is_some() {
                member.recovery = request;
            }
        }
    }
    /// Leader: every member's recent acknowledgement (at most 2 s old: a
    /// member that stops acknowledging does not hold the others back) and
    /// the unanswered recovery requests.
    pub(super) fn take_feedback(
        &self,
    ) -> (
        Vec<crate::features::host::recovery::Held>,
        Vec<crate::features::host::recovery::Request>,
    ) {
        let mut inner = self.group.lock();
        let mut held = Vec::new();
        let mut requests = Vec::new();
        for member in inner.members.values_mut() {
            if let Some((h, at)) = &member.held
                && at.elapsed() <= Duration::from_secs(2)
            {
                held.push(h.clone());
            }
            requests.extend(member.recovery.take());
        }
        (held, requests)
    }
    /// The group's allowance over its active members.
    pub(super) fn aggregate(&self, minimum: u32) -> Option<Vote> {
        aggregate(self.group.lock().members.values().filter_map(|m| m.vote), minimum)
    }
    /// Whether a member other than this one takes frames.
    pub(super) fn others_active(&self) -> bool {
        self.group
            .lock()
            .members
            .iter()
            .any(|(id, member)| *id != self.id && member.vote.is_some())
    }
    /// Whether other members are in the group at all.
    pub(super) fn has_others(&self) -> bool {
        self.group.lock().members.len() > 1
    }
    /// Leader: hand one encoded frame to every follower. A follower whose
    /// queue is full misses it and notices the gap.
    pub(super) fn publish(&self, mut frame: Shared) {
        let mut inner = self.group.lock();
        inner.sequence += 1;
        frame.sequence = inner.sequence;
        let frame = Arc::new(frame);
        for (id, member) in &inner.members {
            if *id == self.id {
                continue;
            }
            match member.outbox.try_send(frame.clone()) {
                Ok(()) | Err(TrySendError::Full(_)) | Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }
    /// Leader: the stream's current description.
    pub(super) fn set_output(&self, output: Output) {
        let mut inner = self.group.lock();
        if inner.output.as_ref().is_some_and(|(_, current)| *current == output) {
            return;
        }
        inner.output_revision += 1;
        inner.output = Some((inner.output_revision, output));
    }
    /// Follower: the stream description, when it changed since last asked.
    pub(super) fn output_changed(&mut self) -> Option<Output> {
        let inner = self.group.lock();
        let (revision, output) = inner.output.as_ref()?;
        if *revision == self.output_seen {
            return None;
        }
        self.output_seen = *revision;
        Some(output.clone())
    }
    pub(super) fn set_size(&self, size: (u32, u32)) {
        self.group.lock().size = size;
    }
    pub(super) fn set_cursor(&self, cursor: Option<Snapshot>) {
        self.group.lock().cursor = cursor;
    }
    pub(super) fn cursor(&self) -> Option<Snapshot> {
        self.group.lock().cursor.clone()
    }
    /// Follower: the next frame, waiting at most `timeout`.
    pub(super) fn receive(&mut self, timeout: Duration) -> Option<Received> {
        let frame = match self.inbox.recv_timeout(timeout) {
            Ok(frame) => frame,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => return None,
        };
        let gap = self
            .last_sequence
            .is_some_and(|last| frame.sequence != last.wrapping_add(1));
        self.last_sequence = Some(frame.sequence);
        Some(Received { frame, gap })
    }
    /// Follower becoming leader: frames queued from the old leader are stale.
    pub(super) fn discard_queued(&mut self) {
        while self.inbox.try_recv().is_ok() {}
        self.last_sequence = None;
        self.output_seen = 0;
    }
}

impl Drop for Membership {
    fn drop(&mut self) {
        self.registry.leave(&self.group, self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(screen: &str, quality: i32) -> Key {
        Key {
            screen: screen.into(),
            adapter: 1,
            backend: Backend::Nvidia,
            format: Format::AVC,
            hdr: false,
            maximum: (3840, 2160),
            fps: 60,
            cursor: true,
            cinema: false,
            quality,
            bitrate: 0,
            isolated: 0,
            low: false,
        }
    }

    fn vote(peak: u32, ratio: f64, fps: u32) -> Vote {
        Vote { peak, ratio, fps }
    }

    const SDR: crate::media::video_color::VideoColorSpace = crate::media::video_color::VideoColorSpace {
        primaries: 1,
        transfer: 1,
        matrix: 1,
        range: 1,
        hdr_metadata: None,
    };

    #[test]
    fn same_key_shares_and_different_key_opens_another() {
        let registry = Arc::new(Registry::default());
        let a = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        let b = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        assert!(a.is_leader());
        assert!(!b.is_leader());
        assert_eq!(registry.len(), 1);
        let c = registry.join(key("D1", 3), (2560, 1440), MAX_ENCODERS);
        assert!(c.is_leader());
        assert_eq!(registry.len(), 2);
        // The newcomer asks for its keyframe once it takes frames.
        assert!(!a.take_keyframe());
        b.request_keyframe();
        assert!(a.take_keyframe());
        assert!(!a.take_keyframe());
    }

    #[test]
    fn newcomers_share_an_encoder_until_their_settings_arrive() {
        let registry = Arc::new(Registry::default());
        let a = registry.join(key("D1", 3), (2560, 1440), MAX_ENCODERS);
        // Start parameters differ (no cursor yet): follow, do not open.
        let mut start = key("D1", 2);
        start.cursor = false;
        let b = registry.join_provisional(start, (1920, 1080));
        assert!(!b.is_leader() && b.provisional);
        assert_eq!(registry.len(), 1);
        // Nothing on that screen to follow: lead a new group as usual.
        let c = registry.join_provisional(key("D2", 2), (1920, 1080));
        assert!(c.is_leader() && !c.provisional);
        assert_eq!(registry.len(), 2);
        drop((a, b, c));
    }

    #[test]
    fn limit_of_four_encoders_shares_the_closest() {
        let registry = Arc::new(Registry::default());
        let held: Vec<_> = (1..=4)
            .map(|quality| {
                let size = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)][quality as usize - 1];
                registry.join(key("D1", quality), size, MAX_ENCODERS)
            })
            .collect();
        assert_eq!(registry.len(), 4);
        let mut fifth = key("D1", 6);
        fifth.bitrate = 20_000_000;
        let extra = registry.join(fifth, (2560, 1440), MAX_ENCODERS);
        assert!(!extra.is_leader());
        assert_eq!(registry.len(), 4);
        assert_eq!(extra.group_key().quality, 3);
        // Another screen cannot share: over the limit.
        let other = registry.join(key("D2", 2), (1920, 1080), MAX_ENCODERS);
        assert!(other.is_leader());
        assert_eq!(registry.len(), 5);
        drop(held);
    }

    #[test]
    fn closest_prefers_same_encoder_then_not_larger_then_nearest() {
        let wanted = key("D1", 3);
        let mut other_encoder = key("D1", 2);
        other_encoder.backend = Backend::Software;
        let groups = vec![
            (other_encoder, (2560, 1440)),
            (key("D1", 4), (3840, 2160)),
            (key("D1", 1), (1280, 720)),
            (key("D2", 3), (2560, 1440)),
        ];
        assert_eq!(choose(&groups, &wanted, (2560, 1440), 4), Choice::Closest(2));
        // Nothing smaller: the nearest larger one.
        let groups = vec![(key("D1", 4), (3840, 2160)), (key("D1", 5), (5120, 2880))];
        assert_eq!(choose(&groups, &wanted, (2560, 1440), 2), Choice::Closest(0));
        // Below the limit a new encoder opens; an exact key always joins.
        assert_eq!(choose(&groups, &wanted, (2560, 1440), 4), Choice::Create);
        assert_eq!(choose(&groups, &key("D1", 5), (5120, 2880), 1), Choice::Exact(1));
        let mut hevc = key("D1", 3);
        hevc.format = Format {
            codec: crate::features::host::format::Codec::H265,
            ..Format::AVC
        };
        assert_eq!(choose(&groups, &hevc, (2560, 1440), 2), Choice::OverLimit);
    }

    #[test]
    fn closest_of_equal_size_prefers_the_nearest_frame_rate() {
        let at = |fps: u32, quality: i32| Key { fps, ..key("D1", quality) };
        let groups = vec![
            (at(30, 2), (1920, 1080)),
            (at(144, 3), (1920, 1080)),
            (at(60, 4), (1920, 1080)),
            (at(144, 2), (2560, 1440)),
        ];
        assert_eq!(choose(&groups, &at(90, 5), (1920, 1080), 4), Choice::Closest(2));
        assert_eq!(choose(&groups, &at(120, 5), (1920, 1080), 4), Choice::Closest(1));
    }

    #[test]
    fn aggregate_takes_the_minimum_of_active_members() {
        assert_eq!(aggregate([], 300_000), None);
        let all = aggregate(
            [vote(8_000_000, 0.0, 60), vote(3_000_000, 0.2, 144), vote(5_000_000, 0.1, 30)],
            300_000,
        )
        .unwrap();
        assert_eq!(all, vote(3_000_000, 0.2, 30));
        // A paused member does not stall the others.
        let paused = aggregate([vote(8_000_000, 0.0, 60), vote(0, 0.0, 60)], 300_000).unwrap();
        assert_eq!(paused.peak, 8_000_000);
        // Unless everyone is paused.
        assert_eq!(aggregate([vote(0, 0.0, 60)], 300_000).unwrap().peak, 0);
        // The forwarder uplink limits the rate, not the frame rate.
        let uplink = aggregate([vote(5_000_000, 0.0, 60), vote(3_000_000, 0.0, 0)], 300_000).unwrap();
        assert_eq!((uplink.peak, uplink.fps), (3_000_000, 60));
        assert_eq!(aggregate([vote(3_000_000, 0.0, 0)], 300_000).unwrap().fps, 0);
    }

    #[test]
    fn leader_hands_over_and_empty_groups_close() {
        let registry = Arc::new(Registry::default());
        let a = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        let mut b = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        let c = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        a.vote(Some(vote(5_000_000, 0.0, 60)));
        b.vote(Some(vote(2_000_000, 0.0, 60)));
        assert_eq!(a.aggregate(300_000).unwrap().peak, 2_000_000);
        assert!(a.others_active());
        let started = Instant::now();
        let frame = Encoded {
            data: vec![1, 2, 3],
            keyframe: true,
            timestamp_100ns: 10_000,
            is_new: true,
            timing: None,
            meta: None,
            ltr: Default::default(),
            format: Format::AVC,
            color: SDR,
        };
        a.publish(Shared::new(&frame, started));
        let received = b.receive(Duration::from_millis(10)).unwrap();
        assert!(!received.gap);
        assert_eq!(received.frame.encoded(started).unwrap().timestamp_100ns, 10_000);
        // A clock that started later sees the frame earlier on its timeline,
        // and a frame from before it started not at all.
        let later = started + Duration::from_micros(500);
        assert_eq!(received.frame.encoded(later).unwrap().timestamp_100ns, 5_000);
        assert!(received.frame.encoded(started + Duration::from_secs(1)).is_none());
        drop(a);
        assert!(b.is_leader());
        assert!(!c.is_leader());
        drop(b);
        assert!(c.is_leader());
        drop(c);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn a_full_follower_queue_shows_up_as_a_gap() {
        let registry = Arc::new(Registry::default());
        let leader = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        let mut follower = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        let started = Instant::now();
        let frame = |keyframe| Encoded {
            data: vec![0; 4],
            keyframe,
            timestamp_100ns: 1,
            is_new: true,
            timing: None,
            meta: None,
            ltr: Default::default(),
            format: Format::AVC,
            color: SDR,
        };
        for _ in 0..FOLLOWER_QUEUE + 2 {
            leader.publish(Shared::new(&frame(false), started));
        }
        for _ in 0..FOLLOWER_QUEUE {
            assert!(!follower.receive(Duration::from_millis(1)).unwrap().gap);
        }
        leader.publish(Shared::new(&frame(true), started));
        let next = follower.receive(Duration::from_millis(1)).unwrap();
        assert!(next.gap && next.frame.keyframe);
    }

    #[test]
    fn observers_take_frames_but_never_lead() {
        let registry = Arc::new(Registry::default());
        let leader = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        let follower = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS);
        assert!(registry.forwarded().is_empty());
        follower.set_forwarded(Some((7, 0)));
        let forwarded = registry.forwarded();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].members, [(7, 0)]);
        let mut observer = registry.observe(&forwarded[0].group);
        assert_eq!(observer.group_serial(), leader.group_serial());
        // The observer asked for a keyframe and counts as an active member.
        assert!(leader.take_keyframe());
        observer.vote(Some(vote(4_000_000, 0.0, 60)));
        assert!(leader.others_active());
        let started = Instant::now();
        let frame = Encoded {
            data: vec![9],
            keyframe: true,
            timestamp_100ns: 1,
            is_new: true,
            timing: None,
            meta: None,
            ltr: Default::default(),
            format: Format::AVC,
            color: SDR,
        };
        leader.publish(Shared::new(&frame, started));
        assert!(observer.receive(Duration::from_millis(10)).is_some());
        // The leader leaves: the follower leads, not the observer.
        drop(leader);
        assert!(follower.is_leader());
        assert!(!observer.is_leader() && !observer.ended());
        // Only the observer is left: the group has ended.
        drop(follower);
        assert!(observer.ended());
        assert_eq!(registry.len(), 0);
        assert!(registry.forwarded().is_empty());
        drop(observer);
    }
}
