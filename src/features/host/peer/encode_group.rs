//! Shared encoders (`docs/protocol/v2-design.md` §4.2): every stream is one
//! encoder pipeline ([`super::pipeline`]), and participants that watch the
//! same screen with the same parameters are members of one.
//!
//! The pipeline thread captures, encodes and hands every frame to each
//! member on that member's own RTP clock. Members keep their transports,
//! packetizers and clocks; they vote their allowance, ask for keyframes and
//! report their viewers' references. The pipeline encodes at the minimum of
//! the active votes and lives while it has members.
//!
//! Multi-viewer forwarding (`docs/protocol/deployment-design.md` §5.4): a
//! participant whose picture goes through the server's forwarder is marked
//! forwarded and takes no frames itself; the uplink to the forwarder joins
//! the group as an observer. Observers alone do not keep a pipeline.
use crate::features::host::encoder::Encoded;
use crate::features::host::format::{Backend, Format};
use crate::features::host::parameters::MIN_ENCODER_RATE;
use crate::platform::windows::cursor_shape::Snapshot;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// Distinct encoders per process.
pub(super) const MAX_ENCODERS: usize = 4;

/// A member waiting for a keyframe asks again after this long.
const KEYFRAME_RETRY: Duration = Duration::from_secs(1);

/// What makes two participants' video identical.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Key {
    /// The source (`capture::Source::key`: a screen's output device name).
    pub source: String,
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
    /// The tier (automatic resolved) or the custom bitrate.
    pub target: crate::features::host::parameters::Target,
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
            key.source == wanted.source && key.format == wanted.format && key.low == wanted.low
        })
        .min_by_key(|(_, (key, group_size))| {
            let same_encoder = key.adapter == wanted.adapter && key.backend == wanted.backend;
            let larger = area(*group_size) > area(size);
            let distance = area(*group_size).abs_diff(area(size));
            (!same_encoder, larger, distance, key.fps.abs_diff(wanted.fps))
        })
        .map_or(Choice::OverLimit, |(index, _)| Choice::Closest(index))
}

/// `frame`, encoded on a clock that started at `from`, on a clock that
/// started at `to`; `None` for a frame captured before `to`.
pub(super) fn rebase(frame: &Encoded, from: Instant, to: Instant) -> Option<Encoded> {
    let captured = from + Duration::from_nanos(frame.timestamp_100ns.max(0) as u64 * 100);
    let since = captured.checked_duration_since(to)?;
    Some(Encoded {
        data: frame.data.clone(),
        keyframe: frame.keyframe,
        timestamp_100ns: (since.as_nanos() / 100) as i64,
        is_new: frame.is_new,
        timing: frame.timing,
        format: frame.format,
        color: frame.color,
        meta: frame.meta,
        ltr: Default::default(),
    })
}

/// What the pipeline reports about its stream, for the members'
/// diagnostics and status pushes.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct Output {
    pub implementation: i32,
    pub maximum_size: (u32, u32),
    pub capture: String,
    pub source: crate::features::host::capture::Source,
    pub target: crate::features::host::parameters::Target,
    pub fps: u32,
    pub format: Format,
    pub budget_size: (u32, u32),
    pub backend: Backend,
    /// The source shows a picture (not a locked or lost desktop).
    pub visible: bool,
    /// Without the member's own preference and selection reason.
    pub video: crate::features::host::ActiveEncoding,
}

/// Why a pipeline ended while it still had members.
#[derive(Clone, Debug)]
pub(super) enum End {
    /// Its encoder failed: the members try another candidate.
    Candidate,
    /// The screen is gone.
    SourceGone,
    /// Capture failed for good.
    Failed(String),
    /// Nobody but the forwarder uplink watched any more.
    Stopped,
}

