# Reference inventory (OpenUUYC v1.0.0-rc.3, commit 506df56)

Working notes for the Farwing migration. Paths refer to the reference tree
(`src/...`), which was copied unchanged into this repository as the starting
point. This file records what the UU-bound code did so every producer and
consumer can be re-pointed at Farwing Protocol / Farwing Server.

## 1. UU official HTTP API (src/account/api/*, nrd_http.rs)

Hosts: `api.nrd.nie.163.com` (primary), `api-dcdn.nrd.nie.163.com` (failover),
`api-ipv4.nrd.nie.163.com` (WoL info only). HMAC-signed `X-Param-*` headers,
envelope `{code,msg,data}`. Scopes: Platform / Device / AccountDevice (+Bearer).

| # | Endpoint | Consumer |
|---|----------|----------|
| 1-3 | QR login gen / status long-poll(65s) / by_qrcode | login/mod.rs, app/mod.rs |
| 4-5 | SMS code / login by mobile | login/sms.rs |
| 6 | GET user/info (session restore, nickname) | session_restore.rs, controller/assist.rs |
| 7 | POST user/logout | client/mod.rs logout |
| 8 | POST device/windows/init (unauth, hw info) -> device_id(16 [a-z0-9]) | session/device_session.rs |
| 9 | GET device/list -> current_device, my_binded_devices[] DeviceInfo | device center, CLI, connect, takeover, viewer switch, files, ports, diag |
| 10 | GET device/groups/of/my (desktop/mobile/tv) | account device mgmt, rename/unbind/power precheck |
| 11 | GET device/{id}/detail -> [[label,value]] | device details panel |
| 12 | PUT device/{id} {alias} | rename |
| 13 | POST device/{id}/unbind | remove device |
| 14 | POST device/controllable {controllable} (2s) | allow-control toggle (host.rs Change) |
| 15 | POST room/create {last_controlled_interval} -> RoomSession | host presence |
| 16 | POST room/join/by_device/{id} {force_join} -> RoomSession | controller connect (retry 5x except -1/1120/2002/2006/2007/4042) |
| 17-18 | guest/create, guest/room/create | guest hosting (assist without login) |
| 19 | v2 room/share/control_mode {connect_id} | assist controller |
| 20 | v2 room/join/share/by_code -> JoinReply; 1136 wait confirm, 1131 wrong code, 1133 code required | assist controller |
| 21 | v2 room/join/share/by_confirmation (65s long poll) | assist controller |
| 22 | v2 room/share/cancel_remote_assist | assist cancel |
| 23 | room/join/refetch {token} | assist reconnect |
| 24-26 | device/share/saved recent/favorites CRUD | assist history/favorites UI |
| 27 | device/share/info/refresh -> connect_id (9 digits) | host assist id |
| 28 | v2 room/share/upload_control_mode | host answers get_control_mode push |
| 29 | v2 room/share/upload_sign (sha256(salt‖code)) | host answers remote_control push |
| 30 | v2 room/share/confirmation | host answers by_confirmation push |
| 31 | device/{id}/wake_on_lan/req | power: WoL |
| 32 | device/{id}/reboot | power: reboot |
| 33 | transport/{id}/rpc/request {p:"shutdown"} | power: shutdown |
| 34 | transport/rpc/response (host) | power result |
| 35 | device/wake_on_lan/info {mac,inner_ip,subnet_mask} (ipv4 host) | WoL setup |
| 36 | device/set_support_wol | WoL setup |
| 37 | tool/configures_with_version (controlled/controlling_ability_map, custom_bitrate_limit, app_white_list, win_keylock_optimize) | feature gating, host input config |
| 38 | release/trigger/{id}/update | remote upgrade prompt |
| 39-41 | tool/fp/token, upload to fp.ps.netease.com, device/wallpaper | wallpaper publication |

Error codes: 1001/1002/1120 invalid credentials -> clear session; 1120 elsewhere
-> retire session; 1006 device-init integrity; -1 synthesized network error;
1131/1133/1136 assist; 1190 QR expired.

Push (bmsg_push over host signaling socket): device_binded, device_info_changed,
device_unbind; get_control_mode, remote_control{salt}, remote_control_by_confirmation,
remote_control_by_confirmation_cancel; wake_on_lan{target_device}, device_reboot,
transport_rpc{p:shutdown,call_id}.

