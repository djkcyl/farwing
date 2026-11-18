//! Sending-side business protocol; deliberately separate from viewer consumers.
use super::wire::PbRpcRequestPayload;
use super::*;
use crate::features::host::VideoConfig;
use crate::features::host::capture::Screen;
use anyhow::Context as _;

mod displays;
pub(crate) use displays::{receive_session, screen_states};

/// The terminal session list changed.
///
/// The official host broadcasts this empty message when sessions appear,
/// vanish or are renamed. The controller reacts by sending its own
/// `listSessionsReq`; it only accepts the first list response after a request
/// it made, so pushing the list proactively would be dropped.
pub(crate) fn terminal_session_changed() -> Vec<u8> {
    PbControlMessage {
        seq: 0,
        timestamp: 0,
        payload: Some(PbPayload::TerminalSessionChanged(Vec::new())),
    }
    .encode_to_vec()
}

pub(crate) struct DrawRequest {
    pub command: super::annotation::wire::PbDrawRequestKind,
    sequence: i64,
    timestamp: i64,
    request: i64,
}
pub(crate) fn draw_request(bytes: &[u8]) -> Result<Option<DrawRequest>> {
    if bytes.first() == Some(&b'{') {
        return Ok(None);
    }
    let message = PbControlMessage::decode(bytes)?;
    let Some(PbPayload::RpcRequest(bytes)) = message.payload else {
        return Ok(None);
    };
    let request = PbRpcRequest::decode(bytes.as_slice())?;
    let Some(PbRpcRequestPayload::Draw(draw)) = request.payload else {
        return Ok(None);
    };
    let command = draw.payload.context("批注请求缺少操作")?;
    let header = request.request_header.context("批注请求缺少标识")?;
    Ok(Some(DrawRequest {
        command,
        sequence: message.seq,
        timestamp: message.timestamp,
        request: header.request_id,
    }))
}
impl DrawRequest {
    pub fn response(&self, error_code: i32) -> Vec<u8> {
        use super::annotation::wire::*;
        let result = PbDrawResult { error_code };
        let payload = match self.command {
            PbDrawRequestKind::Stroke(_) => PbDrawResponseKind::Stroke(result),
            PbDrawRequestKind::Clear(_) => PbDrawResponseKind::Clear(result),
            PbDrawRequestKind::Toggle(_) => PbDrawResponseKind::Toggle(result),
        };
        PbControlMessage {
            seq: self.sequence,
            timestamp: self.timestamp,
            payload: Some(PbPayload::RpcResponse(PbRpcResponse {
                response_header: Some(PbResponseHeader {
                    request_id: self.request,
                }),
                payload: Some(PbRpcResponsePayload::DrawResp(PbDrawResponse {
                    payload: Some(payload),
                })),
            })),
        }
        .encode_to_vec()
    }
}

/// Display RPC semantics and authorization belong to the session, not the
/// incoming carrier. Replies use the same TEXT RPC response path on both.
pub(crate) fn display_request(bytes: &[u8]) -> Result<Option<&'static str>> {
    if bytes.first() == Some(&b'{') {
        return Ok(None);
    }
    let Some(PbPayload::RpcRequest(bytes)) = PbControlMessage::decode(bytes)?.payload else {
        return Ok(None);
    };
    Ok(match PbRpcRequest::decode(bytes.as_slice())?.payload {
        Some(PbRpcRequestPayload::CaptureSetting(_)) => Some("CaptureSetting"),
        Some(PbRpcRequestPayload::CreateVirtualDisplay(_)) => Some("CreateVirtualDisplay"),
        Some(PbRpcRequestPayload::RemoveVirtualDisplay(_)) => Some("RemoveVirtualDisplay"),
        Some(PbRpcRequestPayload::EnterSuperScreen(_)) => Some("EnterSuperScreen"),
        Some(PbRpcRequestPayload::QuitSuperScreen(_)) => Some("QuitSuperScreen"),
        Some(PbRpcRequestPayload::SendVideoTrack(_)) => Some("SendVideoTrack"),
        Some(PbRpcRequestPayload::MouseSwitch(_)) => Some("MouseSwitch"),
        _ => None,
    })
}

