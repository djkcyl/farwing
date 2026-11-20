//! Desktop media preparation within an already authorized controlled connection.
use super::{VideoConfig, capture, encoder::ProbeCache, format};
use crate::features::stream_control::publisher;
use crate::protocol::capability::DeviceCapability;
use anyhow::{Context, Result, ensure};
use std::sync::Arc;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub(crate) struct Capabilities {
    pub screen: capture::Screen,
    pub codecs: Vec<format::Capability>,
    pub adapters: Vec<capture::EncodingAdapter>,
}

#[derive(Default)]
pub(super) struct Cache(Arc<tokio::sync::Mutex<CacheState>>);

#[derive(Default)]
struct CacheState {
    encoders: ProbeCache,
    snapshot: Option<Arc<Capabilities>>,
}
impl Cache {
    pub fn snapshot(&self) -> Option<Arc<Capabilities>> {
        self.0.try_lock().ok()?.snapshot.clone()
    }
    pub async fn prepare(
        &self,
        screen: capture::Screen,
        gpu_compatibility: bool,
        active: impl Fn() -> bool + Send + 'static,
    ) -> Result<Arc<Capabilities>> {
        let mut cache = self.0.clone().lock_owned().await;
        crate::lifecycle::blocking(move |cancel| {
            let active = || active() && !cancel.is_cancelled();
            ensure!(active(), "媒体准备已取消");
            let adapters = capture::encoding_adapters()?;
            let codecs = cache.encoders.prepare(
                screen.adapter,
                screen.render_adapter.unwrap_or(screen.adapter),
                &adapters,
                gpu_compatibility,
                active,
            )?;
            ensure!(active(), "媒体准备已取消");
            let snapshot = Arc::new(Capabilities {
                screen,
                codecs,
                adapters,
            });
            cache.snapshot = Some(snapshot.clone());
            Ok(snapshot)
        })
        .await
        .context("被控能力检查任务中断")?
    }
    pub async fn replace(&self, value: Option<Capabilities>) {
        self.0.lock().await.snapshot = value.map(Arc::new);
    }
    pub async fn invalidate(&self) {
        *self.0.lock().await = CacheState::default();
    }
}

pub(crate) struct Prepared {
    pub screen: capture::Screen,
    pub negotiated: Arc<format::Negotiated>,
    pub config: VideoConfig,
}
impl Prepared {
    pub fn new(
        options: &publisher::ConnectOptions,
        screen: capture::Screen,
        capabilities: &[format::Capability],
        remote: &DeviceCapability,
        settings: super::EncodingSettings,
    ) -> Result<Self> {
        let negotiated = Arc::new(format::Negotiated::new(
            capabilities,
            remote,
            &options.decoders,
            settings,
            screen.render_adapter.unwrap_or(screen.adapter),
        )?);
        let mut config = publisher::config(options.params.as_ref());
        let chroma = options
            .params
            .as_ref()
            .map_or(1, |p| if p.chroma == 3 { 3 } else { 1 });
        let hdr = options.params.as_ref().is_some_and(|p| p.hdr);
        negotiated.apply(
            &mut config,
            None,
            chroma,
            hdr,
            (screen.width, screen.height),
        )?;
        tracing::info!(
            parameters = ?options.params,
            requested_fps = config.requested_fps,
            fps_limit = config.fps_limit,
            maximum_fps = config.maximum_fps,
            effective_fps = config.fps,
            "host initial capture settings negotiated"
        );
        Ok(Self {
            screen,
            negotiated,
            config,
        })
    }
}
