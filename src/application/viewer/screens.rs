//! One device connection, independent video-track decoders and local windows.
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use tokio::sync::watch;

use super::NativeViewerSession;
use super::ViewerDisplayHandle;
use super::ViewerLaunchConfig;
use crate::features::stream_control::{RemoteScreen, StreamControlHandle};
use crate::media::{ConnectionMediaProfile, VideoCodec};
use crate::transport::rtc::{NativePeer, VideoTrackSource};

#[derive(Clone)]
pub(crate) struct ScreenPlayback {
    pub(crate) device_switch: Option<super::device_switch::DeviceSwitcher>,
    peer: Weak<NativePeer>,
    control: StreamControlHandle,
    profile: ConnectionMediaProfile,
    alias: String,
    visible: watch::Sender<Vec<i32>>,
    runtime: tokio::runtime::Handle,
    decoders: Arc<std::sync::Mutex<HashMap<i32, Arc<NativeViewerSession>>>>,
    opening: Arc<tokio::sync::Mutex<()>>,
    capture_lock: Arc<tokio::sync::Mutex<HashSet<i32>>>,
    visibility_update: Arc<AtomicBool>,
}

impl ScreenPlayback {
    pub(crate) fn new(
        peer: &Arc<NativePeer>,
        profile: ConnectionMediaProfile,
        alias: &str,
        initial: i32,
    ) -> Self {
        let control = peer.stream_control_handle();
        let (visible, mut receiver) = watch::channel(vec![initial]);
        let activity_control = control.clone();
        let capture_lock = Arc::new(tokio::sync::Mutex::new(HashSet::new()));
        let activity_lock = Arc::clone(&capture_lock);
        peer.spawn_viewing_task(async move {
            let mut inactive = HashMap::<i32, Instant>::new();
            let mut tick = tokio::time::interval(Duration::from_millis(250));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    changed = receiver.changed() => if changed.is_err() { break; },
                    _ = tick.tick() => {},
                }
                let visible = receiver.borrow_and_update().clone();
                let screens = activity_control.snapshot().screens;
                inactive.retain(|id, _| screens.iter().any(|screen| screen.id == *id));
                activity_lock.lock().await.retain(|id| screens.iter().any(|screen| screen.id == *id));
                for screen in screens {
                    if screen.id < 0 { continue; }
                    if visible.contains(&screen.id) {
                        inactive.remove(&screen.id);
                    } else if screen.video_track_index >= 0 {
                        let since = inactive.entry(screen.id).or_insert_with(Instant::now);
                        if since.elapsed() >= Duration::from_secs(5) {
                            let mut stopped = activity_lock.lock().await;
                            if receiver.borrow().contains(&screen.id) || stopped.contains(&screen.id) { continue; }
                            stopped.insert(screen.id);
                            // No -1/all-screen operation: other windows keep playing.
                            if let Err(error) = activity_control.set_screen_capture(screen.id, false).await {
                                tracing::warn!(screen_id = screen.id, %error, "stop inactive screen capture failed");
                            }
                        }
                    }
                }
            }
        });
        Self {
            device_switch: None,
            peer: Arc::downgrade(peer),
            control,
            profile,
            alias: alias.to_owned(),
            visible,
            runtime: tokio::runtime::Handle::current(),
            decoders: Arc::new(std::sync::Mutex::new(HashMap::new())),
            opening: Arc::new(tokio::sync::Mutex::new(())),
            capture_lock,
            visibility_update: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn topology(&self) -> crate::features::stream_control::DisplayTopologyStatus {
        self.control.snapshot().topology
    }

    pub(crate) fn screens(&self) -> Vec<RemoteScreen> {
        self.control
            .snapshot()
            .screens
            .into_iter()
            .filter(|screen| screen.id >= 0)
            .collect()
    }

    pub(crate) fn set_visible(&self, mut screens: Vec<i32>) {
        screens.sort_unstable();
        screens.dedup();
        let changed = self.visible.send_if_modified(|current| {
            if *current == screens {
                false
            } else {
                *current = screens;
                true
            }
        });
        let wanted = self.visible.borrow().clone();
        let needs_update = super::mutex_lock(&self.decoders).values().any(|session| {
            session.is_software()
                && session.decode_paused() == wanted.contains(&session.screen_id())
        });
        if !(changed || needs_update) || self.visibility_update.swap(true, Ordering::AcqRel) {
            return;
        }
        let factory = self.clone();
        self.runtime.spawn(async move {
            let _opening = factory.opening.lock().await;
            let visible = factory.visible.borrow().clone();
            let sessions = super::mutex_lock(&factory.decoders)
                .values()
                .cloned()
                .collect::<Vec<_>>();
            // Release hidden CPU decoders before resuming the selected one.
            // Late RTP for a paused decoder is retired without decoding.
            for session in &sessions {
                if session.is_software()
                    && !visible.contains(&session.screen_id())
                    && let Err(error) = session.pause_software().await
                {
                    tracing::debug!(%error, "hidden software decoder already stopped");
                }
            }
            for session in sessions {
                if session.is_software()
                    && factory.visible.borrow().contains(&session.screen_id())
                    && session.resume_decode()
                {
                    if let Some(peer) = factory.peer.upgrade() {
                        match factory.capture_track(&peer, session.screen_id()).await {
                            Ok(track) => {
                                let _ = peer.request_keyframe(track.metadata.ssrc).await;
                            }
                            Err(error) => {
                                tracing::warn!(%error, "resume software screen capture failed")
                            }
                        }
                    }
                }
            }
            factory.visibility_update.store(false, Ordering::Release);
        });
    }

    pub(crate) fn focus(&self, screen_id: i32) {
        if let Some(peer) = self.peer.upgrade()
            && let Some(screen) = self
                .screens()
                .into_iter()
                .find(|screen| screen.id == screen_id)
            && let Some(track) = peer.video_tracks().get(screen.video_track_index)
        {
            peer.select_viewed_video_track(track.index);
        }
    }

    pub(crate) fn open(
        &self,
        screen_id: i32,
        display: ViewerDisplayHandle,
        replace: Option<i32>,
        cancelled_pending: Option<i32>,
        ready: impl FnOnce() + Send + 'static,
    ) -> (
        tokio::task::JoinHandle<()>,
        tokio::sync::oneshot::Receiver<Result<Arc<NativeViewerSession>>>,
    ) {
        let replace_software = replace.is_some_and(|id| {
            super::mutex_lock(&self.decoders)
                .values()
                .any(|session| session.screen_id() == id && session.is_software())
        });
        self.visible.send_modify(|visible| {
            visible.retain(|id| {
                Some(*id) != cancelled_pending && !(replace_software && Some(*id) == replace)
            });
            if !visible.contains(&screen_id) {
                visible.push(screen_id);
            }
        });
        let (send, receive) = tokio::sync::oneshot::channel();
        let factory = self.clone();
        let task = self.runtime.spawn(async move {
            let result = factory.open_screen(screen_id, display, replace).await;
            let _ = send.send(result);
            ready();
        });
        (task, receive)
    }

    pub(crate) fn register(&self, session: Arc<NativeViewerSession>) {
        super::mutex_lock(&self.decoders).insert(session.track_index, session);
    }

    pub(crate) fn close_decoders(&self) {
        for session in super::mutex_lock(&self.decoders).values() {
            session.close_handle().close();
        }
        super::mutex_lock(&self.decoders).clear();
    }

    async fn capture_track(
        &self,
        peer: &NativePeer,
        screen_id: i32,
    ) -> Result<Arc<VideoTrackSource>> {
        let generation = {
            let mut stopped = self.capture_lock.lock().await;
            let snapshot = self.control.snapshot();
            let screen = snapshot
                .screens
                .iter()
                .find(|screen| screen.id == screen_id)
                .context("显示器已断开")?;
            if screen.video_track_index < 0 || stopped.contains(&screen_id) {
                // Keep restart required until the new mapping arrives, including cancellation.
                // A local stop may already be sent while its ScreenSources is still in flight.
                stopped.insert(screen_id);
                self.control.set_screen_capture(screen_id, true).await?;
                Some(snapshot.screens_generation)
            } else {
                None
            }
        };
        let track = tokio::time::timeout(Duration::from_secs(12), async {
            loop {
                let snapshot = self.control.snapshot();
                let screen = snapshot
                    .screens
                    .iter()
                    .find(|screen| screen.id == screen_id)
                    .context("显示器已断开")?;
                if generation.is_none_or(|generation| snapshot.screens_generation > generation)
                    && screen.video_track_index >= 0
                    && let Some(track) = peer.video_tracks().get(screen.video_track_index)
                {
                    return Ok::<_, anyhow::Error>(track);
                }
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        })
        .await
        .map_err(|_| anyhow!("显示器未返回视频，请重试"))??;
        if generation.is_some() {
            self.capture_lock.lock().await.remove(&screen_id);
        }
        Ok(track)
    }

    async fn open_screen(
        &self,
        screen_id: i32,
        display: ViewerDisplayHandle,
        replace: Option<i32>,
    ) -> Result<Arc<NativeViewerSession>> {
        let switch_started = Instant::now();
        let _opening = self.opening.lock().await;
        let peer = self.peer.upgrade().context("观看连接已关闭")?;
        let existing_sessions = super::mutex_lock(&self.decoders)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for session in existing_sessions {
            if session.is_software()
                && session.screen_id() != screen_id
                && (Some(session.screen_id()) == replace
                    || !self.visible.borrow().contains(&session.screen_id()))
            {
                self.visible
                    .send_modify(|visible| visible.retain(|id| *id != session.screen_id()));
                session.pause_software().await?;
            }
        }
        let track = self.capture_track(&peer, screen_id).await?;
        let screen = self
            .screens()
            .into_iter()
            .find(|screen| screen.id == screen_id && screen.video_track_index == track.index)
            .context("显示器映射已变化")?;
        tracing::debug!(
            screen_id,
            elapsed_ms = switch_started.elapsed().as_secs_f64() * 1000.0,
            "screen switch mapping ready"
        );
        let existing = { super::mutex_lock(&self.decoders).get(&track.index).cloned() };
        if let Some(session) = existing {
            let rebound = session.bind_screen(&screen);
            if session.resume_decode() || rebound {
                peer.request_keyframe(track.metadata.ssrc).await?;
            }
            tracing::debug!(
                screen_id,
                elapsed_ms = switch_started.elapsed().as_secs_f64() * 1000.0,
                "screen switch decoder reused"
            );
            return Ok(session);
        }
        let mut session = NativeViewerSession::launch(ViewerLaunchConfig {
            codec: codec(&track.metadata.codec)?,
            decoder: self.profile.decoder,
            title: format!("{}{alias}", crate::VIEWER_TITLE_PREFIX, alias = self.alias),
            initial_width: screen.width,
            initial_height: screen.height,
            frame_rate: self.profile.stream_fps,
            receiver_feedback: track.feedback.clone(),
            performance: track.performance.clone(),
            stream_control: self.control.clone(),
            display,
        })
        .await?;
        session.track_index = track.index;
        session.bind_screen(&screen);
        track.add_sink(session.video_sink()).await;
        track.start();
        peer.request_keyframe(track.metadata.ssrc).await?;
        tokio::time::timeout(Duration::from_secs(12), session.startup())
            .await
            .context("等待屏幕解码器启动超时")??;
        let session = Arc::new(session);
        self.register(Arc::clone(&session));
        Ok(session)
    }
}

fn codec(value: &str) -> Result<VideoCodec> {
    match value.to_ascii_lowercase().as_str() {
        "video/h264" => Ok(VideoCodec::H264),
        "video/h265" | "video/hevc" => Ok(VideoCodec::H265),
        "video/av1" => Ok(VideoCodec::Av1),
        _ => bail!("不支持的视频编码：{value}"),
    }
}