pub(crate) struct MicrophonePolicy {
    pub enabled: bool,
    sequence: i64,
    timestamp: i64,
    request: i64,
}
pub(crate) fn microphone_policy(bytes: &[u8]) -> Result<Option<MicrophonePolicy>> {
    let message = PbControlMessage::decode(bytes)?;
    let Some(PbPayload::RpcRequest(bytes)) = message.payload else {
        return Ok(None);
    };
    let request = PbRpcRequest::decode(bytes.as_slice())?;
    let Some(PbRpcRequestPayload::VirtualAudioDriverPolicy(policy)) = request.payload else {
        return Ok(None);
    };
    let header = request.request_header.context("麦克风请求缺少标识")?;
    Ok(Some(MicrophonePolicy {
        enabled: policy.policy & 1 != 0,
        sequence: message.seq,
        timestamp: message.timestamp,
        request: header.request_id,
    }))
}
impl MicrophonePolicy {
    pub fn response(&self, error_code: i32) -> Vec<u8> {
        PbControlMessage {
            seq: self.sequence,
            timestamp: self.timestamp,
            payload: Some(PbPayload::RpcResponse(PbRpcResponse {
                response_header: Some(PbResponseHeader {
                    request_id: self.request,
                }),
                payload: Some(PbRpcResponsePayload::VirtualAudioDriverPolicyRsp(
                    super::microphone::PolicyResponse { error_code }.encode_to_vec(),
                )),
            })),
        }
        .encode_to_vec()
    }
}
pub(crate) fn audio_device_event(action: i32) -> Vec<u8> {
    PbControlMessage {
        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
            action,
            args: String::new(),
            params: None,
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

/// Official desktop ReportError(-6), Message oneof tag 12. This announces a
/// locally confirmed installation; it is not a request to download software.
pub(crate) fn update_started() -> Vec<u8> {
    PbControlMessage {
        payload: Some(PbPayload::ReportError(super::wire::PbReportError {
            action: 0,
            error_code: -6,
            error_msg: String::new(),
            type_value: 0,
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct ConnectOptions {
    #[prost(int32, tag = "1")]
    pub kind: i32,
    #[prost(int32, tag = "2")]
    pub screen_id: i32,
    #[prost(message, optional, tag = "3")]
    pub params: Option<CaptureParams>,
    #[prost(message, repeated, tag = "4")]
    pub decoders: Vec<DecoderCapability>,
    #[prost(bool, tag = "5")]
    pub force_virtual: bool,
    #[prost(message, repeated, tag = "6")]
    pub virtual_modes: Vec<VirtualMode>,
    #[prost(message, optional, tag = "7")]
    pub virtual_initial: Option<Resolution>,
    #[prost(int32, tag = "8")]
    pub client_type: i32,
    #[prost(string, tag = "9")]
    pub device_id: String,
    #[prost(int32, tag = "10")]
    pub connect_type: i32,
    #[prost(message, optional, tag = "11")]
    features: Option<PbFeatureFlag>,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct Resolution {
    #[prost(int32, tag = "1")]
    pub width: i32,
    #[prost(int32, tag = "2")]
    pub height: i32,
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct VirtualMode {
    #[prost(int32, tag = "1")]
    pub width: i32,
    #[prost(int32, tag = "2")]
    pub height: i32,
    #[prost(int32, tag = "3")]
    pub vsync: i32,
}
impl ConnectOptions {
    pub fn file_capabilities(&self) -> crate::features::file_transfer::host::Capabilities {
        self.features.as_ref().map_or(Default::default(), |f| {
            crate::features::file_transfer::host::Capabilities::from_levels(
                f.file_transfer_ftp,
                f.file_transfer_ftp2,
            )
        })
    }
    pub fn clipboard_level(&self) -> i32 {
        self.features.as_ref().map_or(0, |f| f.clipboard)
    }
    pub fn control_screen_reports(&self) -> bool {
        self.features
            .as_ref()
            .is_some_and(|f| f.capture_setting >= 1)
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct DecoderCapability {
    #[prost(int32, tag = "1")]
    pub fps: i32,
    #[prost(int32, tag = "2")]
    pub codec: i32,
    #[prost(int32, tag = "3")]
    pub width: i32,
    #[prost(int32, tag = "4")]
    pub height: i32,
    #[prost(int32, tag = "5")]
    pub chroma: i32,
}
impl DecoderCapability {
    pub fn chroma_sampling(&self) -> u8 {
        // S DB77E0 -> DB75F0: protobuf's unspecified value means 4:2:0.
        // Android currently leaves this field unset, including for HEVC.
        match self.chroma {
            2..=4 => self.chroma as u8,
            _ => 1,
        }
    }

    pub fn maximum_fps(&self) -> u32 {
        // S DB77E0 -> DB72A0 converts a numeric rate to the protocol's tiers;
        // this is distinct from CaptureParams' 1/2/3/4 frame-rate enum.
        match self.fps as u32 {
            117.. => 144,
            75..=116 => 90,
            45..=74 => 60,
            _ => 30,
        }
    }
}
#[derive(Clone, PartialEq, prost::Message)]
pub(crate) struct CaptureParams {
    #[prost(int32, tag = "1")]
    pub fps: i32,
    #[prost(int32, tag = "2")]
    pub quality: i32,
    #[prost(bool, tag = "3")]
    pub cursor_capture: bool,
    #[prost(int32, tag = "4")]
    pub resolution_type: i32,
    #[prost(message, optional, tag = "5")]
    pub local_resolution: Option<Resolution>,
    #[prost(message, optional, tag = "6")]
    pub chosen_resolution: Option<Resolution>,
    #[prost(int32, tag = "7")]
    pub chroma: i32,
    #[prost(int32, tag = "8")]
    pub bitrate: i32,
    #[prost(bool, tag = "9")]
    pub hdr: bool,
    #[prost(int32, tag = "10")]
    pub auto_quality: i32,
    #[prost(int32, tag = "11")]
    pub fps_count: i32,
}

fn flags() -> PbFeatureFlag {
    PbFeatureFlag {
        capture_setting: capture_setting(),
        clipboard: 3,
        system_metrics: 1,
        file_transfer_ftp: 2,
        file_transfer_ftp2: 2,
        qos_stat: 1,
        // Current Windows input protocol generation, not an installed/active
        // driver assertion. RpcRequest 11 retains its no-install support reply.
        virtual_mouse_device: 1,
        ..Default::default()
    }
}

/// The capability level this host can honour.
///
/// 5 那一档在协议里是一整包「分辨率转换 + DPI 设置 + 虚拟屏」，6 再加 HDR，主控是
/// 按这一档整体开菜单的（见 `display_topology::support`），没法只开其中一项。Linux
/// 上有把握的只有虚拟屏：root 辅助程序接一路 VKMS 连接器，niri 认下它就是一块新显
/// 示器（见 `crates/vkms`）。物理屏的分辨率走 RandR，X11 会话里能用、Wayland 下不
/// 行；DPI 和超级屏还没有可写的路径。所以报 5 意味着「虚拟屏真的能用，其余几项会带
/// 着原因失败」，而不是像以前那样把它们全藏起来——藏起来的代价是连虚拟屏都点不了。
#[cfg(windows)]
fn capture_setting() -> i32 {
    6
}

/// 见上。辅助程序装了、当前桌面接得住新输出、内核的 VKMS 有 configfs 接口，三件事
/// 都成立才报 5（见 `virtual_display::ready`）。
///
/// 只看文件在不在是不够的——deb 包总会把辅助程序装上，于是 GNOME、KDE、sway 和所有
/// Xorg 会话都会报 5，主控那边「添加虚拟屏」的按钮点亮，点下去必然失败（而且是先插
/// 一块屏再拔掉，本机用户能看到布局跳一下）；6.19 以前的内核上 niri 也一样。中途装上
/// 辅助程序要重开程序才会对外改口，这比在握手时去连一个 root 服务（那会顺手把它拉
/// 起来）划算。
#[cfg(not(windows))]
fn capture_setting() -> i32 {
    use std::sync::OnceLock;
    static READY: OnceLock<bool> = OnceLock::new();
    if *READY.get_or_init(crate::platform::virtual_display::ready) {
        5
    } else {
        4
    }
}

pub(crate) fn echo(seq: i64, timestamp: i64, request: bool) -> Vec<u8> {
    PbControlMessage {
        seq,
        timestamp,
        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
            action: if request { 0 } else { 1 },
            args: serde_json::json!({"seq":seq}).to_string(),
            params: Some(PbSimpleActionParams::FeatureFlag(flags())),
        })),
    }
    .encode_to_vec()
}

pub(crate) fn stamp_report(bytes: &[u8], sequence: i64) -> Result<Vec<u8>> {
    // S31FA70 stamps unsolicited custom/TEXT messages before serialization.
    // RPC request_id and ECHO correlation are separate from this sender counter.
    let payload = PbControlMessage::decode(bytes)?
        .payload
        .context("缺少上报内容")?;
    Ok(super::wire::encode_envelope(sequence, payload))
}

pub(crate) fn capture_change(screen: &Screen, capturing: bool) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Change {
        #[prost(int32, tag = "1")]
        kind: i32,
        #[prost(int32, tag = "2")]
        value: i32,
    }
    PbControlMessage {
        payload: Some(PbPayload::CaptureChange(
            Change {
                kind: if capturing { 0 } else { 99 },
                value: if capturing { screen.id } else { 0 },
            }
            .encode_to_vec(),
        )),
        ..Default::default()
    }
    .encode_to_vec()
}
pub(crate) fn permissions(visible: bool, audio: bool) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct Permission {
        #[prost(bool, tag = "2")]
        video: bool,
        #[prost(bool, tag = "3")]
        visible: bool,
        #[prost(bool, tag = "4")]
        audio: bool,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct State {
        #[prost(message, optional, tag = "3")]
        permission: Option<Permission>,
    }
    PbControlMessage {
        payload: Some(PbPayload::SystemStateChange(
            State {
                permission: Some(Permission {
                    video: true,
                    visible,
                    audio,
                }),
            }
            .encode_to_vec(),
        )),
        ..Default::default()
    }
    .encode_to_vec()
}

pub(crate) fn secure_desktop(locked: bool) -> Vec<u8> {
    #[derive(Clone, PartialEq, prost::Message)]
    struct SecureDesktop {
        #[prost(bool, tag = "1")]
        locked: bool,
        #[prost(int32, tag = "3")]
        x: i32,
        #[prost(int32, tag = "4")]
        y: i32,
        #[prost(bool, optional, tag = "6")]
        ui_ready: Option<bool>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    struct State {
        #[prost(message, optional, tag = "1")]
        secure: Option<SecureDesktop>,
    }
    PbControlMessage {
        payload: Some(PbPayload::SystemStateChange(
            State {
                secure: Some(SecureDesktop {
                    locked,
                    x: -1,
                    y: -1,
                    ui_ready: Some(true),
                }),
            }
            .encode_to_vec(),
        )),
        ..Default::default()
    }
    .encode_to_vec()
}

#[derive(Default)]
pub(crate) struct Received {
    pub messages: Vec<Vec<u8>>,
    pub handshake_complete: bool,
    pub handshake_request: bool,
    pub control_screen_reports: Option<bool>,
    pub refresh_state: bool,
    pub refresh_secure: bool,
    pub clipboard: Option<i32>,
    pub files: Option<crate::features::file_transfer::host::Capabilities>,
    pub media_active: Option<bool>,
}
impl From<Vec<Vec<u8>>> for Received {
    fn from(messages: Vec<Vec<u8>>) -> Self {
        Self {
            messages,
            ..Default::default()
        }
    }
}
pub(crate) fn quality_report(
    format: crate::features::host::format::Format,
    quality: i32,
    probe: u32,
    source: (u32, u32),
    fps: u32,
    encoder: crate::features::host::format::Backend,
    capture: &str,
) -> Vec<u8> {
    // T C8FF40: custom has no fast/general/hd/bluray reference budgets.
    let budget = |q| {
        if quality == 6 {
            0
        } else {
            u64::from(crate::features::host::parameters::fixed(format, q, 0, source, fps).maximum)
        }
    };
    PbControlMessage {
        payload: Some(PbPayload::ReportQosStats(PbReportQosStats {
            encoder_type: encoder.qos_type().into(),
            capture_type: capture.into(),
            probe_bps: probe.into(),
            video_quality: quality,
            fast_bitrate: budget(1),
            general_bitrate: budget(2),
            hd_bitrate: budget(3),
            bluray_bitrate: budget(4),
        })),
        ..Default::default()
    }
    .encode_to_vec()
}

pub(crate) fn config(params: Option<&CaptureParams>) -> VideoConfig {
    let Some(p) = params else {
        return VideoConfig::default();
    };
    VideoConfig {
        fps: fps(p.fps, p.fps_count),
        requested_fps: fps(p.fps, 0),
        fps_limit: if p.fps_count > 0 {
            p.fps_count as u32
        } else {
            144
        },
        bitrate: bitrate(p.quality, p.auto_quality, p.bitrate),
        quality: quality(p.quality),
        auto_quality: auto_quality(p.auto_quality),
        revision: 0,
        reported_quality: if p.quality == 5 {
            auto_quality(p.auto_quality)
        } else {
            quality(p.quality)
        },
        sending: true,
        capturing: true,
        cursor_capture: p.cursor_capture,
        ..Default::default()
    }
}
fn fps(level: i32, count: i32) -> u32 {
    let requested = match level {
        1 => 30,
        2 => 60,
        3 => 90,
        4 => 144,
        _ => 30,
    };
    if count > 0 {
        requested.min(count as u32)
    } else {
        requested
    }
}
fn quality(value: i32) -> i32 {
    // S DB7E50: unspecified and unknown quality use the general tier.
    match value {
        1..=6 => value,
        _ => 2,
    }
}
fn auto_quality(value: i32) -> i32 {
    // S DB75C0: the automatic baseline is general, HD, or Blu-ray.
    match value {
        3 | 4 => value,
        _ => 2,
    }
}
fn bitrate(quality: i32, auto: i32, custom: i32) -> u32 {
    match if quality == 5 { auto } else { quality } {
        4 => 30_000_000,
        3 => 14_000_000,
        // Both CaptureParams and CaptureSetting carry bits/second on the wire.
        6 => custom.clamp(1_000_000, MAX_CUSTOM_BITRATE_MBPS as i32 * 1_000_000) as u32,
        _ => 8_000_000,
    }
}

/// Unsupported operations never succeed silently or reach a local side-effect handler.
pub(crate) fn input_event(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(match PbControlMessage::decode(bytes)?.payload {
        Some(PbPayload::InputEvent(bytes)) => Some(bytes),
        _ => None,
    })
}
pub(crate) fn input_action(
    bytes: &[u8],
) -> Result<Option<crate::features::host::input::wire::Event>> {
    use crate::features::host::input::wire::{Command, Event};
    let Some(PbPayload::SimpleAction(action)) = PbControlMessage::decode(bytes)?.payload else {
        return Ok(None);
    };
    Ok(Some(match action.action {
        2 => Event::Command(Command::Desktop),
        3 => Event::Command(Command::TaskView),
        10 => Event::Command(Command::Lock),
        13 => Event::Command(Command::TaskManager),
        11 => {
            #[derive(Clone, PartialEq, prost::Message)]
            struct Toggle {
                #[prost(int32, tag = "1")]
                key: i32,
                #[prost(int32, tag = "2")]
                state: i32,
            }
            let Some(PbSimpleActionParams::KeyToggle(bytes)) = action.params else {
                anyhow::bail!("缺少锁定键参数")
            };
            let toggle = Toggle::decode(bytes.as_slice())?;
            let key = match toggle.key {
                1 => 20,
                2 => 144,
                3 => 145,
                _ => anyhow::bail!("无效锁定键类型"),
            };
            Event::Key {
                key,
                down: false,
                interruptible: false,
                toggle: Some(toggle.state == 2),
            }
        }
        _ => return Ok(None),
    }))
}

pub(crate) fn receive(
    bytes: &[u8],
    control: bool,
    screen: Option<&Screen>,
    config: &mut VideoConfig,
    negotiated: &crate::features::host::format::Negotiated,
) -> Result<Received> {
    if bytes.first() == Some(&b'{') {
        return Ok(Received::default());
    }
    let msg = PbControlMessage::decode(bytes)?;
    match msg.payload {
        Some(PbPayload::SimpleAction(action)) if control && matches!(action.action, 0 | 1) => {
            tracing::debug!(
                action = action.action,
                sequence = msg.seq,
                "host PB handshake received"
            );
            Ok(Received {
                handshake_complete: action.action == 1,
                handshake_request: action.action == 0,
                messages: if action.action == 0 {
                    vec![echo(msg.seq, msg.timestamp, false)]
                } else {
                    Vec::new()
                },
                clipboard: match &action.params {
                    Some(PbSimpleActionParams::FeatureFlag(flags)) => Some(flags.clipboard),
                    _ => None,
                },
                files: match &action.params {
                    Some(PbSimpleActionParams::FeatureFlag(flags)) => Some(
                        crate::features::file_transfer::host::Capabilities::from_levels(
                            flags.file_transfer_ftp,
                            flags.file_transfer_ftp2,
                        ),
                    ),
                    _ => None,
                },
                control_screen_reports: match action.params {
                    Some(PbSimpleActionParams::FeatureFlag(flags)) => {
                        Some(flags.capture_setting >= 1)
                    }
                    _ if action.action == 1 => Some(false),
                    _ => None,
                },
                ..Default::default()
            })
        }
        Some(PbPayload::SimpleAction(action)) if !control && action.action == 22 => {
            // S560F60 -> S44BD40: refresh current information, never replay
            // the official implementation's display-management side effects.
            Ok(Received {
                refresh_state: true,
                refresh_secure: true,
                ..Default::default()
            })
        }
        Some(PbPayload::QuerySystemState(bytes)) => {
            #[derive(Clone, PartialEq, prost::Message)]
            struct Query {
                #[prost(int32, tag = "1")]
                id: i32,
            }
            Ok(Received {
                refresh_secure: Query::decode(bytes.as_slice())?.id == 1,
                ..Default::default()
            })
        }
        Some(PbPayload::SimpleAction(action)) if !control && matches!(action.action, 7 | 8) => {
            let screen = screen.context("尚未启动画面")?;
            let args: serde_json::Value = if action.args.is_empty() {
                serde_json::Value::Null
            } else {
                serde_json::from_str(&action.args)?
            };
            // The current server also has a silent-upgrade variant of action 8.
            // Only the explicitly selected existing screen is in our scope.
            anyhow::ensure!(
                action.action == 7 || args.get("scene").is_none_or(|v| v.as_str() == Some("")),
                "不支持的采集重启场景"
            );
            anyhow::ensure!(
                args.get("screen_id")
                    .and_then(serde_json::Value::as_i64)
                    .or_else(|| args.get("screen_id").is_none().then_some(-1))
                    .is_some_and(|id| id == -1 || id == i64::from(screen.id)),
                "只能启停已选择的屏幕"
            );
            config.capturing = action.action == 8;
            tracing::info!(
                capturing = config.capturing,
                "host capture selection applied"
            );
            // CaptureChange/ScreenSources follow the capture owner's actual transition.
            Ok(Received::default())
        }
        Some(PbPayload::RpcRequest(bytes)) if !control => {
            let request = PbRpcRequest::decode(bytes.as_slice())?;
            let header = request.request_header.map(|h| PbResponseHeader {
                request_id: h.request_id,
            });
            let payload = if let Some(PbRpcRequestPayload::CaptureSetting(setting)) =
                request.payload
            {
                let mut errors = Vec::new();
                let screen = screen.context("尚未启动画面")?;
                if (setting.resolution_width > 0 && setting.resolution_width as u32 != screen.width)
                    || (setting.resolution_height > 0
                        && setting.resolution_height as u32 != screen.height)
                    || (setting.resolution_pixel_width > 0
                        && setting.resolution_pixel_width as u32 != screen.width)
                    || (setting.resolution_pixel_height > 0
                        && setting.resolution_pixel_height as u32 != screen.height)
                    || (setting.dpi_scale > 0 && screen.dpi_scale != Some(setting.dpi_scale as u32))
                    || !matches!(setting.resolution_type, 0 | 1)
                {
                    errors.push(PbError {
                        error_code: -1,
                        error_message: "不支持所请求的显示设置".into(),
                        ..Default::default()
                    });
                } else if setting.screen_id != EXISTING_SESSION_TRACKS
                    && setting.screen_id != screen.id
                {
                    errors.push(PbError {
                        error_code: -1,
                        error_message: "只能操作当前连接的显示器".into(),
                        ..Default::default()
                    });
                } else if setting.max_scale_width != 0
                    && setting.max_scale_height != 0
                    && (setting.max_scale_width < 2 || setting.max_scale_height < 2)
                {
                    errors.push(PbError {
                        error_code: -1,
                        error_message: "无效的画面缩放约束".into(),
                        ..Default::default()
                    });
                } else {
                    let mut next = *config;
                    // S543710: both axes must be present; an incomplete pair uses
                    // normal decoder negotiation, rather than retaining a stale RPC.
                    next.requested_maximum =
                        (setting.max_scale_width > 0 && setting.max_scale_height > 0).then_some((
                            setting.max_scale_width as u32,
                            setting.max_scale_height as u32,
                        ));
                    next.fps = fps(setting.fps, setting.fps_count);
                    next.requested_fps = fps(setting.fps, 0);
                    next.fps_limit = if setting.fps_count > 0 {
                        setting.fps_count as u32
                    } else {
                        144
                    };
                    next.cursor_capture = setting.cursor_capture;
                    next.bitrate = bitrate(
                        setting.frame_quality,
                        setting.auto_frame_quality,
                        setting.max_custom_bitrate,
                    );
                    next.quality = quality(setting.frame_quality);
                    next.auto_quality = auto_quality(setting.auto_frame_quality);
                    next.revision = next.revision.wrapping_add(1);
                    let codec = crate::features::host::format::Codec::from_wire(setting.codec_type);
                    let selected = if setting.codec_type == 0 || codec.is_some() {
                        negotiated.apply(
                            &mut next,
                            codec,
                            if setting.chroma_format == 3 { 3 } else { 1 },
                            setting.enable_hdr,
                            (screen.width, screen.height),
                        )
                    } else {
                        Err(anyhow!("不支持的编码类型"))
                    };
                    match selected {
                        Ok(()) => {
                            // S5419C0/S542F40: -6 is a color-result notification,
                            // including successful enable/disable, not an RPC failure.
                            let color_result = if setting.chroma_format == 3 {
                                if next.format.chroma != 3 {
                                    Some(if negotiated.encoder_true_color() {
                                        2
                                    } else {
                                        1
                                    })
                                } else if config.format.chroma != 3 {
                                    Some(0)
                                } else {
                                    None
                                }
                            } else if config.format.chroma == 3 {
                                Some(5)
                            } else {
                                None
                            };
                            if let Some(code) = color_result {
                                errors.push(PbError {
                                    error_code: -6,
                                    error_detail: serde_json::json!({"error_code":code})
                                        .to_string(),
                                    ..Default::default()
                                });
                            }
                            // S47DB30 writes the final fps-degrade flag from the
                            // receiver's fps_count versus this source's actual rate.
                            if setting.fps_count > 0 && setting.fps_count as u32 > screen.fps {
                                errors.push(PbError {
                                    error_code: CAPTURE_RESULT_FPS_ADJUSTED,
                                    ..Default::default()
                                });
                            }
                            *config = next;
                        }
                        Err(error) => errors.push(PbError {
                            error_code: -1,
                            error_message: error.to_string(),
                            ..Default::default()
                        }),
                    }
                    tracing::info!(
                        fps = config.fps,
                        bitrate = config.bitrate,
                        quality = config.quality,
                        maximum = ?config.maximum,
                        "host capture setting applied"
                    );
                }
                // S5419C0 appends an explicit success only when no result exists.
                if errors.is_empty() {
                    errors.push(PbError::default());
                }
                Some(PbRpcResponsePayload::CaptureSetting(
                    PbCaptureSettingResponse { errors },
                ))
            } else if let Some(PbRpcRequestPayload::SendVideoTrack(tracks)) = request.payload {
                let valid = tracks.video_track_index.iter().all(|index| *index == 0);
                if valid {
                    config.sending = tracks.video_track_index.contains(&0);
                    tracing::info!(
                        sending = config.sending,
                        "host video track selection applied"
                    );
                }
                Some(PbRpcResponsePayload::SendVideoTrackRsp(
                    PbSendVideoTrackResponse {
                        error_code: if valid { 0 } else { -1 },
                    },
                ))
            } else {
                None
            };
            if let Some(payload) = payload {
                Ok(vec![
                    PbControlMessage {
                        seq: msg.seq,
                        timestamp: msg.timestamp,
                        payload: Some(PbPayload::RpcResponse(PbRpcResponse {
                            response_header: header,
                            payload: Some(payload),
                        })),
                    }
                    .encode_to_vec(),
                ]
                .into())
            } else {
                Ok(Received::default())
            }
        }
        _ => Ok(Received::default()),
    }
}

pub(crate) fn cursor_report(state: Vec<u8>) -> Vec<u8> {
    // ControllerWrapper::onReceiveControlData consumes this notification before
    // generic PB dispatch. TEXT and signal callbacks do not update its cursor.
    PbControlMessage {
        payload: Some(PbPayload::SystemStateChange(state)),
        ..Default::default()
    }
    .encode_to_vec()
}

pub(crate) fn clipboard_permission(enabled: bool) -> Vec<u8> {
    PbControlMessage {
        payload: Some(PbPayload::SystemStateChange(
            super::wire::ClipboardPermissionState {
                files: Some(super::wire::ClipboardPermission { enabled }),
            }
            .encode_to_vec(),
        )),
        ..Default::default()
    }
    .encode_to_vec()
}
