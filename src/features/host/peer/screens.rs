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

pub(super) struct Inventory {
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
        let topology = Topology::query(false)?;
        Ok(Self {
            targets: topology.targets_within(max_age)?,
            screens: capture::screens()?,
            layout: Topology::query(true)
                .and_then(|topology| topology.snapshot())
                .inspect_err(|error| tracing::debug!(error = %format!("{error:#}"), "display layout not read"))
                .ok(),
        })
    }
    pub async fn collect(cancel: &CancellationToken) -> Result<Option<Self>> {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        let mut task = tokio::task::spawn_blocking(Self::read);
        tokio::select! {
            biased;
            _=cancel.cancelled()=>{let _=task.await;Ok(None)},
            result=&mut task=>result.context(t().features.screen_read_interrupted).and_then(|r|r).map(Some),
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
    pub capabilities: Mutex<Option<Vec<crate::features::host::format::Capability>>>,
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
    pub screen: Option<capture::Screen>,
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
pub(crate) struct Screens {
    inventory_revision: u64,
    pub displays: Arc<crate::features::host::displays::Session>,
    pub slots: Vec<Slot>,
    pub(super) reports: Arc<Reports>,
    connection: std::sync::Weak<RTCPeerConnection>,
    lease: Lease,
    cancel: CancellationToken,
    connected: Arc<AtomicBool>,
    base: VideoConfig,
    source_generation: u16,
    initial: Option<capture::Screen>,
    deferred: Option<Deferred>,
    headless: headless::State,
    receiver: crate::features::host::parameters::ReceiverCap,
    /// When the controller's receive statistics were last logged.
    stats_logged: Option<std::time::Instant>,
}
pub(crate) struct Deferred {
    pub options: crate::features::stream_control::publisher::ConnectOptions,
    pub remote: crate::protocol::capability::DeviceCapability,
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
        if let Some(id) = self.prepare_video().await? {
            return self.start(id).await;
        }
        self.refresh()?;
        let ids: Vec<_> = self
            .slots
            .iter()
            .filter_map(|s| s.screen.as_ref().or(s.suspended.as_ref()))
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
            self.slots.iter().any(|s| s.screen.is_some()),
            "{}",
            t().features.screen_none_not_restored
        );
        Ok(())
    }
    pub(crate) async fn prepare_video(&mut self) -> Result<Option<i32>> {
        let Some(deferred) = self.deferred.as_ref() else {
            return Ok(None);
        };
        let preparation = async {
            let screen = self.displays.prepare(deferred.options.clone()).await?;
            let media = self
                .lease
                .prepare_media(screen.clone(), deferred.encoding.gpu_compatibility)
                .await?;
            let capabilities = deferred.encoding.select(&media.codecs)?;
            let prepared = crate::features::host::desktop::Prepared::new(
                &deferred.options,
                screen,
                &capabilities,
                &deferred.remote,
                deferred.encoding,
            )?;
            Ok::<_, anyhow::Error>((prepared, capabilities))
        }
        .await;
        let (prepared, capabilities) = match preparation {
            Ok(prepared) => prepared,
            Err(error) => {
                if let Err(restore) = self.displays.rollback_preparation().await {
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
        *lock(&self.reports.capabilities) = Some(capabilities);
        self.reports
            .current
            .store(prepared.screen.id, Ordering::Release);
        self.deferred = None;
        self.refresh()?;
        self.reports.notify();
        Ok(Some(prepared.screen.id))
    }
    pub(crate) fn set_media_default(&mut self, config: VideoConfig) {
        self.base = config;
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
        negotiated: Arc<Negotiated>,
        network: Transport,
        selected: Option<capture::Screen>,
        displays: Arc<crate::features::host::displays::Session>,
        deferred: Option<Deferred>,
    ) -> Result<Self> {
        let reports = Arc::new(Reports {
            media_ready: Arc::new(Ready::default()),
            capabilities: Mutex::new(None),
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
                screen: None,
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
            inventory_revision: 0,
            displays,
            slots,
            reports,
            connection: Arc::downgrade(&connection),
            lease,
            cancel,
            connected,
            base: initial,
            source_generation: 0,
            initial: selected.clone(),
            deferred,
            headless: Default::default(),
            receiver: Default::default(),
            stats_logged: None,
        };
        state.apply_inventory(Inventory::read_initial()?)?;
        if let Some(selected) = selected {
            state.start_at(0, selected, initial).await?;
        }
        Ok(state)
    }
    pub(crate) fn refresh(&mut self) -> Result<()> {
        self.apply_inventory(Inventory::read()?)
    }
    pub(super) fn inventory_revision(&self) -> u64 {
        self.inventory_revision
    }
    fn apply_inventory(&mut self, inventory: Inventory) -> Result<()> {
        let Inventory {
            targets,
            screens,
            layout,
        } = inventory;
        if let Some(layout) = &layout {
            self.displays.observe_layout(layout)?;
        }
        let mut catalog = Vec::new();
        for mut screen in screens {
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
        self.inventory_revision = self.inventory_revision.wrapping_add(1);
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
            .map(|(index, state)| (state.screen.id, index))
            .collect();
        crate::features::stream_control::publisher::display_list(&catalog, &tracks, active, revision)
    }
    pub(crate) fn active_slot(&self, id: i32) -> Option<usize> {
        self.slots
            .iter()
            .position(|slot| slot.screen.as_ref().is_some_and(|s| s.id == id))
    }
    /// A track that shows nothing: preferably one not keeping a stopped
    /// display either.
    pub(crate) fn available_slot(&self) -> Option<usize> {
        let free = |index: &usize| self.slots[*index].screen.is_none();
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
        mut config: VideoConfig,
    ) -> Result<()> {
        ensure!(
            self.lease.requested() && !self.cancel.is_cancelled(),
            "{}",
            t().features.peer_lease_expired
        );
        let requested_format = config.format;
        self.slots[index]
            .negotiated
            .source_gpu(screen.render_adapter.unwrap_or(screen.adapter));
        let requested_hdr = config.color.is_hdr();
        self.slots[index].negotiated.apply_requested(
            &mut config,
            requested_format.chroma,
            requested_hdr,
            (screen.width, screen.height),
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
            screen: screen.clone(),
            // The slot already owns this source. Native preparation is not a
            // stopped stream: publishing track=-1/CaptureChange=99 here races
            // the controller's initial setting restoration.
            capturing: true,
            visible: false,
            quality: 0,
            fps: config.fps,
            format: config.format,
            budget_size: (screen.width, screen.height),
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
            screen.clone(),
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
        slot.screen = Some(screen.clone());
        self.reports.current.store(screen.id, Ordering::Release);
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
        slot.screen = None;
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
                if self.slots[index].screen.is_some() {
                    self.slots[index].suspended = self.slots[index].screen.clone();
                }
                self.stop_at(index).await;
            }
        } else if let Some(index) = self.active_slot(screen) {
            self.slots[index].awaiting_source = false;
            self.slots[index].suspended = self.slots[index].screen.clone();
            self.stop_at(index).await;
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
    pub(super) async fn maintain(&mut self, revision: u64, inventory: Inventory) -> Result<()> {
        // A user-requested layout operation may have completed during the read.
        // Its newer catalog wins over the late periodic result.
        if revision != self.inventory_revision {
            return Ok(());
        }
        self.apply_inventory(inventory)?;
        let selected = self.reports.current.load(Ordering::Acquire);
        let mut fatal = false;
        for index in 0..self.slots.len() {
            let disappeared = self.slots[index].screen.as_ref().is_some_and(|screen| {
                !lock(&self.reports.catalog).iter().any(|info| {
                    info.screen.id == screen.id
                        && (screen.identity.is_none() || info.screen.identity == screen.identity)
                })
            });
            if disappeared {
                self.slots[index].awaiting_source = true;
                self.slots[index].suspended = self.slots[index].screen.clone();
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
                    self.slots[index].suspended = self.slots[index].screen.clone();
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
