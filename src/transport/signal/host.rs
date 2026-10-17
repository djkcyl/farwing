//! Host role of the authenticated online room; no second login or parallel room.
use super::*;
use crate::features::host::desktop;
use crate::features::host::peer::Peer;
use prost::Message as _;
use std::sync::Arc;

pub(crate) enum Event {
    Change,
    Candidate(RTCIceCandidateInit),
    Poll,
    Prepared(Result<PreparedPeer>),
    Power(crate::features::host::power::Event),
}
pub(crate) struct PreparedPeer {
    preferred_codec: Option<i32>,
    peer: Peer,
    capabilities: Vec<crate::features::host::format::Capability>,
}
struct PendingPeer {
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<Result<PreparedPeer>>,
}
impl Drop for PendingPeer {
    fn drop(&mut self) {
        self.cancel.cancel();
    }
}
pub(crate) struct Session {
    preferred_codec: Option<i32>,
    power: crate::features::host::power::Inbox,
    client: crate::session::host_client::HostClient,
    desired: tokio::sync::watch::Receiver<Option<crate::features::host::AccessRequest>>,
    active_displays: Option<Vec<crate::protocol::capability::DisplayCapability>>,
    authorization: Option<crate::features::host::Lease>,
    peer: Option<Peer>,
    preparing: Option<PendingPeer>,
    early_soac: std::collections::VecDeque<Value>,
    client_id: String,
    ice_id: String,
    app_control_id: String,
    initialized: bool,
    capabilities: Vec<crate::features::host::format::Capability>,
    active_encoder: Option<Vec<(usize, i32, (u32, u32), String)>>,
}
impl Session {
    pub(crate) fn new(client: crate::session::host_client::HostClient) -> Self {
        Self {
            preferred_codec: None,
            power: client.host.power.bind(),
            desired: client.host.subscribe(),
            client,
            active_displays: None,
            authorization: None,
            peer: None,
            preparing: None,
            early_soac: Default::default(),
            client_id: String::new(),
            ice_id: String::new(),
            app_control_id: String::new(),
            initialized: false,
            capabilities: Vec::new(),
            active_encoder: None,
        }
    }
    pub(crate) async fn next(&mut self) -> Event {
        if !self.initialized {
            self.initialized = true;
            return Event::Change;
        }
        tokio::select! {
            event=self.power.next()=>Event::Power(event),
            _=self.desired.changed()=>Event::Change,
            result=async { match &mut self.preparing { Some(p)=>(&mut p.task).await, None=>pending().await } }=>Event::Prepared(result.context("被控准备任务中断").and_then(|r|r)),
            candidate=async {match &mut self.peer {Some(peer)=>peer.candidates.recv().await,None=>pending().await}}=>{
                candidate.map_or(Event::Poll,Event::Candidate)
            },
            _=tokio::time::sleep(Duration::from_millis(250))=>Event::Poll,
        }
    }
    pub(crate) async fn apply(&mut self, event: Event, signal: &mut SignalSession) -> Result<()> {
        match event {
            Event::Power(crate::features::host::power::Event::Prepare(request)) => {
                self.power.prepare(request, self.client.clone());
            }
            Event::Power(crate::features::host::power::Event::Ready(result)) => {
                let operation = async {
                    let request = result?;
                    anyhow::ensure!(request.valid() && self.client.is_active(), "电源请求已取消");
                    self.client.host.begin_power()?;
                    self.end_control(signal).await;
                    // Keep the room's access lease: Windows may cancel shutdown for
                    // unsaved applications, and the owner must still be reconnectable.
                    let client = self.client.clone();
                    let action = request.action();
                    let (completed, result) = tokio::sync::oneshot::channel();
                    // Thread-scoped privileges cannot leak into a reused runtime worker.
                    std::thread::Builder::new()
                        .name("host-power".into())
                        .spawn(move || {
                            let _ = completed.send(request.execute(&client));
                        })
                        .context("无法启动电源执行线程")?;
                    result.await.context("电源执行任务中断")??;
                    tracing::info!(?action, "Windows accepted remote power request");
                    Ok::<(), anyhow::Error>(())
                }
                .await;
                if let Err(error) = &operation {
                    tracing::warn!(%error,"host power request was not completed");
                }
                self.client
                    .host
                    .power_result(operation.map_err(|e| e.to_string()));
            }
            Event::Prepared(result) => {
                if let Some(mut pending) = self.preparing.take() {
                    pending.cancel = tokio_util::sync::CancellationToken::new();
                }
                match result {
                    Ok(prepared)
                        if self.client.is_active()
                            && self.client.host.requested()
                            && !prepared.peer.access_revoked() =>
                    {
                        self.capabilities = prepared.capabilities;
                        self.preferred_codec = prepared.preferred_codec;
                        self.peer = Some(prepared.peer);
                        self.active_displays = None;
                        self.active_encoder = None;
                        self.send_capability(signal).await?;
                        while let Some(data) = self.early_soac.pop_front() {
                            self.soac(&data, signal).await?;
                        }
                    }
                    Ok(prepared) => prepared.peer.close().await,
                    Err(error) => {
                        tracing::warn!(error=%format!("{error:#}"),"host connection preparation failed");
                        Self::notify_termination(signal).await;
                        if let Some(lease) = &self.authorization {
                            lease.fail(format!("无法建立被控连接：{error:#}"));
                        }
                        self.early_soac.clear();
                        // A local display/codec failure belongs to this incoming
                        // connection; it must not tear down the account's room.
                        return Ok(());
                    }
                }
            }
            Event::Change => {
                let started = std::time::Instant::now();
                let request = self.desired.borrow_and_update().clone();
                self.active_displays = None;
                self.authorization = None;
                self.end_control(signal).await;
                if let Some(request) = request {
                    let lease = self.client.host.lease(&request);
                    if !lease.requested() {
                        return Ok(());
                    }
                    lease.update(false, false, "正在启用被控…");
                    match self.client.set_controllable(true).await {
                        Ok(()) if lease.requested() && self.client.is_active() => {
                            lease.update(true, false, "等待连接");
                            self.authorization = Some(lease);
                            tracing::info!(
                                elapsed_ms = started.elapsed().as_millis(),
                                "host access enabled"
                            );
                        }
                        Ok(()) => {
                            let _ = self.client.set_controllable(false).await;
                        }
                        Err(error) => {
                            let _ = self.client.set_controllable(false).await;
                            lease.update(false, false, "被控服务暂不可用");
                            lease.fail(format!("无法启用被控：{error:#}"));
                        }
                    }
                } else {
                    self.client.host.update(false, false, "已禁止被控");
                    if let Err(error) = self.client.set_controllable(false).await {
                        if !self.client.host.requested() {
                            self.client.host.update(
                                false,
                                false,
                                format!("已禁止被控，在线状态更新失败：{error:#}"),
                            );
                        }
                    }
                    tracing::info!(
                        elapsed_ms = started.elapsed().as_millis(),
                        "host access disabled"
                    );
                }
            }
            Event::Candidate(candidate) => {
                let Some(peer) = self.peer.as_ref() else {
                    return Ok(());
                };
                let Some(candidate) = peer.local_candidate(candidate).await else {
                    return Ok(());
                };
                let payload = serde_json::json!({"client_id":self.client_id,"data":{
                    "app_control_id":self.app_control_id,"ice_id":self.ice_id,"ice_network_type":0,"type":"candidate","candidate":candidate}});
                signal
                    .send_text(encode_event("soac", &[payload], None)?)
                    .await?;
            }
            Event::Poll => {
                if let Some(capabilities) = self
                    .peer
                    .as_ref()
                    .and_then(Peer::take_prepared_capabilities)
                {
                    self.capabilities = capabilities;
                    self.send_capability(signal).await?;
                }
                if let Some(attempt) = self.peer.as_ref().and_then(Peer::network_change) {
                    let payload = serde_json::json!({"client_id":self.client_id,"data":{"type":"switch_network_notify","switch_network_notify":{
                        "transport_type":1,"ice_id":self.ice_id,"attempt_switch_type":attempt}}});
                    signal
                        .send_text(encode_event("forward_setting", &[payload], None)?)
                        .await?;
                }
                if self.peer.is_some() && self.client.host.requested() {
                    let status = self.client.host.status();
                    let metadata: Vec<_> = status
                        .streams
                        .iter()
                        .filter(|(_, s)| s.video.is_some())
                        .filter_map(|(&index, s)| {
                            s.encoder.map(|(implementation, maximum)| {
                                (
                                    index,
                                    implementation,
                                    maximum,
                                    s.capture.clone().unwrap_or_else(|| "DXGI".into()),
                                )
                            })
                        })
                        .collect();
                    if metadata.is_empty() {
                        self.active_encoder = None;
                    } else if self.active_encoder.as_ref() != Some(&metadata) {
                        use crate::features::host::format::Backend;
                        let infos: Vec<_> = metadata.iter().map(|(index,implementation,_,capture)| {
                            let encoder = match implementation { 0=>Backend::Nvidia,1=>Backend::Amd,2=>Backend::Intel,_=>Backend::Software }.name();
                            serde_json::json!({"video_track_index":index,"capture_impl":capture,"encoder_impl":encoder})
                        }).collect();
                        let first = &infos[0];
                        let payload = serde_json::json!({"client_id":self.client_id,"data":{"type":"sender_para_info","sender_para_info":{
                            "ice_id":self.ice_id,"video_track_index":first["video_track_index"],"capture_impl":first["capture_impl"],"encoder_impl":first["encoder_impl"],"sender_media_infos":infos}}});
                        signal
                            .send_text(encode_event("forward_setting", &[payload], None)?)
                            .await?;
                        tracing::info!(tracks = metadata.len(), "host sender media info published");
                        self.active_encoder = Some(metadata);
                    }
                    if self
                        .peer
                        .as_ref()
                        .is_some_and(|peer| self.active_displays.as_ref() != Some(&peer.displays()))
                    {
                        self.send_capability(signal).await?;
                    }
                }

                if !self.client.host.requested()
                    || self.peer.as_ref().is_some_and(Peer::access_revoked)
                {
                    self.end_control(signal).await;
                    self.active_displays = None;
                    self.active_encoder = None;
                } else if self.peer.as_ref().is_some_and(Peer::ended) {
                    tracing::info!(error = ?self.client.host.status().error, "host cancelled media session retiring");
                    // Source/encoding failure can invalidate verified values;
                    // a normal remote close or network loss does not.
                    if self.client.host.status().error.is_some() {
                        self.client.host.invalidate_media().await;
                        self.capabilities.clear();
                    }
                    // A media failure is not a user kick. Preserve the remote
                    // controller's normal reconnect/rejoin classification.
                    if let Some(peer) = self.peer.take() {
                        peer.close().await;
                    }
                }
            }
        }
        Ok(())
    }
    pub(crate) async fn event(
        &mut self,
        event: &str,
        args: &[Value],
        binary: &[Vec<u8>],
        signal: &mut SignalSession,
    ) -> Result<()> {
        if !self.client.is_active() {
            return Ok(());
        }
        if !self.client.host.requested() {
            if event == "be-controlled" {
                // A controller can still submit a request while the cloud
                // device list is stale (the official CLI can do this too).
                // Clear only our disabled host room; do not construct a peer.
                Self::notify_termination(signal).await;
                if let Some(peer) = self.peer.take() {
                    peer.close().await;
                }
                tracing::info!("host incoming connection refused by local access policy");
            }
            return Ok(());
        }
        match event {
            "be-controlled" => {
                let authorization = self.authorization.clone().context("本机被控尚未就绪")?;
                anyhow::ensure!(authorization.requested(), "本次被控许可已失效");
                tracing::info!("host received authenticated be-controlled event");
                let encoding_settings = self.client.host.encoding_settings();
                let value = args.first().context("缺少被控会话参数")?;
                let required = |key: &str| {
                    value
                        .get(key)
                        .and_then(Value::as_str)
                        .filter(|v| !v.is_empty() && v.len() <= 256)
                        .map(str::to_owned)
                        .context("被控会话标识缺失或无效")
                };
                let client_id = required("client_id")?;
                let ice_id = required("ice_id")?;
                let app_control_id = required("app_control_id")?;
                let index = value
                    .get("app_data")
                    .filter(|v| v.get("_placeholder").and_then(Value::as_bool) == Some(true))
                    .and_then(|v| v.get("num"))
                    .and_then(Value::as_u64)
                    .and_then(|v| usize::try_from(v).ok())
                    .context("缺少ConnectOptions二进制引用")?;
                let options = crate::features::stream_control::publisher::ConnectOptions::decode(
                    binary
                        .get(index)
                        .context("缺少ConnectOptions二进制内容")?
                        .as_slice(),
                )?;
                let file_only = options.kind == 5;
                let ports_only = options.kind == 9;
                // UU's `CT_TERMINAL`: a shell on this machine, with no video or
                // audio of its own. The Android client opens it as kind 8.
                let terminal_only = options.kind == 8;
                let data_only = file_only || ports_only || terminal_only;
                let default_streamer = serde_json::json!({});
                let streamer = value
                    .get("streamer_data")
                    .or_else(|| data_only.then_some(&default_streamer))
                    .context("缺少主控能力参数")?;
                let streamer = if let Some(text) = streamer.as_str() {
                    serde_json::from_str::<Value>(text)?
                } else {
                    streamer.clone()
                };
                let remote: crate::protocol::capability::DeviceCapability = serde_json::from_value(
                    streamer
                        .get("device_capability")
                        .cloned()
                        .or_else(|| data_only.then(|| serde_json::json!({})))
                        .context("缺少主控解码能力")?,
                )?;
                // OpenUUYC extension; the ordinary UU connection type stays desktop.
                let audio_control = streamer
                    .get("openuuyc_audio_control")
                    .and_then(Value::as_u64)
                    == Some(1);
                let diagnostics_extension =
                    streamer.get("openuuyc_diagnostics").and_then(Value::as_u64) == Some(1);
                let annotation_extension =
                    streamer.get("openuuyc_annotation").and_then(Value::as_u64) == Some(1);
                let display_geometry = streamer
                    .get("openuuyc_display_geometry")
                    .and_then(Value::as_u64)
                    == Some(1);
                let drag_extension =
                    streamer.get("openuuyc_drag_drop").and_then(Value::as_u64) == Some(1);
                let audio_only = streamer
                    .get("openuuyc_audio_only")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                tracing::info!(client_type=options.client_type,kind=options.kind,connect_type=options.connect_type,
                    decoders=?options.decoders,formats=?remote.video_codec_capability,
                    "host incoming media capabilities");
                anyhow::ensure!(
                    matches!(options.kind, 1 | 5 | 8 | 9)
                        && matches!(options.connect_type, 1 | 2)
                        && (!data_only || options.connect_type == 1),
                    "不支持的被控连接类型"
                );
                anyhow::ensure!(
                    data_only
                        || (!remote.video_codec_capability.is_empty()
                            && !options.decoders.is_empty()),
                    "主控解码能力为空"
                );
                let servers: Vec<ControlIceServer> = serde_json::from_value(
                    value
                        .get("iceServers")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!([])),
                )?;
                let servers = servers
                    .into_iter()
                    .map(|s| webrtc::ice_transport::ice_server::RTCIceServer {
                        urls: vec![s.urls],
                        username: s.username,
                        credential: s.credential,
                        ..Default::default()
                    })
                    .collect();
                anyhow::ensure!(
                    !self.client.is_guest() || options.connect_type == 2,
                    "游客身份仅接受远程协助"
                );
                let authorization = if options.connect_type == 2 {
                    authorization.for_assistance()?
                } else {
                    authorization
                };
                anyhow::ensure!(
                    !file_only || authorization.file_access(),
                    "本机未允许文件传输"
                );
                anyhow::ensure!(
                    !ports_only || authorization.port_policy().0,
                    "本机未允许端口转发"
                );
                // A host that cannot serve a terminal refuses with its own
                // reason before the permission is considered.
                if let Some(reason) = terminal_only
                    .then(crate::features::host::terminal::unavailable)
                    .flatten()
                {
                    anyhow::bail!(reason);
                }
                anyhow::ensure!(
                    !terminal_only || authorization.terminal_allowed(),
                    "本机未允许终端"
                );
                self.cancel_preparation().await;
                if let Some(peer) = self.peer.take() {
                    peer.close().await;
                }
                let owner = authorization.peer_lease()?;
                owner.controller(&options.device_id, &client_id, terminal_only);
                let cancel = tokio_util::sync::CancellationToken::new();
                let lease = owner.with_cancellation(cancel.clone());
                let update_resume = (!audio_only && !data_only)
                    .then(|| {
                        crate::features::host::update_resume::Context::new(
                            lease.display_scope(),
                            &options,
                        )
                    })
                    .flatten();
                let displays = crate::features::host::displays::Session::new(
                    lease.clone(),
                    &options.device_id,
                )?;
                let task_cancel = cancel.clone();
                let relay = value
                    .get("force_relay")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let network = crate::features::host::network::Policy::from_signal(value);
                let configuration_client = self.client.clone();
                let task = tokio::spawn(async move {
                    let preparation_started = std::time::Instant::now();
                    let result = async {
                        let mut display_ms = 0;
                        let mut media_ms = 0;
                        let (screen, config, negotiated, capabilities, deferred) = if audio_only
                            || data_only
                        {
                            (
                                None,
                                crate::features::host::VideoConfig {
                                    capturing: false,
                                    sending: false,
                                    ..Default::default()
                                },
                                Arc::new(crate::features::host::format::Negotiated::deferred()),
                                Vec::new(),
                                Some(crate::features::host::peer::screens::Deferred {
                                    options: {
                                        let mut media = options.clone();
                                        media.kind = 1;
                                        media
                                    },
                                    remote,
                                    encoding: encoding_settings,
                                }),
                            )
                        } else {
                            let screen = displays.prepare(options.clone()).await?;
                            display_ms = preparation_started.elapsed().as_millis();
                            let media_started = std::time::Instant::now();
                            let media = lease.prepare_media(screen.clone(), encoding_settings.gpu_compatibility).await?;
                            media_ms = media_started.elapsed().as_millis();
                            anyhow::ensure!(lease.requested(), "本次被控许可已失效");
                            let capabilities = encoding_settings.select(&media.codecs)?;
                            let restored = if let Some(context) = update_resume.clone() {
                                let original = options.clone();
                                match tokio::task::spawn_blocking(move || context.take(&original)).await? {
                                    Ok(value) => value,
                                    Err(error) => {
                                        tracing::warn!(%error, "update media handoff unavailable");
                                        None
                                    }
                                }
                            } else { None };
                            let prepared = if let Some(restored) = restored {
                                match desktop::Prepared::new(&restored, screen.clone(), &capabilities, &remote, encoding_settings) {
                                    Ok(prepared) => prepared,
                                    Err(error) => {
                                        tracing::warn!(%error, "update media intent no longer supported by current capabilities");
                                        desktop::Prepared::new(&options, screen, &capabilities, &remote, encoding_settings)?
                                    }
                                }
                            } else {
                                desktop::Prepared::new(&options, screen, &capabilities, &remote, encoding_settings)?
                            };
                            (
                                Some(prepared.screen),
                                prepared.config,
                                prepared.negotiated,
                                capabilities,
                                None,
                            )
                        };
                        let peer_started = std::time::Instant::now();
                        let mut peer = Peer::new(
                            screen,
                            owner,
                            task_cancel,
                            displays.clone(),
                            servers,
                            relay,
                            config,
                            options.control_screen_reports(),
                            negotiated,
                            network,
                            crate::features::host::input::wire::Policy::from_client_type(
                                options.client_type,
                            ),
                            deferred,
                            audio_control,
                            annotation_extension,
                            display_geometry,
                            diagnostics_extension,
                            drag_extension,
                            configuration_client.controlling_features(),
                            crate::protocol::peer_platform::PeerPlatform::from_connect_client_type(
                                options.client_type,
                            ),
                            options.clipboard_level(),
                            options.file_capabilities(),
                            data_only,
                            terminal_only,
                            update_resume,
                        )
                        .await?;
                        tracing::info!(display_ms, media_ms,
                            peer_ms=peer_started.elapsed().as_millis(),
                            total_ms=preparation_started.elapsed().as_millis(),
                            "host connection preparation completed");
                        peer.load_input_configuration(configuration_client);
                        Ok(PreparedPeer { peer, capabilities, preferred_codec: encoding_settings.preferred_codec.preferred() })
                    }
                    .await;
                    if result.is_err() {
                        if let Err(error) = displays.close().await {
                            tracing::error!(%error, "initial display preparation rollback failed");
                        }
                    }
                    result
                });
                self.client_id = client_id;
                self.ice_id = ice_id;
                self.app_control_id = app_control_id;
                self.preparing = Some(PendingPeer { cancel, task });
                authorization.update(true, false, "正在准备被控画面…");
            }
            "soac" => {
                let value = args.first().context("缺少SOAC")?;
                if value
                    .get("client_id")
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != self.client_id)
                {
                    return Ok(());
                }
                let data = value.get("data").context("缺少SOAC data")?;
                if data.get("ice_id").and_then(Value::as_str) != Some(self.ice_id.as_str())
                    || data
                        .get("app_control_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != self.app_control_id)
                {
                    return Ok(());
                }
                if self.preparing.is_some() {
                    let bytes: usize = self.early_soac.iter().map(|v| v.to_string().len()).sum();
                    anyhow::ensure!(
                        self.early_soac.len() < 256
                            && bytes + data.to_string().len() <= 8 * 1024 * 1024,
                        "提前协商数据过多"
                    );
                    self.early_soac.push_back(data.clone());
                } else {
                    self.soac(data, signal).await?;
                }
            }
            "left" | "released" => {
                // AFE890 installs the same AFB1F0 consumer for both events;
                // AA81A0 requires a nonempty ice_id before releasing a peer.
                if args
                    .first()
                    .and_then(|v| v.get("ice_id"))
                    .and_then(Value::as_str)
                    != Some(self.ice_id.as_str())
                    || self.ice_id.is_empty()
                {
                    return Ok(());
                }
                if args.first().is_some_and(|v| {
                    v.get("ice_id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| id != self.ice_id)
                        || v.get("app_control_id")
                            .and_then(Value::as_str)
                            .is_some_and(|id| id != self.app_control_id)
                }) {
                    return Ok(());
                }
                if args
                    .first()
                    .and_then(|v| v.get("client_id"))
                    .and_then(Value::as_str)
                    .is_some_and(|id| id != self.client_id)
                {
                    return Ok(());
                }
                self.cancel_preparation().await;
                if let Some(peer) = self.peer.take() {
                    tracing::info!(event, "host matching signaling release received");
                    peer.close().await;
                }
            }
            _ => {}
        }
        Ok(())
    }
    async fn soac(&self, data: &Value, signal: &mut SignalSession) -> Result<()> {
        let peer = self.peer.as_ref().context("没有活动被控会话")?;
        match data.get("type").and_then(Value::as_str) {
            Some("offer" | "restart_ice") => {
                let sdp = data
                    .get("sdp")
                    .and_then(Value::as_str)
                    .context("缺少offer SDP")?;
                let restart = data.get("type").and_then(Value::as_str) == Some("restart_ice");
                let network_type = data
                    .get("ice_network_type")
                    .and_then(Value::as_u64)
                    .and_then(|v| u8::try_from(v).ok())
                    .unwrap_or(0);
                let answer = peer.answer(sdp.to_owned(), restart, network_type).await?;
                let attachment = gzip_sdp(&answer)?;
                let payload = serde_json::json!({"client_id":self.client_id,"data":{"app_control_id":self.app_control_id,
                            "ice_id":self.ice_id,"type":if restart {"restart_ice"} else {"answer"},"ice_network_type":network_type,
                            "sdp":"","gzip_sdp":{"_placeholder":true,"num":0}}});
                signal.send_binary_packet(
                    encode_binary_event("soac", &[payload], None, 1)?,
                    attachment,
                )?;
            }
            Some("candidate") => {
                peer.add_candidate(serde_json::from_value(
                    data.get("candidate").cloned().context("缺少candidate")?,
                )?)
                .await?
            }
            _ => {}
        }
        Ok(())
    }
    async fn cancel_preparation(&mut self) {
        if let Some(mut pending) = self.preparing.take() {
            pending.cancel.cancel();
            if let Ok(Ok(prepared)) = (&mut pending.task).await {
                prepared.peer.close().await;
            }
        }
        self.early_soac.clear();
    }
    pub(crate) async fn signaling_restored(&mut self, signal: &mut SignalSession) -> Result<()> {
        if self.peer.as_ref().is_some_and(|peer| !peer.ended())
            && self
                .authorization
                .as_ref()
                .is_some_and(|lease| lease.requested())
        {
            // A physical socket loss drops its queued messages. Publish the
            // current idempotent snapshot again; never replay old RPC actions.
            self.active_encoder = None;
            if self.peer.is_some() {
                self.send_capability(signal).await?;
            }
            tracing::info!("host signaling restored; current media metadata scheduled");
        }
        Ok(())
    }
    async fn end_control(&mut self, signal: &mut SignalSession) {
        if self.preparing.is_some() {
            Self::notify_termination(signal).await;
            self.cancel_preparation().await;
        }
        let Some(peer) = self.peer.take() else {
            return;
        };
        Self::notify_termination(signal).await;
        peer.close().await;
    }

