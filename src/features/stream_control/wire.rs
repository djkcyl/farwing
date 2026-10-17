//! Protobuf wire schema and envelope encoding.
use super::{
    ACTION_TYPE_ECHO_REQUEST, ACTION_TYPE_ECHO_RESPONSE, annotation, display_topology, microphone,
};
use anyhow::Result;
use prost::Message as _;
use std::time::{SystemTime, UNIX_EPOCH};

pub(crate) fn encode_pb_echo_request() -> Vec<u8> {
    encode_pb_echo(ACTION_TYPE_ECHO_REQUEST, String::new(), 0, 0)
}

pub(crate) fn encode_read_only_feature_flags() -> Vec<u8> {
    PbFeatureFlag::read_only_viewer().encode_to_vec()
}

pub(super) fn encode_pb_echo_response(sequence: i64, timestamp: i64) -> Vec<u8> {
    encode_pb_echo(
        ACTION_TYPE_ECHO_RESPONSE,
        format!("{{ \"seq\" : {sequence} }}"),
        sequence,
        timestamp,
    )
}

pub(super) fn encode_pb_echo(action: i32, args: String, seq: i64, timestamp: i64) -> Vec<u8> {
    PbControlMessage {
        seq,
        timestamp,
        payload: Some(PbPayload::SimpleAction(PbSimpleAction {
            action,
            args,
            params: Some(PbSimpleActionParams::FeatureFlag(
                PbFeatureFlag::read_only_viewer(),
            )),
        })),
    }
    .encode_to_vec()
}

pub(super) fn encode_envelope(sequence: i64, payload: PbPayload) -> Vec<u8> {
    let message = PbControlMessage {
        seq: sequence,
        timestamp: i64::try_from(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(i64::MAX),
        payload: Some(payload),
    };
    message.encode_to_vec()
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbControlMessage {
    #[prost(int64, tag = "1")]
    pub(super) seq: i64,
    #[prost(int64, tag = "2")]
    pub(super) timestamp: i64,
    #[prost(
        oneof = "PbPayload",
        tags = "3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28"
    )]
    pub(super) payload: Option<PbPayload>,
}

pub(crate) fn encode_port_mapping(payload: Vec<u8>) -> Vec<u8> {
    PbControlMessage {
        seq: 0,
        timestamp: 0,
        payload: Some(PbPayload::PortMappingFrame(payload)),
    }
    .encode_to_vec()
}

pub(crate) fn decode_port_mapping(bytes: &[u8]) -> Result<Option<Vec<u8>>> {
    Ok(match PbControlMessage::decode(bytes)?.payload {
        Some(PbPayload::PortMappingFrame(frame)) => Some(frame),
        _ => None,
    })
}

