//! Runtime feature policy, separate from PB protocol negotiation.
//! See official-440-controller-route.md for actual consumers and cache rules.
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime},
};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const CONFIG: &str = "controlled_ability_map";
const RELOAD_AFTER: Duration = Duration::from_secs(21_600);

#[derive(Clone, Copy)]
pub(crate) enum Feature {
    EntitlementSync,
    Microphone,
    PortMapping,
    FileDrop,
    Annotation,
    ControlledUpdate,
    CustomBitrate,
    ScreenChroma,
    SmartMouse,
    ManualTransfer,
    MultiScreen,
    ManualSuperScreen,
    FpsSuperScreen,
    Shutdown,
    Reboot,
}
impl Feature {
    fn name(self) -> &'static str {
        match self {
            Self::EntitlementSync => "user_entitlement_sync",
            Self::Microphone => "device_microphone",
            Self::PortMapping => "port_mapping",
            Self::FileDrop => "remote_clipboard_v5",
            Self::Annotation => "annotation_v2",
            Self::ControlledUpdate => "controlled_update",
            Self::CustomBitrate => "custom_bitrate",
            Self::ScreenChroma => "screen_chroma",
            Self::SmartMouse => "smart_mouse",
            Self::ManualTransfer => "manual_transfer",
            Self::MultiScreen => "multi_screen",
            Self::ManualSuperScreen => "manual_super_screen",
            Self::FpsSuperScreen => "fps_super_screen",
            Self::Shutdown => "device_shutdown",
            Self::Reboot => "device_reboot",
        }
    }
}

type Version = [u32; 4];
type Map = BTreeMap<(i32, String), (Version, Version)>;

fn parse_version(text: &str) -> Option<Version> {
    let mut result = [0; 4];
    let mut count = 0;
    for piece in text.split('.') {
        if count == 4 || piece.is_empty() || !piece.bytes().all(|b| b.is_ascii_digit()) {
            return None;
        }
        result[count] = piece.parse().ok()?;
        count += 1;
    }
    (count == 3 || count == 4).then_some(result)
}

fn feature_version(text: &str) -> Version {
    // The feature policy represents an open bound as [255; 4], and an
    // invalid version as zero. These are protocol values, not semver rules.
    if text.is_empty() {
        [255; 4]
    } else {
        parse_version(text).unwrap_or_default()
    }
}

fn parse_map(value: &serde_json::Value, controlling: bool) -> Map {
    let mut result = Map::new();
    for row in value.as_array().into_iter().flatten() {
        let Some(platform) = row
            .get("platform")
            .and_then(|v| v.as_i64())
            .and_then(|v| i32::try_from(v).ok())
        else {
            continue;
        };
        let Some(abilities) = row.get("abilitys").and_then(|v| v.as_array()) else {
            continue;
        };
        if !controlling && !matches!(platform, 1 | 4) {
            continue;
        }
        let mut entries = Map::new();
        for item in abilities {
            let Some(name) = item.get("feature").and_then(|v| v.as_str()) else {
                continue;
            };
            let first = item
                .get("original_version")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let last = item
                .get("final_version")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            // Entries require a name and original_version;
            // only final_version is allowed to be the empty upper bound.
            if name.is_empty() || first.is_empty() {
                continue;
            }
            entries.insert(
                (platform, name.to_owned()),
                (feature_version(first), feature_version(last)),
            );
        }
        if !entries.is_empty() {
            result.retain(|(p, _), _| *p != platform);
            result.extend(entries);
        }
    }
    result
}

fn embedded(controlling: bool) -> Map {
    parse_map(
        &serde_json::from_str(if controlling {
            include_str!("official_controlling_features.json")
        } else {
            include_str!("official_features.json")
        })
        .expect("embedded feature data"),
        controlling,
    )
}

struct State {
    map: Map,
    online: bool,
    loaded_at: SystemTime,
    requested: bool,
    version: String,
    cached: Option<serde_json::Value>,
}
#[derive(Default)]
struct Job(Option<JoinHandle<()>>);
impl Drop for Job {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

#[derive(Clone)]
pub(crate) struct FeatureCatalog {
    controlling: bool,
    self_hosted: bool,
    state: Arc<Mutex<State>>,
    job: Arc<Mutex<Job>>,
    stop: CancellationToken,
}
impl Default for FeatureCatalog {
    fn default() -> Self {
        Self::new(false)
    }
}
impl FeatureCatalog {
    pub(crate) fn new(controlling: bool) -> Self {
        Self {
            controlling,
            self_hosted: false,
            state: Arc::new(Mutex::new(State {
                map: embedded(controlling),
                online: false,
                loaded_at: SystemTime::now(),
                requested: false,
                version: String::new(),
                cached: None,
            })),
            job: Arc::new(Mutex::new(Job::default())),
            stop: CancellationToken::new(),
        }
    }
}
impl FeatureCatalog {
    pub(crate) fn self_hosted(controlling: bool) -> Self {
        Self { self_hosted: true, ..Self::new(controlling) }
    }

