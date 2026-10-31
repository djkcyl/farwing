use crate::i18n::t;
mod actions;
mod headless;
// Fixed negotiated video-track pool; captures are attached/detached independently.
use super::{Published, media};
use crate::features::host::{
    Lease, VideoConfig, capture, format::Negotiated, lock, track::VideoTrack, transport::Transport,
};
use crate::platform::display::topology::{Target, Topology};
use anyhow::{Context, Result, ensure};
use std::{
    collections::BTreeMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;
use webrtc::{
    peer_connection::RTCPeerConnection, rtp_transceiver::rtp_sender::RTCRtpSender,
    track::track_local::TrackLocal,
};

pub(crate) const TRACK_COUNT: usize = 5;

#[derive(Clone)]
pub(super) struct Inventory {
    /// When the read began: an older read does not undo a newer one.
    read_at: std::time::Instant,
    targets: Vec<Target>,
    screens: Vec<capture::Screen>,
    /// `None` when the layout could not be read (a topology in change):
    /// the screens are still maintained.
    layout: Option<crate::platform::display::topology::SavedTopology>,
}
impl Inventory {
    pub fn read() -> Result<Self> {
        Self::read_within(crate::platform::display::topology::INVENTORY_REUSE)
    }
    /// For a starting session: an unchanged configuration's monitor names and
    /// modes from the last minutes do (reading every mode takes 40-80 ms on
    /// the session's path); its first periodic read refreshes them.
    fn read_initial() -> Result<Self> {
        Self::read_within(crate::platform::display::topology::INVENTORY_KEEP)
    }
    fn read_within(max_age: std::time::Duration) -> Result<Self> {
        let read_at = std::time::Instant::now();
        let topology = Topology::query(false)?;
        Ok(Self {
            read_at,
            targets: topology.targets_within(max_age)?,
            screens: capture::screens()?,
            layout: Topology::query(true)
                .and_then(|topology| topology.snapshot())
                .inspect_err(|error| tracing::debug!(error = %format!("{error:#}"), "display layout not read"))
                .ok(),
        })
    }
}

/// One display inventory for the whole process, read about once a second
/// while any participant watches: the participants maintain their screens
/// from it instead of each enumerating the drivers.
pub(super) mod shared {
    use super::Inventory;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, LazyLock};
    use std::time::Duration;
    use tokio::sync::watch;

    static LATEST: LazyLock<watch::Sender<Option<Arc<Inventory>>>> =
        LazyLock::new(|| watch::channel(None).0);
    static RUNNING: AtomicBool = AtomicBool::new(false);

    /// The latest inventory; a change every read (also when the read
    /// failed, so the participants still check their tracks).
    pub(in crate::features::host::peer) fn subscribe() -> watch::Receiver<Option<Arc<Inventory>>> {
        let mut receiver = LATEST.subscribe();
        receiver.mark_unchanged();
        if !RUNNING.swap(true, Ordering::AcqRel) {
            let started = std::thread::Builder::new()
                .name("display-inventory".into())
                .spawn(run);
            if started.is_err() {
                RUNNING.store(false, Ordering::Release);
            }
        }
        receiver
    }

    fn run() {
        loop {
            std::thread::sleep(Duration::from_secs(1));
            if LATEST.receiver_count() == 0 {
                RUNNING.store(false, Ordering::Release);
                // A participant that subscribed meanwhile found it running.
                if LATEST.receiver_count() == 0 || RUNNING.swap(true, Ordering::AcqRel) {
                    return;
                }
            }
            match Inventory::read() {
                Ok(inventory) => {
                    LATEST.send_replace(Some(Arc::new(inventory)));
                }
                Err(error) => {
                    tracing::debug!(%error, "host screen inventory read failed");
                    LATEST.send_modify(|_| {});
                }
            }
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScreenInfo {
    pub screen: capture::Screen,
    /// The mode before this session changed the layout.
    pub initial: Option<(u32, u32)>,
    pub target: Option<Target>,
    pub kind: crate::features::stream_control::wire::DisplayKind,
}
/// Whether the controller's session takes media: its HELLO was answered and
/// the screens were published. The capture thread waits on it while the
/// connection is made.
#[derive(Default)]
pub(super) struct Ready {
    ready: AtomicBool,
    changes: Mutex<u64>,
    changed: std::sync::Condvar,
}
impl Ready {
    pub fn get(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }
    /// Returns the previous value.
    pub fn set(&self, ready: bool) -> bool {
        let previous = self.ready.swap(ready, Ordering::AcqRel);
        if previous != ready {
            *lock(&self.changes) += 1;
            self.changed.notify_all();
        }
        previous
    }
    pub fn changes(&self) -> u64 {
        *lock(&self.changes)
    }
    /// Waits for a change after `seen` changes, at most `timeout`.
    pub fn wait(&self, seen: u64, timeout: std::time::Duration) {
        let changes = lock(&self.changes);
        let _ = self
            .changed
            .wait_timeout_while(changes, timeout, |changes| *changes == seen);
    }
}

pub(super) struct Reports {
    pub media_ready: Arc<Ready>,
    pub catalog: Mutex<Vec<ScreenInfo>>,
    pub current: AtomicI32,
    pub publications: Vec<Mutex<Option<watch::Receiver<Published>>>>,
    pub pointers: Vec<Arc<Mutex<Option<crate::platform::windows::cursor_shape::Snapshot>>>>,
    pub changed: watch::Sender<u64>,
    /// `VideoFeedback` of this participant, by track SSRC.
    pub recovery: Arc<crate::features::host::recovery::Inbox>,
    /// The participant's picture goes through the forwarder: its tracks
    /// take no frames themselves once it receives from there
    /// (`docs/protocol/deployment-design.md` §5.4).
    pub forwarded: Arc<crate::features::host::forwarding::Flags>,
}
impl Reports {
    pub fn notify(&self) {
        self.changed.send_modify(|v| *v = v.wrapping_add(1));
    }
    pub fn media(&self) -> Vec<(usize, Published)> {
        self.publications
            .iter()
            .enumerate()
            .filter_map(|(index, rx)| lock(rx).as_ref().map(|rx| (index, rx.borrow().clone())))
            .collect()
    }
}

pub(crate) struct Slot {
    /// What the track shows now.
    pub source: Option<capture::Source>,
    /// The camera it shows, among the host's viewers of it.
    camera: Option<std::sync::Arc<crate::features::host::cameras::Use>>,
    /// The screen a stopped screen track keeps, to resume.
    pub suspended: Option<capture::Screen>,
    pub config: Arc<Mutex<VideoConfig>>,
    pub negotiated: Arc<Negotiated>,
    pub transport: Transport,
    pub sender: Arc<RTCRtpSender>,
    track: Arc<VideoTrack>,
    clock: Arc<media::Timeline>,
    worker: Option<media::Worker>,
    observer: Option<tokio::task::JoinHandle<()>>,
    lease: Option<Lease>,
    awaiting_source: bool,
}
impl Slot {
    /// The screen the track shows, if it shows one.
    pub fn screen(&self) -> Option<&capture::Screen> {
        self.source.as_ref().and_then(capture::Source::screen)
    }
    /// The display it shows: a screen's ID or a camera's display ID.
    pub fn display_id(&self) -> Option<i32> {
        match self.source.as_ref()? {
            capture::Source::Screen(screen) => Some(screen.id),
            capture::Source::Camera(camera) => Some(crate::features::host::cameras::display_id(&camera.camera)),
        }
    }
}
pub(crate) struct Screens {
    /// When the inventory applied last was read.
    inventory_read: std::time::Instant,
    pub displays: Arc<crate::features::host::displays::Session>,
    pub slots: Vec<Slot>,
    pub(super) reports: Arc<Reports>,
    connection: std::sync::Weak<RTCPeerConnection>,
    lease: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    base: VideoConfig,
    /// The stream this participant asked for.
    stream: farwing_protocol::session::StreamSettings,
    source_generation: u16,
    initial: Option<capture::Screen>,
    deferred: Option<Deferred>,
    headless: headless::State,
    receiver: crate::features::host::parameters::ReceiverCap,
    /// When the controller's receive statistics were last logged.
    stats_logged: Option<std::time::Instant>,
    /// This participant's parameters by camera ID (its `CameraParams`).
    camera_params: std::collections::HashMap<String, crate::media::camera::Params>,
    /// A session for the cameras only: the displays stay as they are.
    camera_only: bool,
}
pub(crate) struct Deferred {
    pub options: crate::features::stream_control::publisher::ConnectOptions,
    pub encoding: crate::features::host::EncodingSettings,
}
impl Screens {
    /// The controller's decode health; caps the frame rate of every stream
    /// while its decoder falls behind.
    pub(crate) fn receiver_stats(&mut self, stats: &crate::features::stream_control::wire::ReceiverStats) {
        let cap = self
            .receiver
            .report(stats.receive_fps, stats.decode_fps, stats.decode_delay_ms);
        if self
            .stats_logged
            .is_none_or(|at| at.elapsed() >= std::time::Duration::from_secs(10))
        {
            self.stats_logged = Some(std::time::Instant::now());
            tracing::info!(
                receive_fps = stats.receive_fps,
                decode_fps = stats.decode_fps,
                render_fps = stats.render_fps,
                decode_delay_ms = stats.decode_delay_ms,
                dropped_frames = stats.dropped_frames,
                "controller receive statistics"
            );
        }
        if self.base.receiver_fps != cap {
            tracing::info!(
                cap,
                decode_fps = stats.decode_fps,
                receive_fps = stats.receive_fps,
                decode_delay_ms = stats.decode_delay_ms,
                "controller decoder frame-rate cap changed"
            );
        }
        self.base.receiver_fps = cap;
        for slot in &self.slots {
            let mut config = lock(&slot.config);
            if config.receiver_fps != cap {
                config.receiver_fps = cap;
                config.revision = config.revision.wrapping_add(1);
            }
        }
    }
    /// Whether the participant watches the picture (not audio only): only
    /// those watching share the viewers' stream and take part in forwarding.
    fn watching(&self, watching: bool) {
        if let Some(key) = self.lease.participant() {
            crate::features::host::participants::set_watching(key, watching);
            crate::features::host::forwarding::set_eligible(key, watching);
        }
    }
    pub(crate) async fn resume_capture(&mut self) -> Result<()> {
        self.watching(true);
        // A camera session shows what its viewer opens.
        if self.camera_only {
            return Ok(());
        }
        if let Some(id) = self.prepare_video().await? {
            return self.start(id).await;
        }
        self.refresh()?;
        let ids: Vec<_> = self
            .slots
            .iter()
            .filter_map(|s| s.screen().or(s.suspended.as_ref()))
            .filter(|s| {
                self.info(s.id)
                    .is_ok_and(|v| v.screen.identity == s.identity)
            })
            .map(|s| s.id)
            .collect();
        if !ids.is_empty() {
            for id in ids {
                self.start(id).await?;
            }
            return Ok(());
        }
        let current = {
            let catalog = lock(&self.reports.catalog);
            catalog
                .iter()
                .find(|s| s.screen.primary)
                .or(catalog.first())
                .map(|s| s.screen.id)
        };
        if let Some(id) = current {
            return self.start(id).await;
        }
        let index = 0;
        self.slots[index].suspended = self.initial.clone();
        self.slots[index].awaiting_source = true;
        self.maintain_headless().await?;
        ensure!(
            self.slots.iter().any(|s| s.screen().is_some()),
            "{}",
            t().features.screen_none_not_restored
        );
        Ok(())
    }
    pub(crate) async fn prepare_video(&mut self) -> Result<Option<i32>> {
        let Some(deferred) = self.deferred.as_ref() else {
            return Ok(None);
        };
        let camera_only = self.camera_only;
        let preparation = async {
            // A camera session leaves the displays as they are: the primary
            // screen only stands in for the encoder check.
            let screen = if camera_only {
                capture::screens()?
                    .into_iter()
                    .min_by_key(|s| !s.primary)
                    .context(t().features.screen_none_not_restored)?
            } else {
                self.displays.prepare(deferred.options.clone()).await?
            };
            let media = self
                .lease
                .prepare_media(screen.clone(), deferred.encoding.gpu_compatibility)
                .await?;
            let capabilities = deferred.encoding.select(&media.codecs)?;
            let mut options = deferred.options.clone();
            options.stream = self.stream;
            let prepared = crate::features::host::desktop::Prepared::new(
                &options,
                screen,
                &capabilities,
                deferred.encoding,
            )?;
            Ok::<_, anyhow::Error>(prepared)
        }
        .await;
        let prepared = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                if !camera_only
                    && let Err(restore) = self.displays.rollback_preparation().await
                {
                    tracing::error!(%restore,"failed video preparation layout recovery");
                }
                return Err(error);
            }
        };
        self.base = prepared.config;
        self.initial = Some(prepared.screen.clone());
        for slot in &mut self.slots {
            let negotiated = Arc::new(prepared.negotiated.for_track());
            slot.track.configure(negotiated.clone());
            slot.negotiated = negotiated;
            *lock(&slot.config) = prepared.config;
        }
        self.reports
            .current
            .store(prepared.screen.id, Ordering::Release);
        self.deferred = None;
        self.refresh()?;
        self.reports.notify();
        Ok(Some(prepared.screen.id))
    }
    pub(crate) fn stream(&self) -> farwing_protocol::session::StreamSettings {
        self.stream
    }
    /// This participant asks for `settings`: every track it has (shown or
    /// stopped) and the ones it starts take them.
    pub(crate) fn set_stream(&mut self, settings: farwing_protocol::session::StreamSettings) {
        self.stream = settings;
        self.base = crate::features::stream_control::publisher::config_of(
            &settings,
            self.base.auto_tier,
            self.base,
        );
        for slot in &self.slots {
            let Some(screen) = slot.screen().or(slot.suspended.as_ref()) else {
                continue;
            };
            let mut config = lock(&slot.config);
            if let Err(error) = crate::features::stream_control::publisher::configure(
                &settings,
                screen,
                &mut config,
                &slot.negotiated,
            ) {
                tracing::warn!(screen = screen.id, error = %format!("{error:#}"), "stream settings not applied to a track");
            }
        }
        self.reports.notify();
    }
    /// The stream as this participant gets it.
    pub(crate) fn stream_state(&self) -> crate::features::stream_control::wire::StreamState {
        use crate::features::host::parameters::{Target, Tier};
        use crate::features::stream_control::wire::{self, Quality};
        let revision = *self.reports.changed.borrow();
        let tracks = self
            .reports
            .media()
            .into_iter()
            .filter(|(_, state)| state.capturing)
            .map(|(index, state)| wire::TrackState {
                track: index as u32,
                screen: state.display_id(),
                codec: wire::Codec::of(Some(state.format.codec.session())) as i32,
                chroma444: state.format.chroma == 3,
                hdr: state.hdr,
                bit10: state.format.depth == 10,
                quality: match state.target {
                    Target::Tier(Tier::P720) => Quality::Low,
                    Target::Tier(Tier::P1080) => Quality::Standard,
                    Target::Tier(Tier::P1440) => Quality::High,
                    Target::Tier(Tier::P2160) => Quality::Ultra,
                    Target::Custom { .. } => Quality::Custom,
                } as i32,
                fps: state.fps,
                hardware: state
                    .encoder
                    .is_some_and(|b| b != crate::features::host::format::Backend::Software),
                encoder: state.encoder.map(|b| b.name().to_owned()).unwrap_or_default(),
                capture: state.capture,
            })
            .collect();
        wire::StreamState {
            revision,
            settings: Some(self.stream.into()),
            limits: Some(self.slots[0].negotiated.limits()),
            tracks,
        }
    }
    /// The tier automatic quality settled at for this participant, if it
    /// runs in automatic quality.
    pub(super) fn settled_tier(reports: &Reports) -> Option<crate::features::host::parameters::Tier> {
        reports
            .media()
            .into_iter()
            .find(|(_, state)| state.capturing && state.automatic)
            .map(|(_, state)| state.target.tier())
    }
    pub(crate) fn authorization(&self) -> Lease {
        self.lease.clone()
    }
    pub(super) async fn new(
        connection: Arc<RTCPeerConnection>,
        lease: Lease,
        cancel: CancellationToken,
        connected: Arc<AtomicBool>,
        initial: VideoConfig,
        stream: farwing_protocol::session::StreamSettings,
        negotiated: Arc<Negotiated>,
        network: Transport,
        selected: Option<capture::Screen>,
        displays: Arc<crate::features::host::displays::Session>,
        deferred: Option<Deferred>,
        camera_only: bool,
    ) -> Result<Self> {
        let reports = Arc::new(Reports {
            media_ready: Arc::new(Ready::default()),
            catalog: Mutex::default(),
            current: AtomicI32::new(selected.as_ref().map_or(-1, |s| s.id)),
            publications: (0..TRACK_COUNT).map(|_| Mutex::new(None)).collect(),
            pointers: (0..TRACK_COUNT).map(|_| Arc::default()).collect(),
            changed: watch::channel(0).0,
            recovery: Arc::default(),
            forwarded: Arc::default(),
        });
        let mut slots = Vec::new();
        for index in 0..TRACK_COUNT {
            let negotiated = Arc::new(negotiated.for_track());
            let transport = if index == 0 {
                network.clone()
            } else {
                network.fork(index)
            };
            let track = Arc::new(VideoTrack::new(
                index,
                initial.format.codec,
                negotiated.clone(),
            ));
            let sender = connection
                .add_track(track.clone() as Arc<dyn TrackLocal + Send + Sync>)
                .await?;
            let parameters = sender.get_parameters().await;
            for encoding in parameters.encodings {
                transport.register_ssrc(encoding.ssrc);
            }
            slots.push(Slot {
                source: None,
                camera: None,
                suspended: None,
                config: Arc::new(Mutex::new(initial)),
                negotiated,
                transport,
                sender,
                track,
                clock: media::Timeline::new(),
                worker: None,
                observer: None,
                lease: None,
                awaiting_source: false,
            });
        }
        let mut state = Self {
            inventory_read: std::time::Instant::now(),
            displays,
            slots,
            reports,
            connection: Arc::downgrade(&connection),
            lease,
            cancel,
            connected,
            base: initial,
            stream,
            source_generation: 0,
            initial: selected.clone(),
            deferred,
            headless: Default::default(),
            receiver: Default::default(),
            stats_logged: None,
            camera_params: Default::default(),
            camera_only,
        };
        state.apply_inventory(&Inventory::read_initial()?)?;
        if let Some(selected) = selected {
            state.start_at(0, selected, initial).await?;
        }
        Ok(state)
    }
    pub(crate) fn refresh(&mut self) -> Result<()> {
        self.apply_inventory(&Inventory::read()?)
    }
    fn apply_inventory(&mut self, inventory: &Inventory) -> Result<()> {
        let Inventory {
            read_at,
            targets,
            screens,
            layout,
        } = inventory;
        if let Some(layout) = layout {
            self.displays.observe_layout(layout)?;
        }
        let mut catalog = Vec::new();
        for mut screen in screens.iter().cloned() {
            let mut target = targets
                .iter()
                .find(|target| {
                    screen
                        .identity
                        .as_ref()
                        .map_or(target.source == screen.device_name, |identity| {
                            *identity == target.identity
                        })
                })
                .cloned();
            let kind = self.displays.annotate(&mut screen, &mut target)?;
            catalog.push(ScreenInfo {
                initial: self.displays.initial_size(&screen),
                screen,
                target,
                kind,
            });
        }
        self.inventory_read = self.inventory_read.max(*read_at);
        let mut old = lock(&self.reports.catalog);
        if *old != catalog {
            *old = catalog;
            drop(old);
            self.reports.notify();
        }
        Ok(())
    }
    pub(crate) fn info(&self, id: i32) -> Result<ScreenInfo> {
        lock(&self.reports.catalog)
            .iter()
            .find(|s| s.screen.id == id)
            .cloned()
            .ok_or_else(|| {
                crate::features::stream_control::publisher::refuse(
                    farwing_protocol::peer::error_code::NOT_FOUND,
                    crate::features::stream_control::wire::display_reason::NOT_FOUND,
                )
            })
    }
    /// This participant's view of the displays.
    pub(crate) fn display_list(&self) -> crate::features::stream_control::wire::Displays {
        let revision = *self.reports.changed.borrow();
        let catalog = lock(&self.reports.catalog).clone();
        let requested = self.reports.current.load(Ordering::Acquire);
        let active = if catalog.iter().any(|s| s.screen.id == requested) {
            requested
        } else {
            catalog.first().map_or(-1, |s| s.screen.id)
        };
        let tracks: BTreeMap<_, _> = self
            .reports
            .media()
            .into_iter()
            .filter(|(_, state)| state.capturing)
            .map(|(index, state)| (state.display_id(), index))
            .collect();
        let catalog: &[ScreenInfo] = if self.camera_only { &[] } else { &catalog };
        let mut list =
            crate::features::stream_control::publisher::display_list(catalog, &tracks, active, revision);
        list.displays.extend(self.camera_displays(&tracks));
        list
    }
    /// The cameras this participant may see, as display list entries.
    fn camera_displays(&self, tracks: &BTreeMap<i32, usize>) -> Vec<crate::features::stream_control::wire::Display> {
        use crate::features::host::cameras;
        if !self.lease.camera_access() {
            return Vec::new();
        }
        cameras::cameras()
            .into_iter()
            .map(|camera| {
                let id = cameras::display_id(&camera.id);
                let running = self.slots.iter().find_map(|s| {
                    s.source.as_ref().and_then(capture::Source::camera).filter(|c| c.camera == camera.id)
                });
                let params = self.params(&camera.id);
                let mode = running
                    .map(|spec| spec.mode)
                    .or(params.mode)
                    .or_else(|| crate::media::camera::default_mode(&camera.modes));
                crate::features::stream_control::wire::Display {
                    id,
                    kind: crate::features::stream_control::wire::DisplayKind::Camera as i32,
                    name: camera.name.clone(),
                    width: mode.map_or(0, |m| m.width),
                    height: mode.map_or(0, |m| m.height),
                    refresh_hz: mode.map_or(0, |m| m.fps()),
                    track: tracks.get(&id).map_or(-1, |i| *i as i32),
                    mirror: params.orientation.mirror,
                    quarter_turns: u32::from(params.orientation.quarter_turns),
                    ..Default::default()
                }
            })
            .collect()
    }
    /// The cameras in detail, for this participant (`Cameras`).
    pub(crate) fn camera_list(&self) -> crate::features::stream_control::camera::Cameras {
        use crate::features::host::cameras;
        use crate::features::stream_control::camera::{CameraInfo, CameraParams, Cameras};
        if !self.lease.camera_access() {
            return Cameras::default();
        }
        let nickname = self.lease.profile().nickname;
        let stopped = self.lease.participant().is_some_and(cameras::stopped);
        let cameras = cameras::cameras()
            .into_iter()
            .map(|camera| {
                let mut owner = self.lease.camera_defaults(&camera.id).unwrap_or_default();
                if owner.mode.is_none_or(|m| !camera.modes.contains(&m)) {
                    owner.mode = crate::media::camera::default_mode(&camera.modes);
                }
                let running = self.slots.iter().find_map(|s| {
                    s.source.as_ref().and_then(capture::Source::camera).filter(|c| c.camera == camera.id)
                });
                let params = self.params(&camera.id);
                CameraInfo {
                    id: cameras::display_id(&camera.id),
                    name: camera.name.clone(),
                    unavailable: if stopped {
                        t().camera.stopped_by_owner.to_owned()
                    } else {
                        camera.unavailable.clone().unwrap_or_default()
                    },
                    modes: camera.modes.iter().copied().map(Into::into).collect(),
                    owner: Some(owner.into()),
                    custom: self.camera_params.get(&camera.id).copied().map(Into::into),
                    running: running.map(|spec| {
                        CameraParams::from(crate::media::camera::Params {
                            mode: Some(spec.mode),
                            color: spec.color,
                            bitrate_kbps: Some(spec.bitrate_kbps()),
                            orientation: params.orientation,
                        })
                    }),
                    chosen_by: running
                        .and_then(|_| cameras::chosen_by(&camera.id))
                        .filter(|name| *name != nickname)
                        .unwrap_or_default(),
                    controls: camera.controls.iter().copied().map(Into::into).collect(),
                }
            })
            .collect();
        Cameras { cameras }
    }
    /// A participant's camera request: its parameters, back to the owner's,
    /// or a picture adjustment.
    pub(crate) async fn camera_request(&mut self, op: crate::features::stream_control::camera::CameraOp) -> Result<()> {
        use crate::features::host::cameras;
        use crate::features::stream_control::camera::{CameraControlKind, CameraOp};
        use crate::features::stream_control::{publisher::refuse, wire::display_reason};
        use farwing_protocol::peer::error_code;
        if !self.lease.camera_access() {
            return Err(refuse(error_code::FORBIDDEN, display_reason::CAMERA_NOT_ALLOWED));
        }
        match op {
            CameraOp::Params(set) => {
                let params = set
                    .params
                    .ok_or_else(|| refuse(error_code::BAD_REQUEST, display_reason::INVALID))?;
                self.set_camera_params(set.camera, Some(params.params())).await
            }
            CameraOp::Reset(camera) => self.set_camera_params(camera, None).await,
            CameraOp::Adjust(adjust) => {
                let camera = cameras::cameras()
                    .into_iter()
                    .find(|c| cameras::display_id(&c.id) == adjust.camera)
                    .ok_or_else(|| refuse(error_code::NOT_FOUND, display_reason::NOT_FOUND))?;
                let control = CameraControlKind::try_from(adjust.control)
                    .map_err(|_| refuse(error_code::BAD_REQUEST, display_reason::INVALID))?
                    .into();
                tokio::task::spawn_blocking(move || {
                    crate::platform::camera::set_control(&camera.id, control, adjust.value, adjust.auto)?;
                    cameras::controls_changed(&camera.id);
                    anyhow::Ok(())
                })
                .await??;
                self.reports.notify();
                Ok(())
            }
        }
    }
    /// The parameters for `camera`: this participant's, else the owner's.
    fn params(&self, camera: &str) -> crate::media::camera::Params {
        self.camera_params
            .get(camera)
            .copied()
            .or_else(|| self.lease.camera_defaults(camera))
            .unwrap_or_default()
    }
    /// Show camera display `id` on `track` (or a free one).
    async fn start_camera(&mut self, id: i32, track: Option<usize>) -> Result<()> {
        use crate::features::host::cameras;
        use crate::features::stream_control::{publisher::refuse, wire::display_reason};
        use farwing_protocol::peer::error_code;
        if !self.lease.camera_access()
            || self.lease.participant().is_some_and(cameras::stopped)
        {
            return Err(refuse(error_code::FORBIDDEN, display_reason::CAMERA_NOT_ALLOWED));
        }
        let find = |list: Vec<crate::media::camera::Camera>| {
            list.into_iter().find(|c| cameras::display_id(&c.id) == id)
        };
        let camera = match find(cameras::cameras()) {
            Some(camera) => camera,
            None => find(tokio::task::spawn_blocking(cameras::read).await?)
                .ok_or_else(|| refuse(error_code::NOT_FOUND, display_reason::NOT_FOUND))?,
        };
        if let Some(reason) = &camera.unavailable {
            anyhow::bail!("{reason}");
        }
        if let Some(index) = self.active_slot(id) {
            let mut config = lock(&self.slots[index].config);
            config.capturing = true;
            config.sending = true;
            return Ok(());
        }
        let index = track
            .or_else(|| self.available_slot())
            .ok_or_else(|| refuse(error_code::BUSY, display_reason::NO_FREE_TRACK))?;
        let params = self.params(&camera.id);
        let mode = params
            .mode
            .filter(|m| camera.modes.contains(m))
            .or_else(|| crate::media::camera::default_mode(&camera.modes))
            .context(t().camera.no_mode)?;
        let adapter = self
            .initial
            .as_ref()
            .map(|s| s.render_adapter.unwrap_or(s.adapter))
            .map_or_else(crate::platform::camera::default_adapter, Ok)?;
        let mut wanted = crate::media::camera::Spec::new(&camera, mode, params.color, adapter);
        wanted.bitrate_kbps = params.bitrate_kbps;
        // Stopped first: its old use of the camera ends before joining.
        self.stop_at(index).await;
        let (spec, chosen_by, using) = cameras::join(
            wanted,
            self.lease.participant().unwrap_or_default(),
            &self.lease.profile().nickname,
        );
        tracing::info!(camera = %camera.name, width = spec.mode.width, height = spec.mode.height,
            fps = spec.mode.fps(), %chosen_by, track = index, "participant camera starts");
        let config = self.base;
        self.watching(true);
        self.show_at(index, capture::Source::Camera(spec), config).await?;
        self.slots[index].camera = Some(using);
        self.lease.record_camera();
        Ok(())
    }
    /// This participant's parameters for `camera` (`None`: the owner's);
    /// a camera it alone watches restarts with them.
    pub(crate) async fn set_camera_params(&mut self, id: i32, params: Option<crate::media::camera::Params>) -> Result<()> {
        use crate::features::host::cameras;
        let Some(camera) = cameras::cameras().into_iter().find(|c| cameras::display_id(&c.id) == id) else {
            return Err(crate::features::stream_control::publisher::refuse(
                farwing_protocol::peer::error_code::NOT_FOUND,
                crate::features::stream_control::wire::display_reason::NOT_FOUND,
            ));
        };
        let before = self.params(&camera.id);
        match params {
            Some(params) => self.camera_params.insert(camera.id.clone(), params),
            None => self.camera_params.remove(&camera.id),
        };
        let after = self.params(&camera.id);
        // Mirror and rotation are the viewer's: only the capture's own
        // parameters restart it.
        let restart = (before.mode, before.color, before.bitrate_kbps) != (after.mode, after.color, after.bitrate_kbps);
        if restart
            && let Some(index) = self.active_slot(id)
            && cameras::viewers(&camera.id) == 1
        {
            self.stop_at(index).await;
            self.start_camera(id, Some(index)).await?;
        }
        self.reports.notify();
        Ok(())
    }
    pub(crate) fn active_slot(&self, id: i32) -> Option<usize> {
        self.slots.iter().position(|slot| slot.display_id() == Some(id))
    }
    /// A track that shows nothing: preferably one not keeping a stopped
    /// display either.
    pub(crate) fn available_slot(&self) -> Option<usize> {
        let free = |index: &usize| self.slots[*index].source.is_none();
        (0..self.slots.len())
            .filter(free)
            .find(|&index| self.slots[index].suspended.is_none())
            .or_else(|| (0..self.slots.len()).find(free))
    }
    pub(crate) async fn start(&mut self, id: i32) -> Result<()> {
        self.start_on(id, None).await
    }
    /// Capture display `id` on `track`, in place of what it shows; `None`:
    /// its own track, or a free one.
    pub(crate) async fn start_on(&mut self, id: i32, track: Option<usize>) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "{}",
            t().features.peer_lease_expired
        );
        if crate::features::host::cameras::is_camera(id) {
            self.prepare_video().await?;
            return self.start_camera(id, track).await;
        }
        let id = self.prepare_video().await?.unwrap_or(id);
        self.refresh()?;
        let screen = self.info(id)?.screen;
        self.headless.choose(id);
        if let Some(index) = self.active_slot(id) {
            if self.slots[index]
                .worker
                .as_ref()
                .is_some_and(|w| !w.ended())
            {
                let mut config = lock(&self.slots[index].config);
                config.capturing = true;
                config.sending = true;
                drop(config);
                self.reports.current.store(id, Ordering::Release);
                self.reports.notify();
                return Ok(());
            }
            let config = *lock(&self.slots[index].config);
            return self.start_at(index, screen, config).await;
        }
        let index = track
            .or_else(|| {
                self.slots
                    .iter()
                    .position(|s| s.suspended.as_ref().is_some_and(|s| s.id == id))
            })
            .or_else(|| self.available_slot())
            .ok_or_else(|| {
                crate::features::stream_control::publisher::refuse(
                    farwing_protocol::peer::error_code::BUSY,
                    crate::features::stream_control::wire::display_reason::NO_FREE_TRACK,
                )
            })?;
        let config = if self.slots[index]
            .suspended
            .as_ref()
            .is_some_and(|s| s.id == id)
        {
            *lock(&self.slots[index].config)
        } else {
            self.base
        };
        self.start_at(index, screen, config).await
    }
    async fn start_at(
        &mut self,
        index: usize,
        screen: capture::Screen,
        config: VideoConfig,
    ) -> Result<()> {
        self.show_at(index, capture::Source::Screen(screen), config).await
    }
    /// Show `source` on track `index`, in place of what it shows.
    async fn show_at(
        &mut self,
        index: usize,
        source: capture::Source,
        mut config: VideoConfig,
    ) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "{}",
            t().features.peer_lease_expired
        );
        self.slots[index]
            .negotiated
            .source_gpu(source.render_adapter());
        self.slots[index].negotiated.apply_requested(
            &mut config,
            if self.stream.chroma444 { 3 } else { 1 },
            self.stream.hdr,
            source.size(),
        )?;
        self.stop_at(index).await;
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "{}",
            t().features.peer_lease_expired
        );
        let slot = &mut self.slots[index];
        slot.suspended = None;
        slot.awaiting_source = false;
        config.capturing = true;
        config.sending = true;
        *lock(&slot.config) = config;
        self.source_generation = self.source_generation.wrapping_add(1);
        let (tx, mut rx) = watch::channel(Published {
            automatic: config.quality == crate::features::host::parameters::Quality::Auto,
            hdr: config.color.is_hdr(),
            source: source.clone(),
            // The slot already owns this source. Native preparation is not a
            // stopped stream: publishing track=-1/CaptureChange=99 here races
            // the controller's initial setting restoration.
            capturing: true,
            visible: false,
            target: crate::features::host::parameters::Target::Tier(config.auto_tier),
            fps: config.fps,
            format: config.format,
            budget_size: source.size(),
            encoder: None,
            capture: String::new(),
        });
        let lease = self.lease.begin_stream(index)?;
        let cancel = self.cancel.child_token();
        let worker = media::Worker::spawn(
            index,
            self.source_generation,
            slot.clock.clone(),
            self.connection
                .upgrade()
                .context(t().features.peer_media_connection_closed)?,
            slot.track.clone(),
            slot.sender.clone(),
            source.clone(),
            lease.clone(),
            cancel.clone(),
            self.connected.clone(),
            self.reports.media_ready.clone(),
            slot.config.clone(),
            slot.negotiated.clone(),
            slot.transport.clone(),
            tx,
            self.reports.pointers[index].clone(),
            self.reports.recovery.clone(),
            self.reports.forwarded.clone(),
        )?;
        *lock(&self.reports.publications[index]) = Some(rx.clone());
        let reports = self.reports.clone();
        slot.observer = Some(tokio::spawn(async move {
            loop {
                tokio::select! { _=cancel.cancelled()=>break, result=rx.changed()=>{ if result.is_err() { break; } } }
                reports.notify();
            }
            reports.notify();
        }));
        slot.lease = Some(lease);
        slot.worker = Some(worker);
        if let Some(screen) = source.screen() {
            self.reports.current.store(screen.id, Ordering::Release);
        }
        slot.source = Some(source);
        self.reports.notify();
        Ok(())
    }
    async fn stop_at(&mut self, index: usize) {
        let slot = &mut self.slots[index];
        if let Some(lease) = slot.lease.take() {
            lease.clear_stream();
        }
        if let Some(worker) = slot.worker.take() {
            worker.close().await;
        }
        if let Some(observer) = slot.observer.take() {
            let _ = observer.await;
        }
        slot.transport.pause();
        slot.source = None;
        slot.camera = None;
        *lock(&self.reports.publications[index]) = None;
        *lock(&self.reports.pointers[index]) = None;
        self.reports.notify();
    }
    /// The controller stops capturing `screen` (-1: everything; it then
    /// only listens).
    pub(crate) async fn stop(&mut self, screen: i32) -> Result<()> {
        ensure!(
            self.lease.requested(),
            "{}",
            t().features.peer_lease_expired
        );
        if screen == -1 {
            self.watching(false);
        }
        self.halt(screen).await
    }
    /// Stop capturing `screen` (-1: every screen); the slots keep it.
    async fn halt(&mut self, screen: i32) -> Result<()> {
        self.headless.stop(screen);
        self.stop_media(screen).await
    }
    async fn stop_media(&mut self, screen: i32) -> Result<()> {
        if screen == -1 {
            for slot in &mut self.slots {
                slot.awaiting_source = false;
            }
            for index in 0..self.slots.len() {
                if self.slots[index].screen().is_some() {
                    self.slots[index].suspended = self.slots[index].screen().cloned();
                }
                self.stop_at(index).await;
            }
        } else if let Some(index) = self.active_slot(screen) {
            self.slots[index].awaiting_source = false;
            self.slots[index].suspended = self.slots[index].screen().cloned();
            self.stop_at(index).await;
        } else if crate::features::host::cameras::is_camera(screen) {
            // A camera not shown: nothing to stop.
        } else {
            let mut known = self.headless.knows_screen(screen);
            for slot in &mut self.slots {
                if slot.suspended.as_ref().is_some_and(|s| s.id == screen) {
                    known = true;
                    slot.awaiting_source = false;
                }
            }
            if !known {
                self.info(screen)?;
            }
        }
        Ok(())
    }
    pub(super) async fn maintain(&mut self, inventory: Option<&Inventory>) -> Result<()> {
        // A user-requested layout operation may have read the displays
        // after this inventory: its newer catalog wins.
        if let Some(inventory) = inventory
            && inventory.read_at >= self.inventory_read
        {
            self.apply_inventory(inventory)?;
        }
        let selected = self.reports.current.load(Ordering::Acquire);
        // Cameras the owner stopped, or no longer allows, end.
        let camera_ended = !self.lease.camera_access()
            || self
                .lease
                .participant()
                .is_some_and(crate::features::host::cameras::stopped);
        if camera_ended {
            for index in 0..self.slots.len() {
                if self.slots[index].camera.is_some() {
                    self.stop_at(index).await;
                }
            }
        }
        let mut fatal = false;
        for index in 0..self.slots.len() {
            let disappeared = self.slots[index].screen().is_some_and(|screen| {
                !lock(&self.reports.catalog).iter().any(|info| {
                    info.screen.id == screen.id
                        && (screen.identity.is_none() || info.screen.identity == screen.identity)
                })
            });
            if disappeared {
                self.slots[index].awaiting_source = true;
                self.slots[index].suspended = self.slots[index].screen().cloned();
                self.stop_at(index).await;
            }
            if self.slots[index]
                .worker
                .as_ref()
                .is_some_and(|worker| worker.ended())
            {
                if self.slots[index]
                    .worker
                    .as_ref()
                    .is_some_and(|w| w.source_missing())
                {
                    self.slots[index].awaiting_source = true;
                    self.slots[index].suspended = self.slots[index].screen().cloned();
                } else {
                    fatal |= !self.headless.owns_slot(index);
                }
                self.stop_at(index).await;
            }
            self.reattach(index).await?;
        }
        if self.active_slot(selected).is_some() {
            self.reports.current.store(selected, Ordering::Release);
        }
        if let Err(error) = self.maintain_headless().await {
            tracing::warn!(error=%format!("{error:#}"), "headless display recovery failed");
            self.lease
                .fail((t().features.screen_recovery_failed)(&format!("{error:#}")));
        }
        if fatal
            && self.headless.recovery.is_none()
            && self
                .slots
                .iter()
                .all(|s| s.worker.is_none() && !s.awaiting_source)
        {
            self.cancel.cancel();
        }
        Ok(())
    }
    /// A track waiting for its display resumes once that display is back.
    async fn reattach(&mut self, index: usize) -> Result<()> {
        if !self.slots[index].awaiting_source || self.headless.owns_slot(index) {
            return Ok(());
        }
        if let Some(old) = self.slots[index].suspended.clone()
            && let Ok(info) = self.info(old.id)
            // A returning target must have the same monitor identity; never
            // attach an unrelated monitor which inherited DISPLAYn.
            && info.screen.identity == old.identity
            && old.identity.is_some()
        {
            let config = *lock(&self.slots[index].config);
            self.start_at(index, info.screen, config).await?;
        }
        Ok(())
    }
    pub(super) async fn close(&mut self) {
        self.headless.close().await;
        for index in 0..self.slots.len() {
            self.stop_at(index).await;
        }
        if let Err(error) = self.displays.close().await {
            self.lease
                .recovery_failed((t().features.screen_restore_incomplete)(&format!(
                    "{error:#}"
                )));
            tracing::error!(%error,"display cleanup failed");
        }
    }
}
impl Drop for Screens {
    fn drop(&mut self) {
        for slot in &mut self.slots {
            if let Some(lease) = slot.lease.take() {
                lease.clear_stream();
            }
            slot.worker.take();
            if let Some(observer) = slot.observer.take() {
                observer.abort();
            }
            slot.transport.pause();
        }
    }
}
