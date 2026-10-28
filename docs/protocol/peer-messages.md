# Farwing Peer Protocol v2 消息体

本文列出 FPP v2（[farwing-protocol.md §5](farwing-protocol.md#5-对端协议-fpp-v2)）各帧类型的帧体。protobuf 字段写作 `名称: 类型 = 编号`；`optional` 表示带存在性的字段，`repeated` 为重复字段，`oneof` 内各项只出现一个。每节标题后注明定义所在的源文件，源码中的 prost 结构为权威定义。帧头、通道、请求/响应规则与特性协商见 farwing-protocol.md。

通用约定：

- 请求 / 响应 / 推送由帧头的 `request id` 与 `RESPONSE` 标志区分；失败的响应置 `RESPONSE|ERROR`，帧体为 `PeerError`。
- 坐标“屏内归一化”指相对于该屏幕左上角、取值 0..1。
- 颜色为 0xRRGGBB（另注明者除外）。

## HELLO（0x0001）—— `crates/protocol/src/peer/mod.rs`

```
Hello {
  protocol_version: uint32 = 1         // 3
  min_protocol_version: uint32 = 2     // 3
  client_version: string = 4           // 发送方程序版本
  features: map<string, uint32> = 5    // 特性名 → 版本（当前均为 1）
  profile: Profile = 8                 // optional，仅主控
}
Profile {
  color: uint32 = 2                    // 指针颜色 0xRRGGBB，0 由被控按加入顺序分配；显示名取自服务端或直连身份
}
```

主控在会话中修改光标颜色时，再发送一次带 `profile` 的 HELLO 请求；被控照常回复，并把该参与者的颜色改为 `profile.color`（0 恢复被控分配的颜色），随后的 PARTICIPANTS 与 CURSOR 使用新颜色。

特性名列表见 farwing-protocol.md §5.5。

## 错误回复 —— `crates/protocol/src/peer/mod.rs`

```
PeerError {
  code: string = 1      // unsupported | bad_request | forbidden | not_found | busy | unavailable | internal
  reason: string = 2    // 特性定义的原因键（可为空），接收方翻译；不含发送方语言的文字
}
```

## INPUT（0x0101，主控→被控）—— `src/protocol/input.rs`

```
InputMessage { oneof kind {
  Key key = 1 {
    usage: uint32 = 1             // HID usage (page << 16) | id：键盘页 0x07，消费者页 0x0C
    down: bool = 2
    repeat: bool = 3              // 已按下键的自动重复
  }
  LockState lock_state = 2 {
    caps: bool = 1
    num: bool = 2
    scroll: bool = 3
    seq: uint32 = 4               // 此前已提交的锁定键按下次数（不含重复）
  }
  MouseAbsolute mouse_absolute = 3 {
    screen_id: optional int32 = 1
    x: double = 2                 // 屏内归一化
    y: double = 3
  }
  MouseRelative mouse_relative = 4 {
    dx: sint32 = 1                // 被控像素
    dy: sint32 = 2
  }
  MouseButton mouse_button = 5 {
    button: uint32 = 1            // 1 左，2 右，16 中，32 X1，64 X2
    down: bool = 2
  }
  MouseWheel mouse_wheel = 6 {
    dx: sint32 = 1                // 1/120 格，一条消息同时携带两个轴
    dy: sint32 = 2
  }
  SystemCommand system_command = 7   // enum，见下
  Heartbeat heartbeat = 8 {}         // 有键按下时的保活
}}

enum SystemCommand { UNSPECIFIED = 0; SAS = 1 /* Ctrl+Alt+Del，需要被控服务 */; LOCK = 2; TASK_MANAGER = 3; SHOW_DESKTOP = 4; TASK_VIEW = 5 }
```

- 主控按物理键的 set-1 扫描码（E0 扩展键记为 `0xE0xx`）查表得到 usage；被控有 HID 驱动时直接上报 usage，否则按扫描码注入。映射表在 `src/platform/windows/input/keyboard.rs`。不认识的 usage 被忽略。
- `LockState` 的纠正规则见 v2-design §7；保活与按键释放超时见 farwing-protocol.md §5.8。

## CURSOR（0x0103，被控→主控）—— `src/features/remote_cursor.rs`、`src/protocol/participants.rs`

```
CursorMessage {
  shape: CursorShape = 1                          // optional；被控光标形状
  participants: repeated ParticipantPointer = 2   // 其他参与者的指针
  pointers: bool = 3                              // true 表示参与者指针更新（列表可为空，用于清除）
}
CursorShape {
  pos_x: int32 = 1                // 热点（图像像素）
  pos_y: int32 = 2
  width: int32 = 3                // ≤ 2048
  height: int32 = 4
  byte_value: bytes = 5           // PNG
  cursor_type: int32 = 6          // 系统光标类型；-1 表示隐藏（无图像）
  coordinate_x_scale: double = 7  // 形状变化时采样的屏内归一化位置（不是实时位置）
  coordinate_y_scale: double = 8
  screen_id: int32 = 9
}
ParticipantPointer {
  id: uint64 = 1                  // 参与者 ID（同 PARTICIPANTS）
  screen_id: int32 = 2
  x: float = 3                    // 屏内归一化
  y: float = 4
  visible: bool = 5
  idle_ms: uint32 = 6             // 距上次移动；接收方 3 秒后淡出
  click: Click = 7                // optional；最近一次点击，短时间内重复发送
  click_seq: uint32 = 8           // 新的值表示新的一次点击
}
```

`pointers = false` 时按 `shape` 处理；整帧不超过 512 KiB。

## POINTER（0x0106，主控→被控）—— `src/protocol/participants.rs`

```
Pointer {
  seq: uint32 = 1        // 每帧递增
  screen_id: int32 = 2
  x: float = 3           // 屏内归一化，必须在 0..1
  y: float = 4
  visible: bool = 5      // 指针离开远程画面后为 false
  click: Click = 6       // optional
}
Click {
  button: int32 = 1      // 1 左，2 右，3 中
  phase: int32 = 2       // 1 按下，2 抬起
}
```

观看者也发送，作为指示光标，不注入输入。

## SESSION（0x0201）—— `src/features/stream_control/wire.rs`

另见 `src/features/stream_control/display_topology.rs`（显示请求）、`src/features/stream_control/microphone.rs`（麦克风请求）、`src/features/stream_control/control_turn.rs`（控制权）。

```
SessionMessage { oneof body {
  // 推送（request id = 0）
  PbScreenSources screens = 1                       // H→C
  PbReportQosStats quality = 2                      // H→C
  MicrophoneState microphone = 3                    // H→C
  ClipboardPermission clipboard_files = 4           // H→C
  CaptureControl capture = 5                        // C→H
  ReceiverStats receiver_stats = 7                  // C→H，每秒一次
  VideoFeedback video_feedback = 8                  // C→H，control 通道（FCT），`video.recovery`
  // 请求（C→H）
  PbCaptureSettingRequest capture_setting = 10      // → 20
  PbCreateVirtualDisplay create_virtual_display = 11 // → 21
  PbRemoveVirtualDisplay remove_virtual_display = 12 // → 21
  PbEnterSuperScreen enter_exclusive = 13           // → 21
  PbQuitSuperScreen quit_exclusive = 14             // → 21
  PbSendVideoTrackRequest video_tracks = 15         // → 21
  PolicyRequest microphone_policy = 16              // → 22
  TurnRequest control_turn = 17                     // → 23
  // 响应（H→C）
  PbCaptureSettingResponse capture_setting_result = 20
  PbDisplayResult display_result = 21
  PolicyResponse microphone_result = 22
  TurnResponse control_turn_result = 23
}}
```

被控收到不认识的请求回 `RESPONSE|ERROR` + `PeerError{unsupported}`。

### 推送

```
PbScreenSources {
  screens: repeated PbScreen = 1
  current_screen_id: int32 = 2
}
PbScreen {
  id: int32 = 1
  fps: int32 = 2                          // 刷新率
  resolutions: repeated PbWinRect = 3     // 可选模式
  current_resolution: PbWinRect = 4
  screen_type: int32 = 5                  // 0 物理屏，其他为虚拟屏 / 独占屏等
  init_resolution: PbWinRect = 6          // 连接时的快照，不代表面板模式
  is_primary_screen: bool = 7
  dpi_scale: PbDpiScale = 9
  display_name: string = 10
  resolution_type: int32 = 11
  video_track_index: int32 = 12           // 对应 answer 中的 msid video_N
  display_geometry: PbDisplayGeometry = 100  // optional；驱动首选（面板原生）模式
}                                         // 8（dpr）与 13（builtin_screen_type）已删除
PbWinRect { left: int32 = 1, top: int32 = 2, width: int32 = 3, height: int32 = 4 }
PbDpiScale { current_dpi: int32 = 1, recommended_dpi: int32 = 2, dpis: repeated int32 = 3 }
PbDisplayGeometry {
  width: uint32 = 2       // 已按当前旋转方向调整，1–16384
  height: uint32 = 3
}

PbReportQosStats {
  encoder_type: string = 1   // software | hardware
  capture_type: string = 2   // 采集后端名
  video_quality: int32 = 4   // 生效档位 1..4（自动已解析）或 6 自定义
}
MicrophoneState { state: int32 = 1 }       // 1 ACTIVE（正在播放主控麦克风），2 INACTIVE，3 RESTART（被控音频链路出错，主控重启本地采集）
ClipboardPermission { enabled: bool = 1 }  // 被控是否接收剪贴板文件
CaptureControl {
  screen_id: int32 = 1       // -1 表示本会话全部屏幕（停止保留槽位，start(-1) 恢复）
  start: bool = 2            // 开始 / 停止采集；无回复，状态以随后的 Screens 为准
}
VideoFeedback {                // 设计见 rfi-ltr-brief.md §6.3，协议规则见 farwing-protocol.md §6.6
  ssrc: fixed32 = 1            // 视频轨的媒体 SSRC；被控按它找到轨道
  epoch: uint32 = 2            // frame-meta 纪元（0–255）
  decoded: uint32 = 3          // 本纪元连续解码链的最新帧号
  ltr: repeated uint32 = 4     // packed；已解码且仍持有的长期参考帧号，新的在前，最多 4 个
  recover: bool = 5            // true：decoded 之后有修不好的丢包，请求恢复帧
  request: uint32 = 6          // 恢复请求号，每个丢包事件加 1；确认时为 0
  rtp_timestamp: optional fixed32 = 7  // 浏览器：最新解码帧的 RTP 时间戳（2–6 不填）
}
// 被控不回复：恢复帧（frame-meta 置 RECOVERY|REF_LTR）或关键帧就是回复。
// 未声明特性、SSRC 不认识的消息丢弃，不回错误。
// 带 rtp_timestamp 的确认来自浏览器（不声明 video.recovery），只在该视频轨协商了
// Dependency Descriptor 时使用；被控按自己发送过的帧推出该帧依赖链上的长期参考。

ReceiverStats {
  receive_fps: float = 1
  decode_fps: float = 2
  render_fps: float = 3
  decode_delay_ms: float = 4   // 到达到解码完成
  dropped_frames: uint32 = 5   // 自上次报告以来
}
```

- `display_geometry` 只在主控声明 `display.geometry` 时发送；主控只用它做本地显示比例，不据此修改远端分辨率。
- 被控在屏幕列表或任一屏幕变化时发送 `Screens`，内容相同不重发。

### 请求与响应

```
PbCaptureSettingRequest {
  fps: int32 = 1                    // 1:30 2:60 3:90 4:144
  frame_quality: int32 = 2          // 1 低码率 2 高清 3 超清 4 原画 5 自动 6 自定义
  cursor_capture: bool = 3          // 画面合成光标
  screen_id: int32 = 4              // -2 本会话所有正在采集或暂停的轨道；-1 单轨道会话的唯一来源；其他为具体屏幕
  resolution_width: int32 = 5
  resolution_height: int32 = 6
  chroma_format: int32 = 7          // 1 4:2:0，3 4:4:4
  max_custom_bitrate: int32 = 8     // bit/s
  dpi_scale: int32 = 9
  resolution_type: int32 = 10       // 0–4
  enable_hdr: bool = 11
  auto_frame_quality: int32 = 12    // 自动画质的当前档位
  codec_type: int32 = 13            // 1 H.264，2 H.265，5 AV1
  max_scale_width: int32 = 14       // 主控的缩放上限
  max_scale_height: int32 = 15
  fps_count: int32 = 18             // 主控显示刷新率上限
  weak_network: int32 = 19          // 0 优先降码率，1 优先降帧率
  cinema: bool = 20                 // 观影模式
}
PbCaptureSettingResponse { errors: repeated PbError = 1 }   // 每个屏幕一条；全部为空时补一条 0
PbError { error_code: int32 = 1, error_message: string = 2, error_detail: string = 3 }

PbCreateVirtualDisplay {
  local_resolution: repeated PbScreenResolution = 1
}
PbScreenResolution { width: int32 = 1, height: int32 = 2 }
PbRemoveVirtualDisplay { screen_id: repeated int32 = 1 }   // 取第一个；只能删除 Farwing 创建的虚拟屏
PbEnterSuperScreen {
  reason: int32 = 1                  // 1 手动，2 提高帧率
  resolution: PbScreenResolution = 2
  dpi_scale: int32 = 3
  enter_fps: PbSuperScreenFps = 4    // reason = 2 时必需
}
PbSuperScreenFps { fps_count: int32 = 1 }   // 1–144
PbQuitSuperScreen {}
PbSendVideoTrackRequest { video_track_index: repeated int32 = 1 }   // 主控已协商可接收的视频轨道（0..4）；采集新屏幕前必须先确认
PbDisplayResult { error_code: int32 = 1 }

PolicyRequest { policy: int32 = 1 }        // bit0：开启麦克风
PolicyResponse { error_code: int32 = 1 }

TurnRequest {
  take: bool = 1                           // true 获取控制权，false 释放
  automatic: bool = 2                      // 自动恢复上次的控制状态，不是用户操作
  ask: bool = 3                            // 没有 control 授权时，请被控端用户允许本次连接控制
}
TurnResponse {
  held: bool = 1                           // 回复时本参与者是否持有控制权
  refusal: int32 = 3                       // 拒绝原因：1 本次连接无法控制，2 未授权 control，
                                           // 3 他人持有，4 多人时不自动获取，5 被控端用户未同意
  holder: string = 4                       // refusal = 3 时的持有者
}
```

**ControlTurn**：主控开启键鼠控制时发送 `take = true`，关闭时发送 `take = false`。无人持有且本参与者拥有 `control` 能力时获得；他人持有或没有 `control` 能力时 `held = false`，主控关闭键鼠控制并按 `refusal` 提示。因未授权（`refusal = 2`）被拒时，提示里可以“请求本次控制”：主控再发 `take = true, ask = true`，被控端弹出“请求键鼠控制 · 仅本次连接”，用户允许后本参与者在本次连接内拥有 `control`（`PERMISSIONS` 随之包含 `control`，主人收回控制权时失效，不写入受信任设备），回复 `held = true`；拒绝或 60 秒无人回应时 `refusal = 5`。等待期间控制通道照常工作。`automatic = true` 时，只有本参与者已经持有、或会话中只有它一人才会获得；否则 `held = false`，主控静默回到仅观看。获取请求发不出去或被控回 `ERROR` 时，主控同样关闭键鼠控制（提示“未能向被控端请求键鼠控制权”，`automatic` 时不提示）。释放总是成功。控制权的其他变化（持有者离开、失去 `control` 能力）只经 `PARTICIPANTS.controlling` 通知。

**CaptureSetting 结果码**（`PbError.error_code`）

| 码 | 含义 |
|----|------|
| 0 | 成功 |
| -1 | 无效或不支持；需要 `display` 能力的显示变更被拒绝 |
| -2 | 正在更新，或系统执行失败 |
| -3 | 请求帧率超过屏幕刷新率（提示，不算失败） |
| -5 | 分辨率转为独占屏的结果，`error_detail = {"error_code": 0 \| 501}` |
| -6 | 全彩（4:4:4）开关结果，`error_detail = {"error_code": 0 已开启 \| 1 编码器不支持 \| 2 支持但未生效 \| 5 已关闭}` |

带分辨率、DPI 或 `resolution_type` 2..4 的请求是显示变更，需要 `display` 能力；`screen_id = -2` 时不能带分辨率或 DPI。`resolution_type = 3` 恢复被控连接时的分辨率；物理屏不支持所请求的尺寸且 `resolution_type = 2` 时转为独占屏（结果 -5）。

**DisplayResult 结果码**

| 请求 | 码 |
|------|----|
| `CreateVirtualDisplay` | 0 成功；-2 没有空闲且已注册的视频轨道；-1 其他失败（需要 `display` 能力） |
| `RemoveVirtualDisplay`、`QuitExclusive` | 0 / -1（需要 `display` 能力） |
| `EnterExclusive` | 0；502 参数无效；501 能力或策略不允许、系统失败 |
| `VideoTracks` | 0 / -1 |

**MicrophoneResult 错误码**（`src/protocol/microphone.rs`）：0 成功；`0x0e003001` 虚拟音频组件不可用；`0x0e003006` 不允许（需要 `microphone` 能力与被控本机许可）；`0x0e003012` 初始化失败。

## AUDIO（0x0202，JSON）—— `src/protocol/audio_control.rs`

```json
{"kind": "hello",    "revision": 3, "kbps": 128, "effective_bps": 128000}          // H→C：当前桌面声音码率
{"kind": "set",      "request": 7, "kbps": 192}                                     // C→H：请求修改
{"kind": "applied",  "request": 7, "revision": 4, "kbps": 192, "effective_bps": 192000}  // H→C
{"kind": "rejected", "request": 7, "message": "…"}                                 // H→C
```

帧体不超过 2048 字节；`request` 关联请求，帧头 `request id` 为 0。

## HOST_EVENT（0x0203，被控→主控）—— `src/protocol/host_event.rs`

```
HostEvent {
  kind: int32 = 1                    // 1 UPDATING，2 RESTARTING，3 SLEEPING，4 SHUTTING_DOWN，5 SESSION_SWITCH，6 DISPLAY_CHANGING
  actor: string = 3                  // DISPLAY_CHANGING：修改者的昵称（可为空）；文字由接收方按自己的语言生成
}
```

## CLIPBOARD（0x0301）—— `src/features/clipboard/wire.rs`

```
ClipboardMessage { oneof body {
  Offer offer = 1          // 推送；也是 Pull 的响应
  Fetch fetch = 2          // 请求 → Data
  Data data = 3            // 响应
  ListFiles list_files = 4 // 请求 → Files（可分片）
  Files files = 5          // 响应
  ReadFile read_file = 6   // 请求 → Data
  Release release = 7      // 推送
  Abort abort = 8          // 推送
  Pull pull = 9            // 请求（C→H）→ Offer
}}
Offer {
  offer: uint32 = 1                // 发送方递增的编号
  formats: repeated Format = 2     // 为空表示清空
}
Format {
  id: uint32 = 1                   // Windows 剪贴板格式号
  name: string = 2                 // 注册格式的名称，接收方按名称在本机注册
}
Fetch {
  offer: uint32 = 1
  format: Format = 2               // optional
  stream: uint32 = 3               // 请求方登记的流，内容超过 32 KiB 时使用
}
Data {
  size: uint64 = 1
  inline: bytes = 2                // size ≤ 32 KiB 时为内容，否则为空、内容走 TRANSFER
}
ListFiles { offer: uint32 = 1 }
Files { items: repeated FileItem = 1 }
FileItem {
  path: string = 1                 // `\` 分隔的相对路径，含顶层名称
  attributes: uint32 = 2           // Windows 文件属性（0x10 目录）
  mtime: uint64 = 3                // FILETIME（100 ns，1601 起）
  size: uint64 = 4                 // 目录为 0
}
ReadFile {
  offer: uint32 = 1
  index: uint32 = 2                // 文件清单下标
  offset: uint64 = 3
  length: uint32 = 4               // ≤ 1 MiB
  stream: uint32 = 5
}
Release { offer: uint32 = 1 }      // 接收方不再读取该 offer 的文件
Abort { stream: uint32 = 1 }       // 请求方放弃该流，发送方停止
Pull {}
```

- 文件以 `FileGroupDescriptorW` + `FileContents` 格式表示；接收方校验名称安全（拒绝 `..`、设备名、非法字符）。
- 大内容：`TRANSFER{kind: CLIPBOARD, stream: 请求中的 stream, entry: 0}`，最后一块置 `last`；单个格式最多 128 MiB；文件清单最多 100000 项，重组后的消息最多 48 MiB。
- 失败码：`not_found`（offer 已被替换、格式未提供、内容为空）、`forbidden`（文件剪贴板关闭、来源标记为不同步、对主控发 `Pull`）、`unavailable`（同步暂停或未登录桌面）、`bad_request`、`internal`。

## FILE（0x0401）—— `src/features/file_transfer/wire.rs`

```
FileMessage { oneof body {
  // 请求（C→H）
  List list = 1                    // → Listing
  MakeDir make_dir = 2             // → Done
  Rename rename = 3                // → Done
  Remove remove = 4                // → Done
  Exists exists = 5                // → Existing
  Upload upload = 6                // → Plan
  UploadStart upload_start = 7     // → Done
  Download download = 8            // → Manifest
  DownloadStart download_start = 9 // → Accepted
  Cancel cancel = 10               // → Done
  // 响应（H→C）
  Listing listing = 20
  Done done = 21
  Existing existing = 22
  Plan plan = 23
  Manifest manifest = 24
  Accepted accepted = 25
  Failed failed = 26
  // 推送（H→C）
  ItemDone item_done = 30
}}

List { path: string = 1 }                 // 绝对目录；空表示驱动器与常用位置
Listing {
  path: string = 1                        // 被控解析后的目录；根列表为空
  entries: repeated Entry = 2
}
Entry {
  name: string = 1
  kind: int32 = 2                         // 1 FILE，2 DIR，3 DRIVE，4 SHORTCUT
  size: uint64 = 3
  mtime_ns: uint64 = 4
  hidden: bool = 5
  path: string = 6                        // 仅根列表：驱动器或常用位置的绝对路径
  icon: string = 7                        // 仅根列表：常用位置图标
}
MakeDir { parent: string = 1, name: string = 2 }   // 名称已存在时 EXISTS
Rename { path: string = 1, name: string = 2 }      // name 为同目录下的新名称
Remove { path: string = 1, recursive: bool = 2 }
Done { path: string = 1 }
Exists { dir: string = 1, names: repeated string = 2 }
Existing {
  present: repeated string = 1
  writing: repeated string = 2            // 另一个传输正在写入的名称
}
Item {
  path: string = 1                        // `/` 分隔，相对传输根
  size: uint64 = 2
  mtime_ns: uint64 = 3
  dir: bool = 4                           // 空目录：无数据，以一个空的 last 块表示
}
Upload {
  transfer: uint32 = 1                    // 流号（TRANSFER.stream）
  dir: string = 2                         // 被控上的绝对目标目录
  folder: string = 3                      // 在 dir 下创建的顶层文件夹；单个文件时为空
  items: repeated Item = 4
  conflict: int32 = 5                     // 1 OVERWRITE，2 RENAME，3 SKIP
  key: string = 6                         // 主控的任务键：命名被控的部分文件与续传记录
}
ItemPlan {
  offset: uint64 = 1                      // 接收方已有的字节数
  tail_sha256: bytes = 2                  // offset 之前末尾（≤ 1 MiB）的 SHA-256
  skip: bool = 3                          // 已保存或按 SKIP 策略跳过
}
Plan { transfer: uint32 = 1, items: repeated ItemPlan = 2 }
UploadStart {
  transfer: uint32 = 1
  offsets: repeated uint64 = 2            // 主控核对 tail 后的起点（计划值或 0）
}
Download {
  transfer: uint32 = 1
  paths: repeated string = 2              // 同一父目录下的被控绝对路径
  key: string = 3
}
Manifest { transfer: uint32 = 1, folder: string = 2, items: repeated Item = 3 }
DownloadStart { transfer: uint32 = 1, items: repeated ItemPlan = 2 }
Accepted { offsets: repeated uint64 = 1 } // 被控核对 tail 后的最终起点
Cancel {
  transfer: uint32 = 1                    // 要停止的传输；没有时为 0
  key: string = 2
  discard: bool = 3                       // 删除 key 的部分文件与记录（默认保留以便续传）
}
ItemDone {
  transfer: uint32 = 1
  entry: uint32 = 2                       // 清单下标；0xFFFFFFFF 表示整个传输失败
  error_code: int32 = 3                   // 0 已落盘，其他同 Failed.code
  message: string = 4
}
Failed { code: int32 = 1, message: string = 2 }
```

- `Failed.code` / `ItemDone.error_code`：0 OK，1 PROTOCOL，2 DENIED，3 NOT_FOUND，4 EXISTS，5 NO_SPACE，6 IO，7 BUSY，8 CHANGED，9 CANCELLED。主控按代码措辞；`message` 只在 PROTOCOL 与 IO 时带被控的原因，其余为空。带 ERROR 标志的 `PeerError` 回复按 `Failed{PROTOCOL}` 处理。
- 帧体超过 64 KiB − 10 时分片（farwing-protocol.md §5.2），重组后最多 32 MiB；推送不分片。
- 清单包含文件与空目录；未 `skip` 的条目按清单顺序以 `TRANSFER{kind: FILE, stream: transfer, entry: 下标}` 发送。部分文件写在目标目录的隐藏临时文件 `.{名称}.{key}.downloading`，完成后按冲突策略改名并设置 mtime。

## TRANSFER（0x0402）与 FLOW（0x0A01）—— `src/transport/transfer.rs`

```
Chunk {                     // TRANSFER 帧体
  stream: uint32 = 1
  kind: int32 = 2           // 1 FILE，2 CLIPBOARD，3 DRAG，4 DIAGNOSTICS
  offset: uint64 = 3        // 在当前条目内的位置，必须连续
  data: bytes = 4           // ≤ 64 KiB − 64
  last: bool = 5            // 当前条目结束
  entry: uint32 = 6         // 条目在清单中的下标
}
Flow {                      // FLOW 帧体
  channel: int32 = 1        // 1 BULK，2 TUNNEL
  stream: uint32 = 2
  credit: uint64 = 3        // 累计允许发送的字节总数（只增不减）
  kind: int32 = 4           // BULK 时为流的 kind；TUNNEL 时为 0
}
```

信用规则见 farwing-protocol.md §5.7。

## DRAG（0x0501）—— `src/protocol/drag_drop.rs`

```
Packet {
  token: fixed64 = 1          // 本连接的拖放标识，非 0
  drag: fixed64 = 2           // 拖放任务标识；Hello/HelloAck/Available 时为 0，其他非 0
  sequence: uint64 = 3        // 任务内序号，非 0（握手消息除外）
  oneof payload {
    uint32 hello = 4          // 被控 → 主控，原生拖放版本（1）
    uint32 hello_ack = 5      // 主控 → 被控（1）
    Begin begin = 6           // 开始一次拖放
    Point position = 7        // 拖动中的位置
    Point commit = 8          // 在该点放下
    bool cancel = 9           // 取消
    Feedback feedback = 11    // 落点方的反馈
    Finished finished = 12    // 结果
    Point probe = 13          // 探测落点
    uint64 progress = 14      // 已读取的字节数
    bool source_released = 15 // 拖出方已释放
    bool available = 16       // 被控 → 主控：当前是否接受拖放
    DragImage image = 20      // 拖动图像
    Point resume = 21         // 拖回主控：在该点继续
    bool resumed = 22         // 拖回主控：是否已接管
    bool handoff_done = 23    // 拖回主控：交接完成
    bytes files = 24          // 一个完整的 CLIPBOARD 帧（本次拖放的文件交换）
  }
}
Point {
  screen: int32 = 1           // ≥ 0
  x: double = 2               // 屏内归一化
  y: double = 3
}
Begin {
  point: Point = 1            // 必需
  reverse: bool = 2
  preserve: bool = 3
}
Feedback {
  ready: bool = 1
  effect: uint32 = 2          // 只允许 1（复制）或 0
}
Finished {
  stage: uint32 = 1           // 1 已放下，2 读取完成，3 已取消，4 失败
  message: string = 2         // ≤ 240 字符
  bytes_read: uint64 = 3
}
DragImage {
  width: uint32 = 1           // 1–120；无图像时宽高为 0、像素为空
  height: uint32 = 2
  pixels: bytes = 3           // 自上而下的 BGRA（直通 alpha），width × height × 4 字节
}
```

单个 DRAG 帧不超过 64 KiB。`files` 中的 CLIPBOARD 帧只用于本次拖放的文件交换，数据为 `TRANSFER{kind: DRAG}`（farwing-protocol.md §5.8）。v1 的 `data`（tag 10）已删除。

## ANNOTATION（0x0601）—— `src/protocol/annotation.rs`、`src/features/stream_control/annotation/wire.rs`

```
Packet {
  token: fixed64 = 1        // 被控为本连接生成，非 0
  request: int64 = 2        // 主控递增的请求号；Result 回写同一值
  oneof payload {
    uint32 hello = 3        // 被控 → 主控：批注版本（1）
    Command command = 4     // 主控 → 被控
    int32 result = 5        // 被控 → 主控：0 已执行，2 被控未登录，3 被控已锁定，4 拒绝
  }
}
Command { oneof operation {
  PbDrawRequest draw = 1
  Object replace = 2        // 替换一笔
  Batch batch = 3
  Board board = 4           // 白板
  bool clear = 5            // false 清除笔迹；true 同时清除白板
}}
Batch { commands: repeated Command = 1 }
Object { stroke: PbDrawStroke = 1 }
Board { screen: int32 = 1, color: optional fixed32 = 2 }

PbDrawRequest { oneof payload {
  PbDrawStroke stroke = 1
  PbDrawClear clear = 2
  PbDrawToggle toggle = 3
}}
PbDrawPoint { x: float = 1, y: float = 2 }   // 屏内归一化
PbDrawStroke {
  stroke_id: uint32 = 1                 // 非 0；同一笔可分多次追加点
  points: repeated PbDrawPoint = 2      // 1–4096 个
  screen_id: optional int32 = 3         // 缺省为当前屏幕
  line_width: optional float = 4        // (0, 64]，缺省 3
  color: optional uint32 = 5            // 0xAARRGGBB，缺省 0xFFFF4444
}
PbDrawClear {
  clear_type: int32 = 1                 // 1 全部，2 指定笔
  stroke_id: uint32 = 2                 // clear_type = 2 时非 0
  screen_id: optional int32 = 3
}
PbDrawToggle { enable: bool = 1 }       // 开启 / 关闭批注（关闭时清空）
```

单帧不超过 512 KiB。需要 `annotation` 能力。

## PORT（0x0701）与 PORT_DGRAM（0x0702）—— `src/features/port_mapping/wire.rs`

```
PortMessage { oneof body {
  Open open = 1                  // C→H：连接 host:port
  OpenResult open_result = 2     // H→C
  Data data = 3                  // 双向，TCP 数据
  uint32 shutdown_write = 4      // 双向：该 stream 本方向不再发送数据（TCP 半关闭）
  Reset reset = 5                // 双向：中止该 stream
}}
Open {
  stream: uint32 = 1             // 主控分配，非 0
  proto: int32 = 2               // 1 TCP，2 UDP
  host: string = 3               // IP 或主机名（≤ 253 字符），在被控侧解析
  port: uint32 = 4               // 1–65535
}
OpenResult { stream: uint32 = 1, code: int32 = 2, message: string = 3 }
Data {
  stream: uint32 = 1
  data: bytes = 2                // ≤ 64 KiB − 64
}
Reset { stream: uint32 = 1, code: int32 = 2, message: string = 3 }

Datagram {                       // PORT_DGRAM 帧体
  stream: uint32 = 1             // 非 0
  data: bytes = 2                // ≤ 65507
}
```

`code`（`OpenResult` 与 `Reset` 共用）：0 OK，1 REFUSED，2 UNREACHABLE，3 FORBIDDEN，4 TIMEOUT，5 DNS_FAILED，6 LIMIT（连接数已达上限，被控每会话 64 个），7 PROTOCOL，8 CLOSED（正常结束）。TCP 数据按 `FLOW{channel: TUNNEL, stream}` 的信用发送。

## DIAGNOSTICS（0x0801）—— `src/diagnostics/remote/mod.rs`

```
DiagnosticsMessage { oneof body {
  Status status = 1        // 推送 H→C
  Request request = 2      // 请求 C→H → Ready 或 RESPONSE|ERROR
  Progress progress = 3    // 推送 H→C，约 200 ms 一次
  Ready ready = 4          // Request 的响应
  Cancel cancel = 5        // 推送 C→H
  Failed failed = 6        // 推送 H→C：Ready 之后发送中断
}}
Status { allowed: bool = 1 }          // 本参与者能否获取诊断包
Request { stream: uint32 = 1 }        // 主控已登记的接收流
Progress {
  stream: uint32 = 1
  stage: string = 2
  completed: uint64 = 3
  total: uint64 = 4
}
Ready {
  size: uint64 = 1
  sha256: string = 2                  // 小写十六进制
  logs: uint32 = 3
  warnings: uint32 = 4
}
Cancel { stream: uint32 = 1 }
Failed { stream: uint32 = 1 }
```

`Request` 的错误回复（`message` 为空，主控按代码措辞）：`unsupported`（被控不提供诊断包）、`forbidden`（本参与者无权获取）、`busy`（正在生成）、`internal`。诊断包内容为 `TRANSFER{kind: DIAGNOSTICS, stream}`；主控无响应 30 秒即放弃。

## PERMISSIONS（0x0901，被控→主控）—— `src/protocol/host_event.rs`

```
Permissions { grants: repeated string = 1 }   // 能力名（farwing_protocol::caps），未知名称被丢弃
```

## PARTICIPANTS（0x0902，被控→主控）—— `src/protocol/participants.rs`

```
Participants {
  participants: repeated Participant = 1   // ≤ 16
  controlling: uint64 = 2                  // 控制权持有者（被控只注入其键鼠）；0 表示无人
}
Participant {
  id: uint64 = 1                // 在该被控上的会话期间稳定
  nickname: string = 2          // ≤ 64 字符（接收方截断，下同）
  device_name: string = 3
  relation: string = 4          // own | friend | assist | trusted
  color: fixed32 = 5            // 指针与标签颜色
  control: bool = 6             // 拥有 control 能力
  you: bool = 7                 // 接收方本人
  joined_ms: int64 = 8          // 加入时间，Unix 毫秒
}
```

## TOUCH（0x0104）与 PEN（0x0105）—— `src/protocol/pointer.rs`

control 通道，主控 → 被控推送（rid 0），与 INPUT 同一门控（持有控制权、`control` 授权）、同一队列与几何快照。设计与字节示例见 [touch-pen-brief.md](touch-pen-brief.md) §5。

```
Touch {
  screen_id: optional int32 = 1   // 必写，屏 0 也写
  contacts: repeated Contact = 2  // 本参与者此刻的全部活动触点与本帧结束的触点，0–10 个；空表 = 取消全部
  time_us: uint32 = 3             // 主控单调时钟（µs，回绕），只用于诊断
}
Contact {
  id: uint32 = 1                  // 1..=255，帧内唯一；UP/CANCEL 后可复用
  phase: enum = 2                 // 1 DOWN，2 MOVE，3 UP，4 CANCEL
  x: float = 3, y: float = 4      // 屏内归一化 [0,1]
  pressure: float = 5             // (0,1]，0 = 未报告
  width: float = 6, height: float = 7   // 按屏宽高归一化，0 = 未报告
  orientation: uint32 = 8         // 0..359
}
Pen {
  screen_id: optional int32 = 1   // 必写
  samples: repeated PenSample = 2 // 1–16 个，旧的在前
  time_us: uint32 = 3
}
PenSample {
  phase: enum = 1                 // 1 HOVER，2 DOWN，3 MOVE，4 UP，5 LEAVE，6 CANCEL
  x: float = 2, y: float = 3
  pressure: float = 4             // [0,1]
  tilt_x: sint32 = 5, tilt_y: sint32 = 6   // −90..=90 度
  twist: uint32 = 7               // 0..359
  buttons: uint32 = 8             // bit0 笔杆键，bit1 橡皮擦端
  age_us: uint32 = 9              // 早于 time_us 的微秒数
}
```

- 被控规则：本帧没有列出的已知触点按 CANCEL；UP 的位置与上一帧不同时先补一帧 MOVE；超过 10 个的触点在它结束前一直忽略；静止的触点每 100 ms 重注入；持有者超过 `max(150 ms, 3×RTT + 50 ms)` 没有任何输入帧就取消全部触点、笔离开（不看键盘中断设置）；解析或校验失败的 TOUCH/PEN 帧按“取消全部”处理。
- 被控用 Windows 合成指针注入（`CreateSyntheticPointerDevice` + `InjectSyntheticPointerInput`，运行时加载）；合成指针不可用、或落点是发起控制的观看窗口（本机控制本机）时改为鼠标：第一个触点 = 左键，笔尖 = 左键，笔杆键 = 右键。
- 被控光标因触摸进入 `CURSOR_SUPPRESSED` 时仍报告上一个形状，不报隐藏（否则原生的智能模式会误切到相对鼠标）。
- 主控：有活动触点或笔接触时与按键一样每 50 ms 心跳；失焦、关闭控制、交出控制权时发空 TOUCH 帧与笔 CANCEL；纯移动的触摸帧在通道积压时只保留最新一帧。