    pub(crate) fn published(
        &self,
        platform: crate::protocol::peer_platform::PeerPlatform,
        feature: Feature,
    ) -> bool {
        let Some(platform) = platform.account_code() else {
            return false;
        };
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .map
            .contains_key(&(platform, feature.name().to_owned()))
    }
    pub(crate) fn policy(&self, platform: i32, version: &str) -> FeaturePolicy {
        FeaturePolicy {
            catalog: self.clone(),
            platform,
            version: version.to_owned(),
        }
    }

    pub(crate) fn refresh(
        &self,
        api: crate::account::api::NrdApi,
        account_end: CancellationToken,
        session_create: bool,
    ) {
        if self.stop.is_cancelled() || account_end.is_cancelled() {
            return;
        }
        let mut job = self.job.lock().unwrap_or_else(|e| e.into_inner());
        if job.0.as_ref().is_some_and(|j| !j.is_finished()) {
            return;
        }
        let version = {
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let elapsed = SystemTime::now()
                .duration_since(state.loaded_at)
                .unwrap_or_else(|e| e.duration());
            if state.requested && (!session_create || state.online || elapsed < RELOAD_AFTER) {
                return;
            }
            state.requested = true;
            state.version.clone()
        };
        let shared = Arc::clone(&self.state);
        let stop = self.stop.clone();
        let controlling = self.controlling;
        let config = if controlling {
            "controlling_ability_map"
        } else {
            CONFIG
        };
        job.0 = Some(tokio::spawn(async move {
            let versions = [(config.to_owned(), version)];
            let result = tokio::select! {
                biased;
                _ = stop.cancelled() => return,
                _ = account_end.cancelled() => return,
                response = api.query_configures(&versions) => response.and_then(|r| r.into_data()),
            };
            if stop.is_cancelled() || account_end.is_cancelled() {
                return;
            }
            let mut state = shared.lock().unwrap_or_else(|e| e.into_inner());
            let content = match result {
                Ok(mut response) => match response.remove(config) {
                    Some(entry) if entry.status == 0 => {
                        let value = entry.data.and_then(|value| match value {
                            serde_json::Value::String(text) => serde_json::from_str(&text).ok(),
                            value => Some(value),
                        });
                        state.version = entry.version;
                        state.cached = value.clone();
                        value
                    }
                    Some(_) => state.cached.clone(),
                    None => None,
                },
                Err(error) => {
                    tracing::warn!(%error, "official feature configuration query failed");
                    None
                }
            };
            if let Some(value) = content {
                state.map = parse_map(&value, controlling);
                state.online = true;
                state.loaded_at = SystemTime::now();
                tracing::debug!("official runtime feature configuration applied");
            } else if state.online {
                state.map = embedded(controlling);
                state.online = false;
                state.loaded_at = SystemTime::now();
            }
        }));
    }

    pub(crate) async fn close(&self) {
        self.stop.cancel();
        let job = self.job.lock().unwrap_or_else(|e| e.into_inner()).0.take();
        if let Some(job) = job {
            let _ = job.await;
        }
    }
}

#[derive(Clone)]
pub(crate) struct FeaturePolicy {
    catalog: FeatureCatalog,
    platform: i32,
    version: String,
}
impl FeaturePolicy {
    pub(crate) fn is_windows(&self) -> bool {
        self.platform == 1
    }

    pub(crate) fn supports(&self, feature: Feature) -> bool {
        if self.catalog.self_hosted && matches!(feature, Feature::ControlledUpdate) {
            return false;
        }
        let version = if self.catalog.self_hosted && self.platform == 1 {
            crate::protocol::official_version::CLIENT
        } else {
            &self.version
        };
        if matches!(feature, Feature::MultiScreen) && self.platform == 1 {
            // The Windows multi-screen entry uses this explicit minimum;
            // other supported platforms use the runtime feature table.
            return parse_version(version).is_some_and(|v| v >= [4, 0, 0, 0]);
        }
        let state = self.catalog.state.lock().unwrap_or_else(|e| e.into_inner());
        let version = feature_version(version);
        state
            .map
            .get(&(self.platform, feature.name().to_owned()))
            .is_some_and(|(first, last)| first <= &version && &version <= last)
    }
}