Other remote deps: GitHub releases API for update check (app/updates.rs),
NetEase license/privacy links (login/sms.rs), wallpaper URL downloads.

## 2. Credentials / identity

- keyring service `com.openuuyc.session`: `native-device-identity`
  `{schema,client_id(uuid),device_id,system_id(SMBIOS),machine_guid,name,controllable}`,
  `default-nrd-login` `{schema,token,user_id,nickname}`, `registration-generation`="2".
  Farwing has no generation marker any more (its `registration-generation` =
  `farwing-1`, which wiped identity/session records written before it, was
  removed), and Farwing v1 identity records are not replaced: they fail to load.
- Vault mode (installed service): `%ProgramData%\OpenUUYC\host\<sha256>.secret`.
- Locks: `%LOCALAPPDATA%\openuuyc\*.lock`.
- Account generation = sha256(user_id\0token); guest = `guest:<client_id>`.
- Other keyring entries: `com.openuuyc.host.<sha256(user)>/<device>`, `.host.guest`,
  `.assist.<sha256(user)>`, `.wallpaper`, `.host.update-resume`, `.viewing.*`,
  `.audio.*`, `.device-preferences.*`, `.microphone.*`.

## 3. Signaling (src/transport/signal/*)

Engine.IO v4 / Socket.IO v4 over WSS, headers x-nrd-auth (room token),
x-nrd-controlling, x-nrd-reconn-key, streamer_version, streamer_flag.
SDP gzip'd as binary attachments. Events:

- C->S: `room_info` (ack1), `control` (ack2, ConnectOptions protobuf attachment +
  streamer_data JSON incl. device_capability + openuuyc_* extension flags),
  `soac` offer/restart_ice/candidate, `refresh_reconnect_key`, `leave`.
- control ACK -> ControlSessionInfo {client_id, ice_id, iceServers[], force_relay,
  auto_switch_* thresholds}.
- Host: `clear_out`, `soac` answer/candidate, `forward_setting` {device_capability,
  sender_para_info, switch_network_notify}.
- S->C: `soac`, `forward_setting` (incl. `signal_app_data` protobuf -> stream_control
  PbMessageSource::Signal), `leave` (Kicked 2005), `publisher_disconnect` (2006),
  `bmsg_push`, `be-controlled` (host), `left`/`released` (host).
- Timeouts: ws connect (room.ws_connect_timeout_ms), namespace 10s, room_info 10s,
  control 10s, WebRTC connect 12s, close 3s. Retry budget endpoints*2,
  delay streamer_retry_delta_ms*1.5^n capped 25s; physical reconnect keeps media.
- Controller is the SDP offerer; host answers. Remote candidates before answer dropped.
- Host: single active peer; new be-controlled replaces old; early soac queued
  (<=256 msgs, <=8MB) while preparing; revocation -> clear_out + close.
- Presence: Global mutex `OpenUUYC.Presence.{client_id}`; room retry 1-5s random,
  doubling to max_reconnect_delta (300s).
- ICE servers only from control ACK / be-controlled. P2P keeps only stun:,
  Relay => ICE policy relay; switch_network: (1,2)=relay TURNS, (3,3)=all. (Farwing: relay-only uses every relay transport, TLS ranked first.)
- SDP munging: a=x-uuremote-mix-kcp:2, 5 recvonly video transceivers.

## 4. Peer business protocol (data channels)

Channels (controller creates; all ordered+reliable; max-message-size 524288):
CONTROL_DATA_CHANNEL (input JSON, PB ECHO, cursor shape, mouse policy, screens if level>=1;
optionally carried over proprietary mixed-KCP under SCTP: conv 0x23356, MTU 1191, `PCK` prefix,
cmds 85-88, CM256 FEC, SDP `a=x-uuremote-mix-kcp:2`), TEXT_DATA_CHANNEL (PbControlMessage
envelopes sent with text PPID, clipboard, file RPC, OUAN annotation, OUDD drag control, audio
JSON), FILE_DATA_CHANNEL (file blocks, port-mapping frames, OUDD bulk), STREAMER_DATA_CHANNEL
(30s receiver stats JSON, ignored), BINARY_DATA_CHANNEL (OUDIAG01 diagnostics).
Dispatch = content sniffing (magic OUDD\x01 / OUAN\x01 / '{' JSON / trial protobuf decode).