    async fn notify_termination(signal: &mut SignalSession) {
        // S 2F97E0 -> 313FC0 -> ControlledInterface[13], T AFCAB0 ->
        // AFC830: clear_out (no arguments) precedes local peer destruction.
        // This only clears viewers of our own controlled-device room.
        let submitted = async {
            let packet = encode_event("clear_out", &[], None)?;
            signal
                .transport
                .send_connected(vec![Message::Text(packet.into())])
                .await
        }
        .await;
        match submitted {
            Ok(()) => tracing::info!("host active control termination submitted"),
            Err(error) => {
                tracing::warn!(%error, "host termination notice unavailable; stopping locally without replay")
            }
        }
    }

    async fn send_capability(&mut self, signal: &mut SignalSession) -> Result<()> {
        let capability = crate::protocol::capability::DeviceCapability {
            preferred_codec: self.preferred_codec,
            openuuyc_sdr_10bit: true,
            ice_id: self.ice_id.clone(),
            display_info: self.peer.as_ref().context("缺少被控会话")?.displays(),
            video_codec_capability: self.capabilities.iter().map(|c| c.wire()).collect(),
        };
        let displays = capability.display_info.clone();
        let payload = serde_json::json!({"client_id":self.client_id,"data":{"type":"device_capability","device_capability":capability}});
        signal
            .send_text(encode_event("forward_setting", &[payload], None)?)
            .await?;
        self.active_displays = Some(displays);
        Ok(())
    }
    pub(crate) async fn close(mut self) {
        self.cancel_preparation().await;
        if let Some(peer) = self.peer.take() {
            tracing::info!("host signaling owner closing active media session");
            peer.close().await;
        }
        self.client.host.room_closed();
    }
}