struct Member {
    /// The forwarder uplink: takes frames, does not keep the pipeline.
    observer: bool,
    /// Its viewer presents AV1 at the declared visible size (the padding of
    /// an aligned-only encoder left out).
    av1_visible_size: bool,
    /// While this member's picture goes through the forwarder: its
    /// participant and track.
    forwarded: Option<(u64, usize)>,
    vote: Option<Vote>,
    keyframe: bool,
    /// Where this member's frames go, on its own clock.
    frames: mpsc::Sender<Encoded>,
    clock: Instant,
    /// Decoding must (re)start at a keyframe: it joined, fell behind or did
    /// not take frames.
    awaiting: bool,
    /// When the pipeline last made a keyframe for this member's wait.
    asked: Option<Instant>,
    /// Since when its queue has been full while others took frames.
    blocked: Option<Instant>,
    /// Bytes and frames delivered, and when the last one was.
    bytes: u64,
    last: Option<Instant>,
    /// Long-term references this member's viewer holds, and when it said so.
    held: Option<(crate::features::host::recovery::Held, Instant)>,
    /// An unanswered recovery request of this member's viewer.
    recovery: Option<crate::features::host::recovery::Request>,
    rtt: Option<Duration>,
}

impl Member {
    fn new(frames: mpsc::Sender<Encoded>, clock: Instant, observer: bool, av1_visible_size: bool) -> Self {
        Self {
            observer,
            av1_visible_size,
            forwarded: None,
            vote: None,
            keyframe: false,
            frames,
            clock,
            awaiting: true,
            asked: None,
            blocked: None,
            bytes: 0,
            last: None,
            held: None,
            recovery: None,
            rtt: None,
        }
    }
    /// Takes frames now.
    fn active(&self) -> bool {
        self.vote.is_some_and(|vote| vote.peak >= MIN_ENCODER_RATE)
    }
}

struct Inner {
    /// Approximate output size, for the closest-group choice.
    size: (u32, u32),
    /// By member number, i.e. by joining order.
    members: BTreeMap<u64, Member>,
    output_revision: u64,
    output: Option<(u64, Output)>,
    cursor: Option<Snapshot>,
    end: Option<End>,
}

pub(super) struct Group {
    /// What it encodes; a member alone retunes it ([`Registry::retune`]).
    key: Mutex<Key>,
    /// Unique per process, for the uplink's track assignment.
    serial: u64,
    /// The pipeline stops: no member is left.
    stopped: Arc<AtomicBool>,
    inner: Mutex<Inner>,
}