// main.proto's complete oneof range, read from the shipped descriptor.
// Opaque payloads remain mutually exclusive without implementing their
// non-viewing business operations. Media-specific pending handlers stay in C02.
#[derive(Clone, PartialEq, prost::Oneof)]
pub(super) enum PbPayload {
    #[prost(message, tag = "3")]
    SimpleAction(PbSimpleAction),
    #[prost(bytes, tag = "4")]
    LaunchApp(Vec<u8>),
    #[prost(bytes, tag = "5")]
    ShowApp(Vec<u8>),
    #[prost(bytes, tag = "6")]
    MumuOperate(Vec<u8>),
    #[prost(message, tag = "7")]
    Screens(PbScreenSources),
    #[prost(bytes, tag = "8")]
    CaptureChange(Vec<u8>),
    #[prost(bytes, tag = "9")]
    CaptureConfig(Vec<u8>),
    #[prost(bytes, tag = "10")]
    RomMessage(Vec<u8>),
    #[prost(bytes, tag = "11")]
    SendToRom(Vec<u8>),
    #[prost(message, tag = "12")]
    ReportError(PbReportError),
    #[prost(bytes, tag = "13")]
    SystemMetrics(Vec<u8>),
    #[prost(message, tag = "14")]
    ReportQosStats(PbReportQosStats),
    #[prost(bytes, tag = "15")]
    SystemStateChange(Vec<u8>),
    #[prost(bytes, tag = "16")]
    QuerySystemState(Vec<u8>),
    #[prost(bytes, tag = "17")]
    ClipboardChange(Vec<u8>),
    #[prost(bytes, tag = "18")]
    CodecNegotiation(Vec<u8>),
    #[prost(bytes, tag = "19")]
    CaptureConfigResponse(Vec<u8>),
    #[prost(bytes, tag = "20")]
    InputEvent(Vec<u8>),
    #[prost(bytes, tag = "21")]
    RpcRequest(Vec<u8>),
    #[prost(message, tag = "22")]
    RpcResponse(PbRpcResponse),
    #[prost(bytes, tag = "23")]
    ActiveWindowChange(Vec<u8>),
    #[prost(bytes, tag = "24")]
    LaunchCloudPcApp(Vec<u8>),
    #[prost(bytes, tag = "25")]
    CaptureSettingSync(Vec<u8>),
    #[prost(bytes, tag = "26")]
    RemoteDownloadPath(Vec<u8>),
    #[prost(bytes, tag = "27")]
    PortMappingFrame(Vec<u8>),
    #[prost(bytes, tag = "28")]
    TerminalSessionChanged(Vec<u8>),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbReportError {
    #[prost(int32, tag = "1")]
    pub(super) action: i32,
    #[prost(int32, tag = "2")]
    pub(super) error_code: i32,
    #[prost(string, tag = "3")]
    pub(super) error_msg: String,
    #[prost(int32, tag = "4")]
    pub(super) type_value: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbReportQosStats {
    #[prost(string, tag = "1")]
    pub(super) encoder_type: String,
    #[prost(string, tag = "2")]
    pub(super) capture_type: String,
    #[prost(uint64, tag = "3")]
    pub(super) probe_bps: u64,
    #[prost(int32, tag = "4")]
    pub(super) video_quality: i32,
    #[prost(uint64, tag = "5")]
    pub(super) fast_bitrate: u64,
    #[prost(uint64, tag = "6")]
    pub(super) general_bitrate: u64,
    #[prost(uint64, tag = "7")]
    pub(super) hd_bitrate: u64,
    #[prost(uint64, tag = "8")]
    pub(super) bluray_bitrate: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbSimpleAction {
    #[prost(int32, tag = "1")]
    pub(super) action: i32,
    #[prost(string, tag = "2")]
    pub(super) args: String,
    #[prost(oneof = "PbSimpleActionParams", tags = "3, 4, 5")]
    pub(super) params: Option<PbSimpleActionParams>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(super) enum PbSimpleActionParams {
    #[prost(bytes, tag = "3")]
    KeyToggle(Vec<u8>),
    #[prost(message, tag = "4")]
    FeatureFlag(PbFeatureFlag),
    #[prost(uint32, tag = "5")]
    Value(u32),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbFeatureFlag {
    #[prost(int32, tag = "1")]
    pub(super) capture_setting: i32,
    #[prost(int32, tag = "2")]
    pub(super) simple_action: i32,
    #[prost(int32, tag = "3")]
    pub(super) system_metrics: i32,
    #[prost(int32, tag = "4")]
    pub(super) private_screen: i32,
    #[prost(int32, tag = "5")]
    pub(super) update_acquire: i32,
    #[prost(int32, tag = "6")]
    pub(super) file_transfer_ftp: i32,
    #[prost(int32, tag = "7")]
    pub(super) file_transfer_ftp2: i32,
    #[prost(int32, tag = "8")]
    pub(super) clipboard: i32,
    #[prost(int32, tag = "9")]
    pub(super) qos_stat: i32,
    #[prost(int32, tag = "10")]
    pub(super) mumu_control: i32,
    #[prost(int32, tag = "11")]
    pub(super) virtual_mouse_device: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ClipboardPermissionState {
    #[prost(message, optional, tag = "4")]
    pub(super) files: Option<ClipboardPermission>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct ClipboardPermission {
    #[prost(bool, tag = "1")]
    pub(super) enabled: bool,
}

impl PbFeatureFlag {
    pub(super) fn read_only_viewer() -> Self {
        Self {
            capture_setting: crate::protocol::official_version::CAPTURE_SETTING_LEVEL as i32,
            simple_action: 0,
            system_metrics: 0,
            private_screen: 0,
            update_acquire: 0,
            file_transfer_ftp: 2,
            file_transfer_ftp2: 2,
            clipboard: 3,
            qos_stat: 1,
            mumu_control: 0,
            virtual_mouse_device: 0,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbRequestHeader {
    #[prost(int64, tag = "1")]
    pub(super) request_id: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbResponseHeader {
    #[prost(int64, tag = "1")]
    pub(super) request_id: i64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbRpcRequest {
    #[prost(message, optional, tag = "1")]
    pub(super) request_header: Option<PbRequestHeader>,
    #[prost(
        oneof = "PbRpcRequestPayload",
        tags = "2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29"
    )]
    pub(super) payload: Option<PbRpcRequestPayload>,
}

// Current rpc_main.proto: even unsupported operations replace an earlier oneof
// member. Ignoring their tags must not execute a preceding supported operation.
#[derive(Clone, PartialEq, prost::Oneof)]
pub(super) enum PbRpcRequestPayload {
    #[prost(message, tag = "2")]
    CaptureSetting(PbCaptureSettingRequest),
    #[prost(bytes, tag = "3")]
    FpsSetting(Vec<u8>),
    #[prost(bytes, tag = "4")]
    VideoQualitySetting(Vec<u8>),
    #[prost(bytes, tag = "5")]
    CursorSetting(Vec<u8>),
    #[prost(bytes, tag = "6")]
    ResolutionSetting(Vec<u8>),
    #[prost(bytes, tag = "7")]
    PrivateScreenSetting(Vec<u8>),
    #[prost(bytes, tag = "8")]
    FileTransfer(Vec<u8>),
    #[prost(bytes, tag = "9")]
    Clipboard(Vec<u8>),
    #[prost(bytes, tag = "10")]
    ClipboardText(Vec<u8>),
    #[prost(bytes, tag = "11")]
    MouseSwitch(Vec<u8>),
    #[prost(message, tag = "12")]
    CreateVirtualDisplay(display_topology::PbCreateVirtualDisplay),
    #[prost(message, tag = "13")]
    RemoveVirtualDisplay(display_topology::PbRemoveVirtualDisplay),
    #[prost(message, tag = "14")]
    QuitSuperScreen(display_topology::PbQuitSuperScreen),
    #[prost(message, tag = "15")]
    SendVideoTrack(PbSendVideoTrackRequest),
    #[prost(bytes, tag = "16")]
    QueryPluginSetting(Vec<u8>),
    #[prost(bytes, tag = "17")]
    UpdatePluginSetting(Vec<u8>),
    #[prost(bytes, tag = "18")]
    InstallPlugin(Vec<u8>),
    #[prost(bytes, tag = "19")]
    EnablePlugin(Vec<u8>),
    #[prost(bytes, tag = "20")]
    PluginNotification(Vec<u8>),
    #[prost(message, tag = "21")]
    VirtualAudioDriverPolicy(microphone::PolicyRequest),
    #[prost(bytes, tag = "22")]
    FlipScreen(Vec<u8>),
    #[prost(bytes, tag = "23")]
    QuickLaunchScan(Vec<u8>),
    #[prost(bytes, tag = "24")]
    QuickLaunchIcon(Vec<u8>),
    #[prost(bytes, tag = "25")]
    UpdateScreenSaver(Vec<u8>),
    #[prost(message, tag = "26")]
    EnterSuperScreen(display_topology::PbEnterSuperScreen),
    #[prost(message, tag = "27")]
    Draw(annotation::PbDrawRequest),
    #[prost(bytes, tag = "28")]
    TerminalCheck(Vec<u8>),
    #[prost(bytes, tag = "29")]
    TerminalRename(Vec<u8>),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbSendVideoTrackRequest {
    #[prost(int32, repeated, tag = "1")]
    pub(super) video_track_index: Vec<i32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbSendVideoTrackResponse {
    #[prost(int32, tag = "1")]
    pub(super) error_code: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbRpcResponse {
    #[prost(message, optional, tag = "1")]
    pub(super) response_header: Option<PbResponseHeader>,
    #[prost(
        oneof = "PbRpcResponsePayload",
        // The list has to name every tag the oneof can carry. 25 is skipped
        // in the reference schema, so 26 has to be spelled out here too or
        // prost rejects the variant.
        tags = "2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 26"
    )]
    pub(super) payload: Option<PbRpcResponsePayload>,
}

#[derive(Clone, PartialEq, prost::Oneof)]
pub(super) enum PbRpcResponsePayload {
    #[prost(message, tag = "2")]
    CaptureSetting(PbCaptureSettingResponse),
    #[prost(bytes, tag = "3")]
    PrivateScreenSetting(Vec<u8>),
    #[prost(bytes, tag = "4")]
    FileTransferFtpResponse(Vec<u8>),
    #[prost(bytes, tag = "5")]
    ClipResponse(Vec<u8>),
    #[prost(bytes, tag = "6")]
    TextChangeResponse(Vec<u8>),
    #[prost(bytes, tag = "7")]
    MouseSwitchResponse(Vec<u8>),
    #[prost(bytes, tag = "8")]
    CreateVirtualDisplayRsp(Vec<u8>),
    #[prost(bytes, tag = "9")]
    RemoveVirtualDisplayRsp(Vec<u8>),
    #[prost(bytes, tag = "10")]
    QuitSuperScreen(Vec<u8>),
    #[prost(message, tag = "11")]
    SendVideoTrackRsp(PbSendVideoTrackResponse),
    #[prost(bytes, tag = "12")]
    QueryPluginSettingRsp(Vec<u8>),
    #[prost(bytes, tag = "13")]
    UpdatePluginSettingRsp(Vec<u8>),
    #[prost(bytes, tag = "14")]
    StartDownloadAndInstallPluginRsp(Vec<u8>),
    #[prost(bytes, tag = "15")]
    PluginEnableNotificationRsp(Vec<u8>),
    #[prost(bytes, tag = "16")]
    PluginNotificationRsp(Vec<u8>),
    #[prost(bytes, tag = "17")]
    VirtualAudioDriverPolicyRsp(Vec<u8>),
    #[prost(bytes, tag = "18")]
    FlipScreenRsp(Vec<u8>),
    #[prost(bytes, tag = "19")]
    QuickLaunchScanRsp(Vec<u8>),
    #[prost(bytes, tag = "20")]
    QuickLaunchAppIconRsp(Vec<u8>),
    #[prost(bytes, tag = "21")]
    UpdateScreenSaverRsp(Vec<u8>),
    #[prost(bytes, tag = "22")]
    EnterSuperScreenRep(Vec<u8>),
    #[prost(message, tag = "23")]
    DrawResp(annotation::PbDrawResponse),
    // UU 4.41.2.2602 rpc_main.proto uses 24 and 26 (25 is absent), as does
    // the reference server's own `RpcResponse` descriptor. The terminal
    // handlers encode the bodies themselves, so the oneof keeps them opaque.
    #[prost(bytes, tag = "24")]
    TerminalCheckEnvRsp(Vec<u8>),
    // 26, not 25: a reply on a field the client does not know is dropped.
    #[prost(bytes, tag = "26")]
    TerminalRenameSessionRsp(Vec<u8>),
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbCaptureSettingRequest {
    #[prost(int32, tag = "1")]
    pub(super) fps: i32,
    #[prost(int32, tag = "2")]
    pub(super) frame_quality: i32,
    #[prost(bool, tag = "3")]
    pub(super) cursor_capture: bool,
    #[prost(int32, tag = "4")]
    pub(super) screen_id: i32,
    #[prost(int32, tag = "5")]
    pub(super) resolution_width: i32,
    #[prost(int32, tag = "6")]
    pub(super) resolution_height: i32,
    #[prost(int32, tag = "7")]
    pub(super) chroma_format: i32,
    #[prost(int32, tag = "8")]
    pub(super) max_custom_bitrate: i32,
    #[prost(int32, tag = "9")]
    pub(super) dpi_scale: i32,
    #[prost(int32, tag = "10")]
    pub(super) resolution_type: i32,
    #[prost(bool, tag = "11")]
    pub(super) enable_hdr: bool,
    #[prost(int32, tag = "12")]
    pub(super) auto_frame_quality: i32,
    #[prost(int32, tag = "13")]
    pub(super) codec_type: i32,
    #[prost(int32, tag = "14")]
    pub(super) max_scale_width: i32,
    #[prost(int32, tag = "15")]
    pub(super) max_scale_height: i32,
    #[prost(int32, tag = "16")]
    pub(super) resolution_pixel_width: i32,
    #[prost(int32, tag = "17")]
    pub(super) resolution_pixel_height: i32,
    #[prost(int32, tag = "18")]
    pub(super) fps_count: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbCaptureSettingResponse {
    #[prost(message, repeated, tag = "1")]
    pub(super) errors: Vec<PbError>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbError {
    #[prost(int32, tag = "1")]
    pub(super) error_code: i32,
    #[prost(string, tag = "2")]
    pub(super) error_message: String,
    #[prost(string, tag = "3")]
    pub(super) error_detail: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbScreenSources {
    #[prost(message, repeated, tag = "1")]
    pub(super) screens: Vec<PbScreen>,
    #[prost(int32, tag = "2")]
    pub(super) current_screen_id: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbScreen {
    /// OpenUUYC display_geometry v1, sent only after explicit capability negotiation.
    /// Never repurpose init_resolution: it is a connection snapshot, not the panel mode.
    #[prost(message, optional, tag = "100")]
    pub(super) openuuyc_geometry: Option<PbDisplayGeometry>,
    #[prost(int32, tag = "1")]
    pub(super) id: i32,
    #[prost(int32, tag = "2")]
    pub(super) fps: i32,
    #[prost(message, repeated, tag = "3")]
    pub(super) resolutions: Vec<PbWinRect>,
    #[prost(message, optional, tag = "4")]
    pub(super) current_resolution: Option<PbWinRect>,
    #[prost(int32, tag = "5")]
    pub(super) screen_type: i32,
    #[prost(message, optional, tag = "6")]
    pub(super) init_resolution: Option<PbWinRect>,
    #[prost(bool, tag = "7")]
    pub(super) is_primary_screen: bool,
    #[prost(double, tag = "8")]
    pub(super) dpr: f64,
    #[prost(message, optional, tag = "9")]
    pub(super) dpi_scale: Option<PbDpiScale>,
    #[prost(string, tag = "10")]
    pub(super) display_name: String,
    #[prost(int32, tag = "11")]
    pub(super) resolution_type: i32,
    #[prost(int32, tag = "12")]
    pub(super) video_track_index: i32,
    #[prost(int32, tag = "13")]
    pub(super) builtin_screen_type: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbDisplayGeometry {
    #[prost(uint32, tag = "1")]
    pub version: u32,
    #[prost(uint32, tag = "2")]
    pub width: u32,
    #[prost(uint32, tag = "3")]
    pub height: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbWinRect {
    #[prost(int32, tag = "1")]
    pub(super) left: i32,
    #[prost(int32, tag = "2")]
    pub(super) top: i32,
    #[prost(int32, tag = "3")]
    pub(super) width: i32,
    #[prost(int32, tag = "4")]
    pub(super) height: i32,
    #[prost(int32, tag = "5")]
    pub(super) pixel_width: i32,
    #[prost(int32, tag = "6")]
    pub(super) pixel_height: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub(super) struct PbDpiScale {
    #[prost(int32, tag = "1")]
    pub(super) current_dpi: i32,
    #[prost(int32, tag = "2")]
    pub(super) recommended_dpi: i32,
    #[prost(int32, repeated, tag = "3")]
    pub(super) dpis: Vec<i32>,
}

#[cfg(test)]
mod terminal_rpc_tests {
    use super::*;
    use prost::Message;

    /// The response oneof numbers the client reads, from the reference
    /// server's own `RpcResponse` descriptor. 25 is skipped there, and a
    /// reply on a field the client does not know is dropped, leaving the
    /// phone waiting for an answer that never comes.
    #[test]
    fn terminal_responses_use_the_reference_field_numbers() {
        let tag_of = |payload: PbRpcResponsePayload| {
            let bytes = PbRpcResponse {
                response_header: None,
                payload: Some(payload),
            }
            .encode_to_vec();
            // A oneof member is a length-delimited field, so byte 1 holds
            // `field << 3 | 2` in its varint form.
            let (mut value, mut shift) = (0u32, 0);
            for byte in &bytes {
                value |= u32::from(byte & 0x7f) << shift;
                if byte & 0x80 == 0 {
                    break;
                }
                shift += 7;
            }
            (value >> 3, value & 7)
        };

        assert_eq!(
            tag_of(PbRpcResponsePayload::TerminalCheckEnvRsp(vec![0x08, 0x00])),
            (24, 2)
        );
        assert_eq!(
            tag_of(PbRpcResponsePayload::TerminalRenameSessionRsp(vec![])),
            (26, 2)
        );
        assert_eq!(
            tag_of(PbRpcResponsePayload::DrawResp(Default::default())),
            (23, 2)
        );
    }
}
