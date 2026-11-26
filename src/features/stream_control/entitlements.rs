use super::*;
use crate::account::api::Entitlements;
use crate::protocol::entitlements::Snapshot;
use std::time::Duration;
use tokio::sync::{oneshot, watch};

impl StreamControlHandle {
    pub(crate) fn bind_entitlements(&self, updates: watch::Receiver<Option<Arc<Entitlements>>>) {
        self.network.bind_entitlements(updates.clone());
        lock(&self.shared).entitlements = Some(updates);
    }
    pub(crate) fn require_entitlement(&self, feature: &str, label: &str) -> Result<()> {
        require(&lock(&self.shared), feature, None, label)
    }

    pub(crate) async fn synchronize_entitlements(&self) {
        let Some(mut updates) = lock(&self.shared).entitlements.clone() else {
            return;
        };
        let mut connection = self.entitlement_connection.subscribe();
        'updates: loop {
            let ready = {
                let state = lock(&self.shared);
                connection.borrow_and_update();
                state.pb_connected && state.text_channel_open
            };
            if !ready {
                tokio::select! {
                    changed=updates.changed()=>if changed.is_err(){return},
                    changed=connection.changed()=>if changed.is_err(){return},
                }
                continue;
            }
            let table = updates.borrow_and_update().clone();
            {
                let snapshot = match &table {
                    Some(table) => table.snapshot.clone(),
                    None => {
                        Snapshot::new(crate::protocol::entitlements::Schedule::builtin_free(), 0)
                            .expect("static rights")
                    }
                };
                for attempt in 0..4 {
                    let result = tokio::select! {
                        result=self.send_entitlements(&snapshot)=>result,
                        changed=updates.changed()=>{
                            if changed.is_err(){return}
                            continue 'updates;
                        }
                        changed=connection.changed()=>{if changed.is_err(){return}continue 'updates;}
                    };
                    match result {
                        Ok(()) => break,
                        Err(error) => {
                            tracing::warn!(%error,attempt,"entitlement synchronization failed")
                        }
                    }
                    if attempt < 3 {
                        tokio::select! {
                            _=tokio::time::sleep(Duration::from_secs([1,2,4][attempt]))=>{},
                            changed=updates.changed()=>{if changed.is_err(){return}continue 'updates},
                        }
                    }
                }
            }
            if updates.has_changed().unwrap_or(false) {
                continue;
            }
            if connection.has_changed().unwrap_or(false) {
                continue;
            }
            tokio::select! {
                changed=updates.changed()=>if changed.is_err(){return},
                changed=connection.changed()=>if changed.is_err(){return},
            }
        }
    }

    async fn send_entitlements(&self, snapshot: &Snapshot) -> Result<()> {
        let (sequence, response) = {
            let mut state = lock(&self.shared);
            anyhow::ensure!(
                state.pb_connected && state.text_channel_open,
                "权益同步通道尚未就绪"
            );
            if state.features.as_ref().is_some_and(|p| {
                !p.supports(crate::account::feature_ability::Feature::EntitlementSync)
            }) {
                return Ok(());
            }
            let sequence = state.next_sequence;
            state.next_sequence = state.next_sequence.wrapping_add(1);
            let (data, time) = snapshot.wire()?;
            let (tx, rx) = oneshot::channel();
            let request = wire::PbRpcRequest {
                request_header: Some(wire::PbRequestHeader {
                    request_id: sequence,
                }),
                payload: Some(wire::PbRpcRequestPayload::UserEntitlementSync(
                    wire::PbEntitlementSync {
                        entitlement_data: data,
                        current_server_time: time,
                    },
                )),
            };
            self.outgoing
                .send(OutgoingControlMessage {
                    annotation_generation: None,
                    sequence,
                    payload: wire::encode_envelope(
                        sequence,
                        wire::PbPayload::RpcRequest(request.encode_to_vec()),
                    ),
                    protocol: protocol(&state),
                    completion: None,
                })
                .map_err(|_| anyhow!("连接已关闭"))?;
            state.entitlement_sync = Some((sequence, tx));
            tracing::info!(
                request_id = sequence,
                features = snapshot.schedule.features.len(),
                "entitlement synchronization sent"
            );
            (sequence, rx)
        };
        let response = response.await;
        {
            let mut state = lock(&self.shared);
            if state
                .entitlement_sync
                .as_ref()
                .is_some_and(|(id, _)| *id == sequence)
            {
                state.entitlement_sync = None;
            }
        }
        let code = response.map_err(|_| anyhow!("权益同步已取消"))?;
        anyhow::ensure!(code == 0, "对端拒绝权益同步（{code}）");
        tracing::info!(
            request_id = sequence,
            "entitlement synchronization acknowledged"
        );
        Ok(())
    }
}

pub(super) fn require(
    state: &StreamControlState,
    feature: &str,
    value: Option<serde_json::Value>,
    label: &str,
) -> Result<()> {
    if let Some(table) = state
        .entitlements
        .as_ref()
        .and_then(|rx| rx.borrow().clone())
    {
        anyhow::ensure!(
            table.allows(feature, value),
            "当前账号权益不支持{label}，可在 UU 远程官方客户端查看或开通会员"
        );
    }
    Ok(())
}

pub(super) fn settings(state: &StreamControlState, next: StreamControlSettings) -> Result<()> {
    use serde_json::json;
    let old = state.settings;
    if next.frame_rate != old.frame_rate {
        require(
            state,
            "screen_refresh_rate",
            Some(json!(next.frame_rate.value(state.local_display))),
            "所选帧率",
        )?;
    }
    if next.quality != old.quality {
        let quality = match next.quality {
            StreamQuality::Clear => Some("general"),
            StreamQuality::High => Some("hd"),
            StreamQuality::Original => Some("bluray"),
            _ => None,
        };
        if let Some(quality) = quality {
            require(state, "screen_quality", Some(json!(quality)), "所选画质")?;
        }
    }
    if next.quality == StreamQuality::Custom
        && (old.quality != StreamQuality::Custom
            || next.custom_bitrate_mbps != old.custom_bitrate_mbps)
    {
        require(
            state,
            "screen_custom_bitrate",
            Some(json!(u64::from(next.custom_bitrate_mbps) * 1_000_000)),
            "所选码率",
        )?;
    }
    if next.true_color && !old.true_color {
        require(state, "screen_true_color", None, "真彩")?;
    }
    if next.hdr && !old.hdr {
        require(state, "screen_hdr", None, "HDR")?;
    }
    Ok(())
}

pub(super) fn can_save_auto_quality(state: &StreamControlState) -> bool {
    if state.entitlement_preferences.is_some() {
        return false;
    }
    let Some(rights) = &state.entitlements else {
        return true;
    };
    let Some(table) = rights.borrow().clone() else {
        return false;
    };
    let saved = match state.confirmed_preferences.auto_frame_quality {
        4 => "bluray",
        3 => "hd",
        _ => "general",
    };
    table.allows("screen_quality", Some(serde_json::json!(saved)))
}

pub(crate) fn constrain_initial(
    table: &Entitlements,
    profile: &mut ConnectionMediaProfile,
    preferences: &mut Option<StreamControlPreferences>,
) -> Result<()> {
    use serde_json::json;
    let mut settings = preferences
        .map(|p| p.settings)
        .unwrap_or(StreamControlSettings {
            hdr: false,
            true_color: false,
            frame_rate: frame_rate_choice(profile.stream_fps),
            quality: StreamQuality::Auto,
            custom_bitrate_mbps: DEFAULT_CUSTOM_BITRATE_MBPS,
        });
    if !table.allows("screen_refresh_rate", Some(json!(profile.stream_fps))) {
        let fps = [144, 90, 60, 30]
            .into_iter()
            .filter(|fps| *fps <= profile.stream_fps)
            .find(|fps| table.allows("screen_refresh_rate", Some(json!(fps))))
            .ok_or_else(|| anyhow!("当前账号未开放可用的帧率"))?;
        profile.stream_fps = fps;
        settings.frame_rate = frame_rate_choice(fps);
    }
    let quality = match settings.quality {
        StreamQuality::Original => Some("bluray"),
        StreamQuality::High => Some("hd"),
        StreamQuality::Clear => Some("general"),
        _ => None,
    };
    if quality.is_some_and(|q| !table.allows("screen_quality", Some(json!(q)))) {
        settings.quality = [
            (StreamQuality::Original, "bluray"),
            (StreamQuality::High, "hd"),
            (StreamQuality::Clear, "general"),
        ]
        .into_iter()
        .filter(|(q, _)| q.protobuf() <= settings.quality.protobuf())
        .find(|(_, name)| table.allows("screen_quality", Some(json!(name))))
        .map(|(q, _)| q)
        .ok_or_else(|| anyhow!("当前账号未开放可用的画质"))?;
    }
    if settings.quality == StreamQuality::Custom
        && !table.allows(
            "screen_custom_bitrate",
            Some(json!(u64::from(settings.custom_bitrate_mbps) * 1_000_000)),
        )
    {
        if let Some(limit) = table
            .snapshot
            .current("screen_custom_bitrate")
            .and_then(|p| p.limit["max_bitrate"].as_u64())
        {
            anyhow::ensure!(limit >= 1_000_000, "当前账号未开放可用的自定义码率");
            settings.custom_bitrate_mbps = (limit / 1_000_000).min(500) as u32;
        }
    }
    settings.true_color &= table.allows("screen_true_color", None);
    settings.hdr &= table.allows("screen_hdr", None);
    if let Some(p) = preferences {
        p.settings = settings;
    }
    Ok(())
}