impl Group {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
    pub(super) fn serial(&self) -> u64 {
        self.serial
    }
    pub(super) fn key(&self) -> Key {
        self.key.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    // ------------------------------------------------- the pipeline's side

    /// Set once the last member left; also aborts a blocked encode.
    pub(super) fn stop_flag(&self) -> Arc<AtomicBool> {
        self.stopped.clone()
    }
    pub(super) fn stopped(&self) -> bool {
        self.stopped.load(Ordering::Acquire)
    }
    /// The pipeline ended; its members leave and try again.
    pub(super) fn end(&self, end: End) {
        self.lock().end = Some(end);
    }
    /// The allowance over the active members; `None` while nobody takes
    /// frames.
    pub(super) fn aggregate(&self) -> Option<Vote> {
        aggregate(
            self.lock().members.values().filter_map(|m| m.vote),
            MIN_ENCODER_RATE,
        )
    }
    /// Whether a keyframe is due: a member asked, or one waits to start.
    pub(super) fn take_keyframe(&self, now: Instant) -> bool {
        let mut inner = self.lock();
        let mut wanted = false;
        for member in inner.members.values_mut() {
            wanted |= std::mem::take(&mut member.keyframe);
            if member.active()
                && member.awaiting
                && member.asked.is_none_or(|at| now.saturating_duration_since(at) >= KEYFRAME_RETRY)
            {
                member.asked = Some(now);
                wanted = true;
            }
        }
        wanted
    }
    /// Every member's recent acknowledgement (at most 2 s old: a member
    /// that stops acknowledging does not hold the others back), the
    /// unanswered recovery requests and the longest round trip.
    pub(super) fn take_feedback(
        &self,
    ) -> (
        Vec<crate::features::host::recovery::Held>,
        Vec<crate::features::host::recovery::Request>,
        Option<Duration>,
    ) {
        let mut inner = self.lock();
        let mut held = Vec::new();
        let mut requests = Vec::new();
        let mut rtt = None::<Duration>;
        for member in inner.members.values_mut() {
            if let Some((h, at)) = &member.held
                && at.elapsed() <= Duration::from_secs(2)
            {
                held.push(h.clone());
            }
            requests.extend(member.recovery.take());
            rtt = rtt.max(member.rtt);
        }
        (held, requests, rtt)
    }
    /// Whether more than one member watches (references must suit all).
    pub(super) fn shared(&self) -> bool {
        self.lock().members.len() > 1
    }
    /// Whether to wait before the next frame: an active member's queue is
    /// full. A member alone holds the pipeline back as long as it needs;
    /// among several, one that stays full for `patience` is left behind
    /// (it restarts at a keyframe).
    pub(super) fn backpressure(&self, now: Instant, patience: Duration) -> bool {
        let mut inner = self.lock();
        let active = inner.members.values().filter(|m| m.active()).count();
        let mut wait = false;
        for member in inner.members.values_mut() {
            let full = member.active() && !member.awaiting && member.frames.capacity() == 0;
            if !full {
                member.blocked = None;
                continue;
            }
            let since = *member.blocked.get_or_insert(now);
            wait |= active == 1 || now.saturating_duration_since(since) < patience;
        }
        wait
    }
    /// Hand one encoded frame (on the pipeline clock `clock`) to every
    /// member that takes frames and is not waiting for a keyframe.
    pub(super) fn publish(&self, frame: &Encoded, clock: Instant) {
        let mut inner = self.lock();
        let now = Instant::now();
        for member in inner.members.values_mut() {
            if !member.active() {
                member.awaiting = true;
                continue;
            }
            if member.awaiting && !frame.keyframe {
                continue;
            }
            let Some(encoded) = rebase(frame, clock, member.clock) else {
                continue;
            };
            let bytes = encoded.data.len() as u64;
            match member.frames.try_send(encoded) {
                Ok(()) => {
                    if frame.keyframe {
                        member.awaiting = false;
                        member.asked = None;
                    }
                    member.bytes += bytes;
                    member.last = Some(now);
                }
                Err(mpsc::error::TrySendError::Full(_)) => member.awaiting = true,
                Err(mpsc::error::TrySendError::Closed(_)) => {}
            }
        }
    }
    /// The stream's current description.
    pub(super) fn set_output(&self, output: Output) {
        let mut inner = self.lock();
        if inner.output.as_ref().is_some_and(|(_, current)| *current == output) {
            return;
        }
        inner.output_revision += 1;
        inner.output = Some((inner.output_revision, output));
    }
    /// Whether the picture is shown; `false` for a lost or locked desktop.
    pub(super) fn set_visible(&self, visible: bool) {
        let mut inner = self.lock();
        let Some((_, output)) = inner.output.as_ref() else {
            return;
        };
        if output.visible != visible {
            let mut output = output.clone();
            output.visible = visible;
            inner.output_revision += 1;
            inner.output = Some((inner.output_revision, output));
        }
    }
    pub(super) fn set_size(&self, size: (u32, u32)) {
        self.lock().size = size;
    }
    /// Every member's viewer presents AV1 at its declared visible size.
    pub(super) fn av1_visible_size(&self) -> bool {
        self.lock().members.values().all(|m| m.av1_visible_size)
    }
    pub(super) fn set_cursor(&self, cursor: Option<Snapshot>) {
        self.lock().cursor = cursor;
    }
}

/// How a new group's pipeline starts.
type Spawn = dyn Fn(Arc<Group>, super::pipeline::Start) + Send + Sync;

pub(super) struct Registry {
    groups: Mutex<Vec<Arc<Group>>>,
    next: AtomicU64,
    spawn: Box<Spawn>,
}

impl Default for Registry {
    /// A registry whose groups run no pipeline (tests).
    fn default() -> Self {
        Self::new(Box::new(|_, _| {}))
    }
}

/// A group with forwarded members, for the forwarder uplink.
#[derive(Clone)]
pub(super) struct Forwarded {
    pub group: Arc<Group>,
    /// `(participant, track)` of each forwarded member.
    pub members: Vec<(u64, usize)>,
}

pub(super) static REGISTRY: LazyLock<Arc<Registry>> =
    LazyLock::new(|| Arc::new(Registry::new(Box::new(super::pipeline::spawn))));

/// A new member: where its frames go and on which clock.
pub(super) struct Joining {
    pub frames: mpsc::Sender<Encoded>,
    pub clock: Instant,
    /// Its viewer presents AV1 at the declared visible size.
    pub av1_visible_size: bool,
    /// How a new pipeline starts, if this member opens one.
    pub start: super::pipeline::Start,
}

impl Registry {
    fn new(spawn: Box<Spawn>) -> Self {
        Self {
            groups: Mutex::default(),
            next: AtomicU64::new(1),
            spawn,
        }
    }

