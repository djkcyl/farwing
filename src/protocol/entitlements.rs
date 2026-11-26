//! UU 4.48.1 ordered entitlement periods, shared by the two connection roles.
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Period {
    pub start_time: Option<i64>,
    pub end_time: Option<i64>,
    pub grant_type: u32,
    pub limit: Value,
}

impl Period {
    fn valid(&self) -> bool {
        self.grant_type <= 2
            && self.start_time.is_none_or(|t| t >= 0)
            && self.end_time.is_none_or(|t| t >= 0)
            && self
                .start_time
                .zip(self.end_time)
                .is_none_or(|(a, b)| a < b)
            && self.limit.is_object()
    }
    pub fn active(&self, now: i64) -> bool {
        self.start_time.is_none_or(|t| t <= now) && self.end_time.is_none_or(|t| now < t)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct Feature {
    pub periods: Vec<Period>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub(crate) struct Schedule {
    pub features: BTreeMap<String, Feature>,
}

impl Schedule {
    pub fn builtin_free() -> Self {
        use serde_json::json;
        let mut features = BTreeMap::new();
        for (name, limit) in [
            ("screen_quality", json!({"quality":["general"]})),
            ("screen_refresh_rate", json!({"fps":[30]})),
            ("screen_custom_bitrate", json!({"max_bitrate":8000000})),
            ("file_transport", json!({"max_total_speed":1000000})),
            (
                "clipboard_file_transport",
                json!({"max_total_speed":1000000}),
            ),
            (
                "screensaver",
                json!({"allow_custom":false,"preset_ids":["0"]}),
            ),
        ] {
            features.insert(
                name.into(),
                Feature {
                    periods: vec![Period {
                        start_time: None,
                        end_time: None,
                        grant_type: 0,
                        limit,
                    }],
                },
            );
        }
        for name in [
            "device_gamepad",
            "multi_window",
            "screen_wall",
            "relay_connection",
            "international_connect",
            "multi_channel",
            "screen_true_color",
            "screen_hdr",
            "screen_virtual",
            "second_screen",
            "screen_super",
            "privacy_enhanced",
            "viewing_mode",
        ] {
            features.insert(
                name.into(),
                Feature {
                    periods: vec![Period {
                        start_time: None,
                        end_time: None,
                        grant_type: 0,
                        limit: json!({"enable":false}),
                    }],
                },
            );
        }
        Self { features }
    }
    pub fn from_features(features: BTreeMap<String, Value>) -> Result<Self> {
        ensure!(
            !features.is_empty() && features.len() <= 512,
            "权益功能数量无效"
        );
        let mut result = BTreeMap::new();
        for (name, value) in features {
            if let Ok(feature) = serde_json::from_value::<Feature>(value)
                && !feature.periods.is_empty()
                && feature.periods.len() <= 128
                && feature.periods.iter().all(Period::valid)
            {
                result.insert(name, feature);
            }
        }
        ensure!(!result.is_empty(), "权益表缺少有效功能");
        Ok(Self { features: result })
    }

    pub fn parse(text: &str) -> Result<Self> {
        ensure!(text.len() <= 1024 * 1024, "权益同步数据过大");
        let mut value: Value = serde_json::from_str(text).context("权益同步 JSON 无效")?;
        let features = value
            .get_mut("features")
            .context("权益同步缺少 features")?
            .take();
        Self::from_features(serde_json::from_value(features)?)
    }

    pub fn current(&self, name: &str, now: i64) -> Option<&Period> {
        self.features
            .get(name)?
            .periods
            .iter()
            .find(|period| period.active(now))
    }

    pub fn next_transition(&self, now: i64) -> Option<i64> {
        self.features
            .values()
            .flat_map(|f| &f.periods)
            .flat_map(|p| [p.start_time, p.end_time])
            .flatten()
            .filter(|t| *t > now)
            .min()
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Snapshot {
    pub schedule: Schedule,
    server_time: i64,
    received: Instant,
}

impl Snapshot {
    pub fn new(mut schedule: Schedule, server_time: i64) -> Result<Self> {
        ensure!(server_time >= 0, "权益服务器时间无效");
        if server_time == 0 {
            // Official built-in snapshots have no server clock. Their periods
            // describe static limits, so the host drops both time bounds.
            for feature in schedule.features.values_mut() {
                for period in &mut feature.periods {
                    period.start_time = None;
                    period.end_time = None;
                }
            }
        }
        Ok(Self {
            schedule,
            server_time,
            received: Instant::now(),
        })
    }
    pub fn server_time(&self) -> i64 {
        if self.server_time == 0 {
            return 0;
        }
        self.server_time
            .saturating_add(self.received.elapsed().as_secs().min(i64::MAX as u64) as i64)
    }
    pub fn wire(&self) -> Result<(String, i64)> {
        Ok((serde_json::to_string(&self.schedule)?, self.server_time()))
    }
    pub fn current(&self, name: &str) -> Option<&Period> {
        self.schedule.current(name, self.server_time())
    }
    pub fn until_transition(&self) -> Duration {
        let now = self.server_time();
        self.schedule
            .next_transition(now)
            .map_or(Duration::from_secs(86400), |t| {
                Duration::from_secs(t.saturating_sub(now).clamp(1, 86400) as u64)
            })
    }
    pub fn quality_limit(&self) -> i32 {
        let Some(p) = self.current("screen_quality") else {
            return 2;
        };
        if p.limit.get("quality") == Some(&Value::Null) {
            return 4;
        }
        let Some(items) = p.limit.get("quality").and_then(Value::as_array) else {
            return 2;
        };
        items
            .iter()
            .filter_map(|q| match q.as_str()? {
                "general" => Some(2),
                "hd" => Some(3),
                "bluray" => Some(4),
                _ => None,
            })
            .max()
            .unwrap_or(2)
    }
    pub fn allows(&self, feature: &str, value: Option<Value>) -> bool {
        let Some(period) = self.current(feature) else {
            return true;
        };
        let field = match feature {
            "screen_quality" => "quality",
            "screen_refresh_rate" => "fps",
            "screen_custom_bitrate" => "max_bitrate",
            _ => "enable",
        };
        match (&period.limit[field], value) {
            (Value::Bool(allowed), _) => *allowed,
            (Value::Array(values), Some(wanted)) => values.is_empty() || values.contains(&wanted),
            (Value::Number(max), Some(Value::Number(wanted))) => max
                .as_u64()
                .zip(wanted.as_u64())
                .is_none_or(|(m, w)| w <= m),
            _ => true,
        }
    }
}