PbControlMessage (stream_control/wire.rs:56-142) seq=1 ts=2 oneof: 3 SimpleAction, 7 Screens,
8 CaptureChange, 12 ReportError, 13 SystemMetrics, 14 ReportQosStats, 15 SystemStateChange,
16 QuerySystemState, 20 InputEvent, 21 RpcRequest, 22 RpcResponse, 25 CaptureSettingSync,
27 PortMappingFrame. RpcRequest oneof: 2 CaptureSetting, 8 FileTransfer, 9 Clipboard,
10 ClipboardText, 11 MouseSwitch, 12 CreateVirtualDisplay, 13 RemoveVirtualDisplay,
14 QuitSuperScreen, 15 SendVideoTrack, 21 VirtualAudioDriverPolicy, 26 EnterSuperScreen, 27 Draw.
ECHO handshake = SimpleAction 0/1 with FeatureFlag levels (capture_setting 6, ftp 2/2,
clipboard 3, qos 1, host adds system_metrics 1, virtual_mouse_device 1); gates CaptureSetting>=1,
bitrate>=4, DPI>=5, HDR>=6, files ftp>=2.

Feature wire summary: input JSON actions (remote_input.rs:122-160 -> host/input/wire.rs);
special_game_mouse H->C; CursorShape SystemStateChange{2}; CaptureSetting RPC + response errors
(0,-1,-2,-3,-5 json,-6 json); ReportQosStats; ScreenSources; SimpleAction 7/8 capture stop/start,
SendVideoTrack, Create/RemoveVirtualDisplay, Enter/QuitSuperScreen; audio JSON Hello/Set/Applied/
Rejected (openuuyc_audio); mic policy RPC 21 + SimpleAction 23-27 status; clipboard envelope
{21 req,22 resp} kinds FormatList/FormatDataAsk/DataBlock/FileDescList/FileContents/Cancel/
DescSegment/AutoSaveComplete; file transfer RPC 8 (13 kinds) + FileBlock on FILE, SystemMetrics
control_allowed; OUDD drag; OUAN annotation (+ official Draw RPC 27); port mapping frame
{session_id,rule_id,stream_id,kind,payload} TCP 0 SYN/3 ACK/4 READY/2 DATA/1 FIN, UDP 16-20;
ReportError -6 (upgrade); OUDIAG01 diagnostics Hello/Request/Progress/Begin/Data/Ack/Finish/
Cancel/Error; Permission/SecureDesktop/CaptureChange (unconsumed by OpenUUYC controller).

Media: standard RTP via webrtc-rs; official_receiver = port of libwebrtc receive pipeline (keep).
Proprietary: rs-fec-cm256 PT 36 (host/fec.rs, fec-*.bin tables from UU SDK), UU RTP ext
video-capture-index / video-frame-is-new-frame / video-frame-sending-delay. ULPFEC/FlexFEC
receivers exist only for official hosts. Controller offers audio_0 sendrecv + 5 recvonly video.

Capability: ConnectOptions protobuf (hand-encoded, session/negotiation.rs) + streamer_data JSON
(device_capability, openuuyc_* flags); DualCapability::negotiate (protocol/capability.rs);
FeaturePolicy by peer platform+version_name vs official_features.json (account/feature_ability.rs);
impersonated versions CLIENT 4.41.2.2602 / SDK V4.7.3 (protocol/official_version.rs).

## 5. Product identity (to rename/isolate)

See the identity survey: named objects (Local\/Global\OpenUUYC.*), pipes (\.\pipe\OpenUUYC.*),
window classes/properties/messages/clipboard formats, service OpenUUYCInputService, Run value,
uninstall key, URI scheme openuuyc-notification, AUMID OpenUUYC.RemoteAccess, toast CLSID
8eafbec2-..., shortcuts, %ProgramFiles%\OpenUUYC, %ProgramData%\OpenUUYC\host vault,
%LOCALAPPDATA%\OpenUUYC (+ lowercase openuuyc locks), keyring com.openuuyc.*, drivers
ROOT\OPENUUYC_{DISPLAY,INPUT,AUDIO}, interface GUIDs, cert CN=OpenUUYC Drivers (pinned SHA-1
CCDA4E99...), plugin ABI symbols openuuyc_*, crates openuuyc-*, xtask/packaging names,
OPENUUYC_* env vars. No profile override exists; installed-copy routing would hijack dev builds.