    fn groups(&self) -> MutexGuard<'_, Vec<Arc<Group>>> {
        self.groups.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Join the group for `key` (or the closest one at the encoder limit),
    /// or open one.
    pub(super) fn join(self: &Arc<Self>, key: Key, size: (u32, u32), limit: usize, joining: Joining) -> Membership {
        self.join_with(key, size, limit, false, joining)
    }

    /// A forwarded participant: share the closest group of the same screen
    /// and format (all forwarded viewers get one stream), else open one.
    pub(super) fn join_forwarded(self: &Arc<Self>, key: Key, size: (u32, u32), joining: Joining) -> Membership {
        self.join_with(key, size, 0, true, joining)
    }

    /// `any`: share the closest group whatever the limit, else open one.
    fn join_with(
        self: &Arc<Self>,
        key: Key,
        size: (u32, u32),
        limit: usize,
        any: bool,
        joining: Joining,
    ) -> Membership {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let member = Member::new(joining.frames, joining.clock, false, joining.av1_visible_size);
        let mut groups = self.groups();
        // An ended pipeline takes nobody new.
        let open: Vec<&Arc<Group>> = groups.iter().filter(|g| g.lock().end.is_none()).collect();
        let existing: Vec<(Key, (u32, u32))> = open
            .iter()
            .map(|group| (group.key(), group.lock().size))
            .collect();
        let choice = match choose(&existing, &key, size, limit) {
            Choice::OverLimit if any => Choice::Create,
            choice => choice,
        };
        let group = match choice {
            Choice::Exact(index) | Choice::Closest(index) => {
                let group = open[index].clone();
                group.lock().members.insert(id, member);
                if matches!(choice, Choice::Exact(_)) {
                    tracing::info!(
                        source = %key.source,
                        target = ?key.target,
                        fps = key.fps,
                        "participant joined a shared encoder"
                    );
                } else if any {
                    tracing::info!(
                        source = %key.source,
                        sharing = ?group.key(),
                        "forwarded participant shares the stream of its screen"
                    );
                } else {
                    tracing::info!(
                        source = %key.source,
                        wanted = ?key,
                        sharing = ?group.key(),
                        "encoder limit reached; participant shares the closest encoder"
                    );
                }
                group
            }
            Choice::Create | Choice::OverLimit => {
                if choice == Choice::OverLimit {
                    tracing::warn!(
                        source = %key.source,
                        encoders = groups.len(),
                        "encoder limit reached but no encoder can be shared; opening another"
                    );
                } else {
                    tracing::info!(?key, encoders = groups.len() + 1, "new encoder pipeline");
                }
                let group = Arc::new(Group {
                    key: Mutex::new(key.clone()),
                    serial: id,
                    stopped: Arc::default(),
                    inner: Mutex::new(Inner {
                        size,
                        members: BTreeMap::from([(id, member)]),
                        output_revision: 0,
                        output: None,
                        cursor: None,
                        end: None,
                    }),
                });
                groups.push(group.clone());
                (self.spawn)(group.clone(), joining.start);
                group
            }
        };
        drop(groups);
        Membership {
            registry: self.clone(),
            group,
            id,
            wanted: key,
            output_seen: 0,
            bytes_seen: (Instant::now(), 0),
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

    /// Watch `group` for the forwarder: take its frames on `clock`.
    pub(super) fn observe(
        self: &Arc<Self>,
        group: &Arc<Group>,
        frames: mpsc::Sender<Encoded>,
        clock: Instant,
    ) -> Membership {
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        // The forwarder's viewers are members of their own.
        group.lock().members.insert(id, Member::new(frames, clock, true, true));
        Membership {
            registry: self.clone(),
            group: group.clone(),
            id,
            wanted: group.key(),
            output_seen: 0,
            bytes_seen: (Instant::now(), 0),
        }
    }

    /// Change what a member alone in its group watches, in place: the
    /// pipeline keeps its capture and encoder, so a cursor, frame-rate or
    /// quality change is not a new stream. `false`: it joins elsewhere
    /// (others share the stream, the encoder or format differs, or another
    /// group has `key`).
    pub(super) fn retune(&self, membership: &mut Membership, key: &Key) -> bool {
        let groups = self.groups();
        let group = &membership.group;
        let current = group.key();
        let same_encoder = current.source == key.source
            && current.adapter == key.adapter
            && current.backend == key.backend
            && current.format == key.format
            && current.isolated == key.isolated
            && current.low == key.low;
        let alone = {
            let inner = group.lock();
            inner.end.is_none()
                && inner
                    .members
                    .iter()
                    .all(|(id, member)| *id == membership.id || member.observer)
        };
        let elsewhere = groups
            .iter()
            .any(|other| !Arc::ptr_eq(other, group) && other.lock().end.is_none() && other.key() == *key);
        if !same_encoder || !alone || elsewhere {
            return false;
        }
        *group.key.lock().unwrap_or_else(|e| e.into_inner()) = key.clone();
        membership.wanted = key.clone();
        tracing::info!(?key, "encoder pipeline retuned");
        true
    }

    fn leave(&self, group: &Arc<Group>, id: u64) {
        let mut groups = self.groups();
        let mut inner = group.lock();
        inner.members.remove(&id);
        // Observers alone do not keep a pipeline: nobody watches directly.
        let empty = inner.members.values().all(|member| member.observer);
        if empty {
            inner.end.get_or_insert(End::Stopped);
        }
        drop(inner);
        if empty {
            group.stopped.store(true, Ordering::Release);
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
    output_seen: u64,
    /// The delivered byte count at the last [`Self::delivered_rate`].
    bytes_seen: (Instant, u64),
}

impl Membership {
    /// The key of the group this member is in.
    pub(super) fn group_key(&self) -> Key {
        self.group.key()
    }
    /// Whether this member's picture goes through the forwarder (its
    /// participant and track), for the uplink.
    pub(super) fn set_forwarded(&self, forwarded: Option<(u64, usize)>) {
        let mut inner = self.group.lock();
        let Some(member) = inner.members.get_mut(&self.id) else { return };
        let newly = forwarded.is_some() && member.forwarded != forwarded;
        member.forwarded = forwarded;
        drop(inner);
        if newly && self.group.key().low {
            // The low layer's uplink track takes this group's frames now
            // rather than on the next tick. (Other groups wait for it: the
            // members of a group that changes format move over within it,
            // and its track goes to the new group.)
            crate::features::host::forwarding::wake();
        }
    }
    /// Why the pipeline ended, if it did.
    pub(super) fn ended(&self) -> Option<End> {
        self.group.lock().end.clone()
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
    /// Ask for a keyframe (the next frame this member takes starts one).
    pub(super) fn request_keyframe(&self) {
        if let Some(member) = self.group.lock().members.get_mut(&self.id) {
            member.keyframe = true;
            member.awaiting = true;
            member.asked = Some(Instant::now());
        }
    }
    /// This member's viewer feedback, for the pipeline's reference
    /// decisions (`docs/protocol/rfi-ltr-brief.md` §4.2).
    pub(super) fn report(
        &self,
        held: Option<crate::features::host::recovery::Held>,
        request: Option<crate::features::host::recovery::Request>,
        rtt: Option<Duration>,
    ) {
        let mut inner = self.group.lock();
        if let Some(member) = inner.members.get_mut(&self.id) {
            if let Some(held) = held {
                member.held = Some((held, Instant::now()));
            }
            if request.is_some() {
                member.recovery = request;
            }
            member.rtt = rtt;
        }
    }
    /// The group's allowance over its active members.
    pub(super) fn aggregate(&self) -> Option<Vote> {
        self.group.aggregate()
    }
    /// Whether other members are in the group at all.
    pub(super) fn has_others(&self) -> bool {
        self.group.lock().members.len() > 1
    }
    /// When this member last got a frame.
    pub(super) fn last_frame(&self) -> Option<Instant> {
        self.group.lock().members.get(&self.id).and_then(|m| m.last)
    }
    /// What this member got since the last call (bit/s).
    pub(super) fn delivered_rate(&mut self) -> Option<u64> {
        let bytes = self.group.lock().members.get(&self.id).map_or(0, |m| m.bytes);
        let now = Instant::now();
        let (at, seen) = self.bytes_seen;
        let elapsed = now.saturating_duration_since(at);
        if elapsed < Duration::from_secs(1) {
            return None;
        }
        self.bytes_seen = (now, bytes);
        Some(((bytes - seen) as f64 * 8.0 / elapsed.as_secs_f64()) as u64)
    }
    /// The stream description, when it changed since last asked.
    pub(super) fn output_changed(&mut self) -> Option<Output> {
        let inner = self.group.lock();
        let (revision, output) = inner.output.as_ref()?;
        if *revision == self.output_seen {
            return None;
        }
        self.output_seen = *revision;
        Some(output.clone())
    }
    pub(super) fn cursor(&self) -> Option<Snapshot> {
        self.group.lock().cursor.clone()
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

    /// Tiers 1–4, else a custom bitrate of that many Mbit/s.
    fn target(quality: i32) -> crate::features::host::parameters::Target {
        use crate::features::host::parameters::{Target, Tier};
        match quality {
            1..=4 => Target::Tier(Tier::ALL[quality as usize - 1]),
            _ => Target::Custom { bps: quality as u32 * 1_000_000 },
        }
    }
    fn key(source: &str, quality: i32) -> Key {
        Key {
            source: source.into(),
            adapter: 1,
            backend: Backend::Nvidia,
            format: Format::AVC,
            hdr: false,
            maximum: (3840, 2160),
            fps: 60,
            cursor: true,
            cinema: false,
            target: target(quality),
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

    fn frame(keyframe: bool, timestamp_100ns: i64) -> Encoded {
        Encoded {
            data: vec![1, 2, 3],
            keyframe,
            timestamp_100ns,
            is_new: true,
            timing: None,
            meta: None,
            ltr: Default::default(),
            format: Format::AVC,
            color: SDR,
        }
    }

    /// A member whose frames arrive on the returned receiver.
    fn joining(clock: Instant) -> (Joining, mpsc::Receiver<Encoded>) {
        let (frames, received) = mpsc::channel(1);
        (
            Joining {
                frames,
                clock,
                av1_visible_size: true,
                start: super::super::pipeline::Start::test(),
            },
            received,
        )
    }

    fn join(registry: &Arc<Registry>, key: Key, size: (u32, u32)) -> (Membership, mpsc::Receiver<Encoded>) {
        let (joining, frames) = joining(Instant::now());
        (registry.join(key, size, MAX_ENCODERS, joining), frames)
    }

    #[test]
    fn same_key_shares_and_different_key_opens_another() {
        let registry = Arc::new(Registry::default());
        let (a, _) = join(&registry, key("D1", 2), (1920, 1080));
        let (b, _) = join(&registry, key("D1", 2), (1920, 1080));
        assert_eq!(a.group_serial(), b.group_serial());
        assert_eq!(registry.len(), 1);
        let (c, _) = join(&registry, key("D1", 3), (2560, 1440));
        assert_ne!(c.group_serial(), a.group_serial());
        assert_eq!(registry.len(), 2);
        // A member's request is the pipeline's next keyframe.
        let group = a.group.clone();
        assert!(!group.take_keyframe(Instant::now()));
        b.request_keyframe();
        assert!(group.take_keyframe(Instant::now()));
        assert!(!group.take_keyframe(Instant::now()));
    }

    #[test]
    fn a_member_alone_retunes_its_pipeline_in_place() {
        let registry = Arc::new(Registry::default());
        let (mut a, _) = join(&registry, key("D1", 2), (1920, 1080));
        let serial = a.group_serial();
        // Taking control: the cursor leaves the picture, the stream stays.
        let uncursored = Key { cursor: false, ..key("D1", 3) };
        assert!(registry.retune(&mut a, &uncursored));
        assert_eq!((a.group_serial(), a.group_key()), (serial, uncursored.clone()));
        assert_eq!(a.wanted, uncursored);
        // Another encoder or format is another pipeline.
        let other = Key { format: Format { chroma: 3, ..Format::AVC }, ..uncursored.clone() };
        assert!(!registry.retune(&mut a, &other));
        // Shared: the others keep their stream.
        let (b, _) = join(&registry, uncursored.clone(), (1920, 1080));
        assert_eq!(b.group_serial(), serial);
        assert!(!registry.retune(&mut a, &key("D1", 2)));
        drop(b);
        // A group that already has the key takes the member instead.
        let (_c, _) = join(&registry, key("D1", 4), (1920, 1080));
        assert!(!registry.retune(&mut a, &key("D1", 4)));
        assert!(registry.retune(&mut a, &key("D1", 1)));
    }

    #[test]
    fn limit_of_four_encoders_shares_the_closest() {
        let registry = Arc::new(Registry::default());
        let held: Vec<_> = (1..=4)
            .map(|quality| {
                let size = [(1280, 720), (1920, 1080), (2560, 1440), (3840, 2160)][quality as usize - 1];
                join(&registry, key("D1", quality), size)
            })
            .collect();
        assert_eq!(registry.len(), 4);
        let (extra, _) = join(&registry, key("D1", 6), (2560, 1440));
        assert_eq!(registry.len(), 4);
        assert_eq!(extra.group_key().target, target(3));
        // Another screen cannot share: over the limit.
        let (other, _) = join(&registry, key("D2", 2), (1920, 1080));
        assert_eq!(other.group_key().source, "D2");
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
    fn the_group_encodes_for_the_weakest_active_member() {
        let a = aggregate([vote(5_000_000, 0.1, 60), vote(2_000_000, 0.3, 30)], 300_000).unwrap();
        assert_eq!((a.peak, a.ratio, a.fps), (2_000_000, 0.3, 30));
        // A paused transport does not hold the others back...
        let a = aggregate([vote(5_000_000, 0.0, 60), vote(0, 0.0, 60)], 300_000).unwrap();
        assert_eq!(a.peak, 5_000_000);
        // ...unless everyone is paused.
        assert_eq!(aggregate([vote(0, 0.0, 60)], 300_000).unwrap().peak, 0);
        assert!(aggregate([], 300_000).is_none());
        // The forwarder uplink limits the rate, not the frame rate.
        let uplink = aggregate([vote(5_000_000, 0.0, 60), vote(3_000_000, 0.0, 0)], 300_000).unwrap();
        assert_eq!((uplink.peak, uplink.fps), (3_000_000, 60));
        assert_eq!(aggregate([vote(3_000_000, 0.0, 0)], 300_000).unwrap().fps, 0);
    }

    #[test]
    fn frames_reach_active_members_from_a_keyframe_on_their_clocks() {
        let registry = Arc::new(Registry::default());
        let clock = Instant::now();
        let (a, mut a_frames) = join(&registry, key("D1", 2), (1920, 1080));
        let (b_joining, mut b_frames) = joining(clock + Duration::from_micros(500));
        let b = registry.join(key("D1", 2), (1920, 1080), MAX_ENCODERS, b_joining);
        let group = a.group.clone();
        a.vote(Some(vote(5_000_000, 0.0, 60)));
        b.vote(Some(vote(2_000_000, 0.0, 60)));
        assert_eq!(group.aggregate().unwrap().peak, 2_000_000);
        // Waiting members want a keyframe; a delta frame reaches nobody.
        assert!(group.take_keyframe(Instant::now()));
        group.publish(&frame(false, 10_000), clock);
        assert!(a_frames.try_recv().is_err());
        group.publish(&frame(true, 10_000), clock);
        assert_eq!(b_frames.try_recv().unwrap().timestamp_100ns, 5_000);
        let at_a = a_frames.try_recv().unwrap();
        assert!(at_a.keyframe);
        // Both queues hold one frame: a single member would be waited for,
        // two are left behind only after the patience runs out.
        group.publish(&frame(false, 20_000), clock);
        let now = Instant::now();
        assert!(group.backpressure(now, Duration::from_millis(100)));
        assert!(!group.backpressure(now + Duration::from_millis(200), Duration::from_millis(100)));
        // A full queue loses the frame: that member starts over.
        a_frames.try_recv().unwrap();
        group.publish(&frame(false, 30_000), clock);
        assert!(b_frames.try_recv().is_ok());
        assert!(b_frames.try_recv().is_err(), "b missed the frame while full");
        group.publish(&frame(false, 40_000), clock);
        assert!(b_frames.try_recv().is_err(), "b waits for a keyframe");
        drop(a);
        drop(b);
        assert_eq!(registry.len(), 0);
    }

    #[test]
    fn rebase_moves_frames_between_clocks() {
        let clock = Instant::now();
        let encoded = rebase(&frame(true, 10_000), clock, clock).unwrap();
        assert_eq!(encoded.timestamp_100ns, 10_000);
        let later = clock + Duration::from_micros(500);
        assert_eq!(rebase(&frame(true, 10_000), clock, later).unwrap().timestamp_100ns, 5_000);
        assert!(rebase(&frame(true, 10_000), clock, clock + Duration::from_secs(1)).is_none());
    }

    #[test]
    fn observers_take_frames_but_do_not_keep_the_pipeline() {
        let registry = Arc::new(Registry::default());
        let (a, _) = join(&registry, key("D1", 2), (1920, 1080));
        let (b, _) = join(&registry, key("D1", 2), (1920, 1080));
        assert!(registry.forwarded().is_empty());
        b.set_forwarded(Some((7, 0)));
        let forwarded = registry.forwarded();
        assert_eq!(forwarded.len(), 1);
        assert_eq!(forwarded[0].members, [(7, 0)]);
        let (frames, mut received) = mpsc::channel(1);
        let observer = registry.observe(&forwarded[0].group, frames, Instant::now());
        assert_eq!(observer.group_serial(), a.group_serial());
        observer.vote(Some(vote(4_000_000, 0.0, 0)));
        let group = a.group.clone();
        assert!(group.take_keyframe(Instant::now()), "the observer waits for a keyframe");
        group.publish(&frame(true, 1), Instant::now());
        assert!(received.try_recv().is_ok());
        drop(a);
        assert!(observer.ended().is_none());
        // Only the observer is left: the pipeline stops.
        drop(b);
        assert!(observer.ended().is_some());
        assert!(group.stopped());
        assert_eq!(registry.len(), 0);
        assert!(registry.forwarded().is_empty());
        drop(observer);
    }
}
