# 网页主控（键鼠、剪贴板、文件、麦克风）实现简报

本文是浏览器从“只看”升级为“控制”的实现依据。浏览器端为 TypeScript，在 `server/web` 中实现。目标是让浏览器按会话授予的能力，做到和原生主控一样的事：

- 键鼠控制，包括触屏模拟鼠标、全屏 Keyboard Lock 和特殊键菜单；
- 文本剪贴板；
- 上传、下载文件；
- 把浏览器麦克风送进被控的虚拟麦克风。

> 实施结果（2026-10-17）：按本文实现，见 [web-console.md](../web-console.md) §7.1 与 §7.7；与本文的出入：被控光标隐藏时网页显示普通箭头（§2.5 建议 `cursor: none`，没有鼠标的被控会因此无法瞄准）；`caps_limit` 按 §7.2 实现，服务端设置名为 `web_caps`。2026-10-19 起不再兼容旧版本：`Platform::controller_caps_limit()` 回退已删除，服务端对网页主控总是发 `caps_limit`，被控完全按它执行（§7.2 第 4 条）。

连接、`session.start`、签名 offer、HELLO、FPP 帧头和 protobuf 基础见 [web-viewer-brief.md](web-viewer-brief.md)（下称“观看简报”），本文不再重复。

- 所有结论都来自代码阅读，`文件:行` 对应 2026-10-17 的代码快照。代码与 `docs/protocol/*.md` 不一致时以代码为准，差异汇总在 §9。
- 用语：被控 = host；主控 = controller = 浏览器；“原生”指 Windows 客户端主控。
- 帧头：`46 57 02 <flags> <type u16 BE> <request_id u32 BE>`。flags 为 `0x01` RESPONSE、`0x02` ERROR、`0x04` MORE（`crates/protocol/src/peer/mod.rs:4-13`、`:35-40`、`:217-254`）。下文的字节示例中，“帧体”指帧头之后的部分。

---

## 0. 结论速览

### 0.1 现在为什么做不了（阻断项）

网页主控的能力在 4 处被硬裁剪为 `view` + `audio`：

1. 协议常量 `Platform::controller_caps_limit()`（`crates/protocol/src/lib.rs:156-163`）。
2. 服务端创建会话时（`server/src/http/sessions.rs:123-130`）。
3. 被控收到 `session.incoming` 时（`src/transport/signal/host.rs:759-766`）。
4. 访客“允许并信任”时（`src/features/host/assist.rs:265-274`、`:556`）。

而被控的每一项功能都按授权放行：

| 功能 | 需要的授权 | 位置 |
|---|---|---|
| 键鼠 | `control`，并持有控制权 | `src/features/host/input/mod.rs:322-328` |
| 剪贴板 | `clipboard` | `src/features/host/access.rs:1673-1701` |
| 文件 | `files` | `src/features/host/access.rs:1725-1730` |
| 麦克风 | `microphone` | `src/features/host/microphone.rs:264-269` |

所以在**改动 §7 之前，浏览器发什么都会被拒绝或丢弃**。改动需要同时升级服务端和被控：被控的协议 crate 里有自己的一份裁剪。

### 0.2 各功能一览（假定 §7 已改）

| 功能 | 授权 | 浏览器 HELLO 需要声明的特性 | 通道 | 帧 | 被控实现状态 |
|---|---|---|---|---|---|
| 获取/释放控制权 | `control` | 无 | **session** | SESSION 17/23 | 已实现 |
| 键盘、鼠标、系统命令 | `control` + 持有控制权 | 无（`input.*` 不检查） | **control** | INPUT 0x0101 | 已实现 |
| 触摸 | — | — | — | TOUCH 0x0104 | **未实现**，只有类型号（§6） |
| 剪贴板文本 | `clipboard` | **`clipboard: 1`**（必需） | session；大于 32 KiB 的数据走 bulk | CLIPBOARD 0x0301（+TRANSFER/FLOW） | 已实现 |
| 文件 | `files` | `files: 1`（被控不检查，建议声明） | FILE 走 session；数据走 **bulk**（必需） | FILE 0x0401、TRANSFER 0x0402、FLOW 0x0A01 | 已实现 |
| 麦克风 | `microphone` | 无（`audio.microphone` 被控不检查） | SESSION 走 session；音频走 RTP | SESSION 16/22/3 + Opus RTP | 已实现 |

浏览器建议的 HELLO 特性：在现有集合（`server/web/src/lib/fpp.ts:71-80`）上加 `clipboard`，可选加 `files`、`audio.microphone`。**不要**声明 `clipboard.files`、`display.config`、`drag`、`fct`。

### 0.3 浏览器现有代码需要补的基础能力

- `pb.ts` 的 `Writer` 目前不能写以下三类字段：
  - `sint32`（zigzag 编码）：`MouseRelative`、`MouseWheel` 要用。
  - `double`（wire type 1）：`MouseAbsolute.x/y` 要用。
  - 值为 0 但必须写出的字段：`Writer.int` 遇 0 跳过（`server/web/src/lib/pb.ts:21-28`），而 `MouseAbsolute.screen_id` 是 proto3 `optional`（`src/protocol/input.rs:70-71`），屏幕 0 也必须写出 `08 00`。
- `fpp.ts` 的类型表（`server/web/src/lib/fpp.ts:20-31`）要补 INPUT、CLIPBOARD、FILE、TRANSFER、FLOW。
- 要实现 MORE 分片的拼接：FILE 的 Listing/Manifest 可能超过 64 KiB，见 §4.2。
- `viewer.ts` 的音频收发器目前是 `recvonly`（`server/web/src/lib/viewer.ts:269`）。要发麦克风，必须在**首个 offer**里改为 `sendrecv`（§5.4）。

---

## 1. 控制权（ControlTurn）

### 1.1 消息与字节

SESSION（0x0201）的 oneof 中，tag 17 为 `TurnRequest`，tag 23 为 `TurnResponse`（`src/features/stream_control/wire.rs:39-95`、`src/features/stream_control/control_turn.rs:7-27`）。

```proto
message TurnRequest  { bool take = 1; bool automatic = 2; }   // C→H 请求，rid ≠ 0
message TurnResponse { bool held = 1; string message = 2; }   // H→C，RESPONSE，同一 rid
```

| 消息 | 字节 |
|---|---|
| 获取（rid 1） | `46 57 02 00 02 01 00 00 00 01` + `8a 01 02 08 01` |
| 自动恢复获取 | 帧体 `8a 01 04 08 01 10 01` |
| 释放 | 帧体 `8a 01 00`（空消息，oneof 仍要写出） |
| 成功回复 | `46 57 02 01 02 01 00 00 00 01` + `ba 01 02 08 01` |
| 拒绝回复 | 帧体 `ba 01 <len> 12 <len> <UTF-8 原因>` |

### 1.2 被控的处理（`src/features/host/peer/channels.rs:342-360`、`:680-704`）

- **只在 session 通道上处理**。被控只在 session 通道的分支里解析 ControlTurn（`channels.rs:305-313` 要求该通道是当前的 session 路由）。发在 control 通道上不会被处理。
- `request_id` 必须非 0，否则视为无效请求，记日志、不回复（`src/features/stream_control/publisher.rs:44-57`）。
- 决策（`channels.rs:680-704`，`src/features/host/participants.rs:337-383`）：

| 情况 | 结果 |
|---|---|
| 本连接没有参与者身份 | `held=false`，“本次连接不能控制被控端” |
| `take=false` | 释放，回 `held=false`、`message=""`。总是成功 |
| 没有 `control` 授权或许可已失效（`lease.requested()`） | `held=false`，“你没有这台设备的键鼠控制权限” |
| 他人持有 | `held=false`，“<昵称> 正在控制，对方释放控制权后才能开启键鼠控制”（`control_turn.rs:29-34`） |
| `automatic=true`，且会话中多于 1 人、自己不是持有者 | `held=false`，“多人连接时请手动开启键鼠控制” |
| 其他情况 | 获得控制权（重复获取是幂等的），`held=true` |

- 先到先得，没有抢占。持有者释放、离开（`participants.rs:136-150`）或失去 `control`，控制权才会释放。被控输入线程每 200 ms 核对一次持有者（`participants.rs:387-404`，`src/features/host/input/mod.rs:341-343`、`:402-405`）。“失去 `control`”包括主人在被控端“禁止控制”（`src/features/host/access.rs:1361-1372`）。
- 持有者变化时，被控先释放原持有者按住的键和按钮，再注入新持有者的输入（`src/features/host/input/mod.rs:374-385`）。

### 1.3 PARTICIPANTS 与控制权丢失

- `Participants.controlling` 是持有者的 `id`，0 表示无人。接收方自己那一项 `you=true`（`participants.rs:194-213`）。浏览器从 `you=true` 的条目取得自己的 id。
- 原生的做法：收到列表时，如果此前自己持有、现在不持有，并且没有尚未回复的 ControlTurn，就关闭键鼠控制，提示“被控端收回了本机的键鼠控制权”（`src/features/stream_control/handshake.rs:229-243`、`control_turn.rs:105-113`）。
- 列表里每人的 `control` 位表示他是否有 `control` 授权（`src/features/host/peer/mod.rs:639-644`）。他人持有时，原生直接禁用“开启控制”，不发请求（`src/features/stream_control/input.rs:118-122`，`src/features/participants.rs:126-130`）。

### 1.4 原生的时序（浏览器照做）

原生的前提：control 与 session 通道都已打开、HELLO 已完成（`pb_connected`）、已收到屏幕基线、编码已协商。见 `src/features/stream_control/mod.rs:927-943` 与 `control_turn.rs:45-47`。

1. **开启**（`src/features/stream_control/input.rs:102-148`）：
   1. 先在本地开启输入。
   2. 如果之前是观看状态，发 `take=true, automatic=false`。
   3. 收到 `held=false` 就关闭控制并提示 `message`。自动恢复的拒绝不提示（`control_turn.rs:68-93`）。
2. **关闭**：先在本地停止并释放按住的键，再发 `take=false`（`input.rs:107-112`）。
3. **重连后自动恢复**（`input.rs:152-205`）：只在上次开着控制、且自己独自在会话中或自己就是持有者时，发 `take=true, automatic=true`。
4. 被控在门口丢弃非持有者的 INPUT（`src/features/host/input/mod.rs:275-277`、`:322-328`）。原生不等回复就开始发输入，回复之前发出的输入会被静默丢弃。**浏览器建议收到 `held=true` 后再转发键鼠**。

### 1.5 多人时的其他影响

| 方面 | 规则 |
|---|---|
| 画面 | 非持有者看统一的多人观看流（合成光标、1080p、60 fps、观影模式）。持有者（或独自一人的参与者）保持自己的设置（`participants.rs:406-412`，观看简报 §4.11） |
| 剪贴板 | 多人时只有持有控制权的人自动同步；有 `clipboard` 授权的其他人只能手动 Pull（`access.rs:1673-1701`，§3.1） |
| 麦克风 | 与控制权无关：每个有 `microphone` 授权的人都可以同时说话（§5.3） |
| 文件 | 每位参与者各自一套引擎，与控制权无关（§4.1） |

### 1.6 指针模式

- 原生的鼠标模式（`src/features/remote_input.rs:55-61`）：观看、智能（默认，`src/features/viewing_settings.rs:86`）、本地（绝对）、远程（相对）。

  各模式下是否用相对坐标、是否让被控合成光标（`src/features/stream_control/input.rs:390-401`）：

  | 模式 | 相对坐标 | 被控合成光标（`cursor_capture`） |
  |---|---|---|
  | 观看 | 否 | 是 |
  | 本地 | 否 | 否（主控在本地画被控光标） |
  | 远程 | 是 | 是 |
  | 智能，被控光标隐藏（CURSOR `cursor_type == -1`，例如游戏） | 是 | 否 |
  | 智能，其他情况 | 否 | 否 |

- 光标合成通过 `capture_setting` 请求切换（`input.rs:207-259`，观看简报 §4.6 第 3 点；`server/web/src/lib/fpp.ts:233-251` 已实现）。

---

## 2. INPUT（0x0101）

### 2.1 通道、顺序与大小

- **只走 `farwing.control.v2`**，有序可靠：
  - 原生的输入发送任务绑定在 control 通道（`src/transport/rtc/channels.rs:84-88`、`:468-565`）。
  - 被控只把 control 通道上的帧交给输入模块（`src/features/host/peer/channels.rs:100-108`、`:242-249`，`src/features/host/peer/ingress.rs:39-49`）。
  - 发在 session 通道上会被当作 SESSION 解析并忽略（`src/features/stream_control/publisher/displays.rs:112-116`）。dgram 通道只用于端口转发（`channels.rs:75-81`）。
- 浏览器没有 FCT，INPUT 走 SCTP（观看简报 §4.2）。丢包时有队头阻塞，见 §8。
- 单帧大于 4 KiB 不按输入处理（`src/features/host/input/wire.rs:8`，`input/mod.rs:229-232`）。
- 原生的流控：每发一个非释放事件前，等 control 通道积压低于 `INPUT_SEND_BUDGET_BYTES`（1191−4−24−4 = 1159 字节，`src/transport/fct.rs:29-31`、`:50`），等待期间把连续的移动合并（`src/transport/rtc/channels.rs:500-513`，`src/features/remote_input.rs:1082-1116`）。释放事件（键抬起、按钮抬起）不等。
  - 浏览器的等价做法：`dc.bufferedAmount > ~1 KiB` 时暂存移动，只保留最后一个绝对位置，或累加相对位移；键和按钮事件照常立即发送。

### 2.2 消息定义（`src/protocol/input.rs:15-117`）

```proto
message InputMessage { oneof kind {
  Key key = 1;  LockState lock_state = 2;  MouseAbsolute mouse_absolute = 3;
  MouseRelative mouse_relative = 4;  MouseButton mouse_button = 5;
  MouseWheel mouse_wheel = 6;  SystemCommand system_command = 7;  Heartbeat heartbeat = 8; } }
message Key           { uint32 usage = 1; bool down = 2; bool repeat = 3; }   // usage = (page << 16) | id
message LockState     { bool caps = 1; bool num = 2; bool scroll = 3; uint32 seq = 4; }
message MouseAbsolute { optional int32 screen_id = 1; double x = 2; double y = 3; } // 屏内归一化
message MouseRelative { sint32 dx = 1; sint32 dy = 2; }
message MouseButton   { uint32 button = 1; bool down = 2; }   // 1 左 2 右 16 中 32 X1 64 X2
message MouseWheel    { sint32 dx = 1; sint32 dy = 2; }       // 1/120 格，两个轴同一条
message Heartbeat     {}
enum SystemCommand { UNSPECIFIED = 0; SAS = 1; LOCK = 2; TASK_MANAGER = 3; SHOW_DESKTOP = 4; TASK_VIEW = 5; }
```

### 2.3 被控的校验与执行

被控先解析校验（`src/features/host/input/wire.rs:57-159`），再入队（`input/mod.rs:254-316`），再执行（`engine.rs:556-672`）。

| 消息 | 校验 | 失败时 |
|---|---|---|
| Key | 页必须是 0x07 或 0x0C，且 id ≠ 0（`wire.rs:104-107`） | 解析错误，丢弃。错误只写进被控本地状态，主控收不到任何反馈（`input/mod.rs:244-249`，`src/features/host/access.rs:1785-1790`） |
| Key | 页合法，但 usage 不在 §2.4 的表中（`wire.rs:108-110`） | 静默忽略 |
| Key | 抬起时强制 `repeat=false`（`wire.rs:114`） | — |
| MouseAbsolute | x、y 有限，且在 `[0,1]` 内（`wire.rs:138-145`） | 丢弃 |
| MouseRelative | \|dx\|、\|dy\| ≤ 1 000 000（`engine.rs:520-524`） | **执行出错，会释放全部按键**（见下） |
| MouseButton | button ∈ {1, 2, 16, 32, 64}（`wire.rs:90-93`） | 丢弃 |
| MouseWheel | \|dx\|、\|dy\| ≤ 120 000（`wire.rs:87-88`、`:153-157`） | 丢弃 |
| SystemCommand | 1..=5（`wire.rs:123-133`） | 丢弃 |

**执行时的规则**（`src/features/host/input/mod.rs`、`engine.rs`）：

- **门控**：会话未取消、许可有效、有 `control` 授权、PeerConnection 已连接、本参与者持有控制权。否则直接丢弃（`input/mod.rs:275-277`、`:322-328`）。
- **队列**：每位参与者的待执行队列上限 64。满了就作废本代、释放全部按住的键和按钮，只接受之后的新输入（`input/mod.rs:291-311`）。
  - 相邻的、同一屏的绝对移动会合并；相对移动不合并（`:53-76`）。
  - 一批最多执行 16 个移动（`:27`、`:91-106`）。
- **几何变化**：屏幕布局变化后，入队时的几何与当前不符的事件直接丢弃（`input/mod.rs:623-629`）；引擎在几何变化或桌面切换（UAC、锁屏）时先释放全部按键（`engine.rs:245-262`）。
- **执行出错**：任何执行错误（例如 `screen_id` 不存在：“输入目标屏幕已失效”，`engine.rs:60-65`）都会释放全部按键（`input/mod.rs:672-680`）。**浏览器绝不能发不存在的 `screen_id`**。
- **心跳**：Heartbeat 只对当前持有输入的会话有效（`input/mod.rs:630-634`）。
- **注入路径**：前台不是原生主控窗口、且装了 Farwing HID 驱动时，用 HID 报告；否则用 `SendInput` 按扫描码注入（`engine.rs:236-244`、`:312-323`，`src/platform/windows/input/system.rs:57-72`）。
  - Pause、NumLock、Hanja、Hangul 用固定 VK 注入（`system.rs:63-69`）。
  - HID 路径只覆盖键盘页 0x04–0xE7 与 7 个消费者键（`src/platform/windows/input/hid.rs:131-155`）。其他消费者键回落到 `SendInput`。

### 2.4 键盘

**键码**：物理键的 USB HID usage，`(page << 16) | id`。键盘页为 0x07，消费者页为 0x0C。

- 被控按 usage 反查 set-1 扫描码注入，再由被控自己的布局和输入法决定字符（`src/platform/windows/input/keyboard.rs:1-8`、`:176-179`）。
- 原生的取法：从 Windows 扫描码查同一张表（`keyboard.rs:18-169`、`:221-232`）。
- 浏览器用 `KeyboardEvent.code` 查下表即可。**不要用** `key`、`keyCode`。

**`KeyboardEvent.code` → usage**：下表只列被控表中存在的键。不在表中的 code 不发送（被控也会忽略）。

| code | usage | code | usage | code | usage |
|---|---|---|---|---|---|
| `KeyA`…`KeyZ` | 0x070004…0x07001D（按字母顺序递增） | `Digit1`…`Digit9` | 0x07001E…0x070026 | `Digit0` | 0x070027 |
| `Enter` | 0x070028 | `Escape` | 0x070029 | `Backspace` | 0x07002A |
| `Tab` | 0x07002B | `Space` | 0x07002C | `Minus` | 0x07002D |
| `Equal` | 0x07002E | `BracketLeft` | 0x07002F | `BracketRight` | 0x070030 |
| `Backslash` | 0x070031 | `Semicolon` | 0x070033 | `Quote` | 0x070034 |
| `Backquote` | 0x070035 | `Comma` | 0x070036 | `Period` | 0x070037 |
| `Slash` | 0x070038 | `CapsLock` | 0x070039 | `F1`…`F12` | 0x07003A…0x070045 |
| `PrintScreen` | 0x070046 | `ScrollLock` | 0x070047 | `Pause` | 0x070048 |
| `Insert` | 0x070049 | `Home` | 0x07004A | `PageUp` | 0x07004B |
| `Delete` | 0x07004C | `End` | 0x07004D | `PageDown` | 0x07004E |
| `ArrowRight` | 0x07004F | `ArrowLeft` | 0x070050 | `ArrowDown` | 0x070051 |
| `ArrowUp` | 0x070052 | `NumLock` | 0x070053 | `NumpadDivide` | 0x070054 |
| `NumpadMultiply` | 0x070055 | `NumpadSubtract` | 0x070056 | `NumpadAdd` | 0x070057 |
| `NumpadEnter` | 0x070058 | `Numpad1`…`Numpad9` | 0x070059…0x070061 | `Numpad0` | 0x070062 |
| `NumpadDecimal` | 0x070063 | `IntlBackslash` | 0x070064 | `ContextMenu` | 0x070065 |
| `NumpadEqual` | 0x070067 | `F13`…`F24` | 0x070068…0x070073 | `NumpadComma` | 0x070085 |
| `IntlRo` | 0x070087 | `KanaMode` | 0x070088 | `IntlYen` | 0x070089 |
| `Convert` | 0x07008A | `NonConvert` | 0x07008B | `Lang1`（한/영） | 0x070090 |
| `Lang2`（한자） | 0x070091 | `ControlLeft` | 0x0700E0 | `ShiftLeft` | 0x0700E1 |
| `AltLeft` | 0x0700E2 | `MetaLeft`（旧 Firefox：`OSLeft`） | 0x0700E3 | `ControlRight` | 0x0700E4 |
| `ShiftRight` | 0x0700E5 | `AltRight` | 0x0700E6 | `MetaRight`（`OSRight`） | 0x0700E7 |
| `MediaTrackNext` | 0x0C00B5 | `MediaTrackPrevious` | 0x0C00B6 | `MediaStop` | 0x0C00B7 |
| `MediaPlayPause` | 0x0C00CD | `AudioVolumeMute` | 0x0C00E2 | `AudioVolumeUp` | 0x0C00E9 |
| `AudioVolumeDown` | 0x0C00EA | `MediaSelect` | 0x0C0183 | `LaunchMail` | 0x0C018A |
| `LaunchApp2`（计算器） | 0x0C0192 | `LaunchApp1`（我的电脑） | 0x0C0194 | `BrowserSearch` | 0x0C0221 |
| `BrowserHome` | 0x0C0223 | `BrowserBack` | 0x0C0224 | `BrowserForward` | 0x0C0225 |
| `BrowserStop` | 0x0C0226 | `BrowserRefresh` | 0x0C0227 | `BrowserFavorites` | 0x0C022A |

- 字母表的完整对应见 `keyboard.rs:34-71`：A=04、B=05 … Z=1D，即 HID 标准顺序。
- ISO 键盘的 `#` 键在浏览器里也报为 `Backslash`。被控把 0x070032 当作 0x070031 的别名（`keyboard.rs:168`），所以发 0x070031 即可。
- 被控表里没有的键（例如 `Power`、`Sleep`、`Fn`、`Help`）直接丢弃，不要发送。

**按下、抬起、重复**：

- `keydown` → `Key{usage, down: true, repeat: e.repeat}`；`keyup` → `Key{usage}`。
  - 原生把“已按住的键再次按下”标为重复（`src/features/remote_input.rs:441`）。
  - 被控对 `SendInput` 路径会再注入一次按下；HID 路径交给被控系统自己重复（`engine.rs:601-609`）。
  - **浏览器应转发 `e.repeat`**，不要自己丢弃重复。
- 按 `code` 记录哪些键处于按下状态。只发送“按下时已经处于控制状态”的键的抬起。原生的做法：开启控制后，要等本地键鼠全部松开才开始转发（`remote_input.rs:368-386`、`:603-611`）。
- **统一释放**（失去焦点、`visibilitychange`、关闭控制、控制权丢失时）：先抬起普通键，再抬起修饰键，最后抬起按住的鼠标键（`remote_input.rs:634-666`）。
- **修饰键与 AltGr**：
  - 修饰键按普通键发送。原生不吞掉修饰键，由本机维护修饰状态（`src/application/viewer/windows_keyboard.rs:1051-1057`）。
  - Windows 上的 Chrome 把 AltGr 报为 `ControlLeft` + `AltRight`，照原样转发即可。原生同样转发 Windows 产生的假 LCtrl（`windows_keyboard.rs:1038-1044`）。
  - macOS/Linux 上的浏览器只报 `AltRight`。被控布局如果带 AltGr，Windows 会把注入的右 Alt 当作 AltGr，问题不大，以实测为准。

**保活与卡键**：

- 被控在 `max(150 ms, 3×RTT + 50 ms)`（RTT 上限按 3 s 计，取该参与者的 RTCP RTT）内收不到该参与者的任何 INPUT 帧，就释放其按住的键（`src/features/host/input/mod.rs:36-44`，`engine.rs:93`、`:673-688`）。
  - Backspace 按住时，100 ms 无输入就提前释放（`engine.rs:90-91`、`:679-681`）。
  - 注意：这个“键盘中断”开关取自输入配置，默认开启，当前也没有修改入口（`src/features/host/input/config.rs:16-23`）。
- 原生的做法：有键按住、且 50 ms 内没有发出任何 INPUT 时发 `Heartbeat`，每 15 ms 检查一次（`remote_input.rs:18-19`、`:490-503`，`src/transport/rtc/channels.rs:475`、`:481`）。任何 INPUT 帧都算保活。
- 浏览器照做：有键按住时，用 `setInterval(15)` 检查，距上次发帧 ≥ 50 ms 就发 `42 00`。后台标签页的计时器会被节流，被控会因此释放按键，这是期望的安全行为。

**锁定键**（`src/features/host/input/locks.rs`，`remote_input.rs:463-488`、`:978-985`、`:1306-1311`）：

- CapsLock、NumLock、ScrollLock 按普通键转发。
- 下面两种时机，发一次 `LockState{caps, num, scroll, seq}`：
  - 开启控制时；
  - 本地锁定状态变了，而且不是由已转发的锁定键造成的（例如在别的窗口切了 CapsLock）。
- `seq` = 本连接已发出的锁定键按下次数（不含重复）。被控统计同一流上收到的锁定键按下次数：
  - 收到的比 `seq` 多，说明有按键与这个状态交叉，不纠正，然后把计数对齐到 `seq`（`locks.rs:17-31`）。
  - 否则等本参与者的锁定键注入稳定 50 ms、且没有锁定键按住时，只对不一致的位注入一次锁定键（`locks.rs:8-10`、`:58-73`，`engine.rs:385-406`）。
  - 2 s 内没能执行的纠正作废。
- 浏览器的本地状态用 `e.getModifierState("CapsLock"|"NumLock"|"ScrollLock")` 读取。macOS 上 NumLock 读数恒为假，见 §8。

**输入法**：

- 原生在控制时解除本窗口的输入法上下文，按键原样发给被控，由被控的输入法组字（`windows_keyboard.rs:1133-1162`）。
- 浏览器的做法：把焦点放在**不可编辑**的元素上（`<div tabindex=0>` 或 `<video>`），浏览器就不会启用本地输入法。
- `isComposing` 为真的事件不转发。`key === "Process"` 时仍按 `code` 转发。
- 被控**没有文本（Unicode）输入事件**，v2 删除了它（`docs/protocol/v2-design.md` §7 “删除”）。文本输入的替代办法见 §8。

**快捷键拦截**：原生有“拦截本机快捷键”开关（`src/application/viewer/stream_menu.rs:1063`），用低级键盘钩子截获 Win、Alt+Tab 等（`windows_keyboard.rs:226-240`）。浏览器对应的是全屏 + Keyboard Lock（§2.8、§8）。

### 2.5 鼠标：绝对坐标

- **坐标空间**：屏内归一化 `[0,1]`，对应 `screen_id` 那块屏的完整矩形（被控物理像素）。被控按下式取整、钳位后注入（`engine.rs:619-645`）：

  ```
  px = clamp(round(left + width × x), left, right − 1)
  ```

  y 同理。
- **`screen_id`**：取 SESSION Screens 里 `video_track_index` 等于浏览器正在显示的轨道（通常为 0）的那个 `PbScreen.id`。
  - 这些 id 与被控的输入几何同源（`src/features/stream_control/publisher/displays.rs:10-40`，`src/features/host/peer/mod.rs:451-463`）。
  - 原生的取法相同：`mouse_screen(track)`（`src/features/stream_control/input.rs:52-59`）。
  - 省略 `screen_id` 时，被控用自己的“当前屏”（`engine.rs:60-65`）。**总是显式写出**，屏幕 0 也要写 `08 00`。
- **原生的映射**（`src/application/viewer/windows_mouse.rs:114-166`）：
  - 先求视频在窗口中按比例缩放后的显示矩形，用指针相对该矩形的位置除以宽高。
  - x 钳位到 `[0, 1 − 1/屏宽]`，y 钳位到 `[0, 1 − 1/屏高]`，屏宽、屏高取 Screens 的 `current_resolution`。
  - 原生本地只接受 `[0,1)` 的值（`src/features/remote_input.rs:834-861`）。
  - 按住按键拖出画面时，坐标钳在边缘、继续发送（`windows_mouse.rs:156-163`）。
- 浏览器：用 `<video>` 的 `object-fit: contain` 显示矩形，加上 `videoWidth/videoHeight` 算出画面区域，算法同上。鼠标在画面外、且没有按键按住时不发送。
- 光标：绝对模式下原生的“本地”模式关闭被控的光标合成（`cursor_capture=false`），把 CURSOR 形状作为本地光标显示（`input.rs:390-401`）。浏览器可以把 CURSOR 的 PNG 设为 CSS `cursor: url(data:…) hotX hotY, auto`；`cursor_type == -1` 时用 `cursor: none`。CSS 光标尺寸的限制见 §8。

### 2.6 鼠标：相对模式与指针锁定

- `MouseRelative{dx, dy}` 是累计位移。被控用 `SendInput(MOUSEEVENTF_MOVE)` 或 HID 相对报告注入（`engine.rs:520-543`），**会叠加被控自己的指针速度和加速**。
  - 文档写的是“被控像素”，实际是鼠标计数，见 §9。
  - 原生发送原始鼠标的 `lLastX/lLastY`（`windows_mouse.rs:449-455`），连续的相对移动在本地累加合并（`remote_input.rs:863-883`）。
- 被控的执行规则：没有按键按住时，相对移动把被控切到“相对模式”；按住按键期间，路由和模式保持不变（`engine.rs:525-533`）。
- 浏览器：`await el.requestPointerLock({ unadjustedMovement: true })`。不支持时会抛 `NotSupportedError`，回退为不带参数再请求一次。用 `pointermove` 的 `movementX/movementY` 取位移，把同一帧内的位移累加后发送。
  - 指针锁定期间，要用 `capture_setting` 打开 `cursor_capture`，让被控把光标合成进画面，与原生“远程”模式相同（`input.rs:390-401`）。
  - 按 Esc 会退出指针锁定，这是浏览器保留的行为，要处理 `pointerlockchange`。
- 智能模式：被控光标隐藏（CURSOR `cursor_type == -1`）时切到相对模式（`input.rs:395-398`）。指针锁定需要用户手势才能进入，浏览器只能提示用户点击画面来进入。

### 2.7 鼠标按键与滚轮

- 按键：`MouseEvent.button` 0/1/2/3/4 分别对应 wire 上的 1（左）、16（中）、2（右）、32（X1）、64（X2）。按下和抬起各发一次，不要合并快速的按下/抬起（`remote_input.rs:885-922`）。
  - 被控对没按下的键收到抬起会忽略（`engine.rs:464-466`）。
  - 要阻止 `contextmenu` 和中键的默认行为；X1/X2 要阻止浏览器的前进/后退。
- 原生在按键前先发一次当前位置（`windows_mouse.rs:449-455`、`:515-517`）。浏览器在发 `MouseButton` 前要先确保最新的绝对位置已发出（不能被暂存）。
- 滚轮：`MouseWheel{dx, dy}`，单位为 1/120 格，一条消息同时带两个轴（`src/protocol/input.rs:94-101`）。
  - **符号与 Windows 一致**：dy 为正 = 向上（远离用户），dx 为正 = 向右。原生把 winit 的行增量还原为 `[-x×120, y×120]`（`windows_mouse.rs:557-563`），被控原样作为 `MOUSEEVENTF_WHEEL/HWHEEL` 的数据（`engine.rs:661-668`）。
  - 浏览器的 `WheelEvent.deltaY` 为正 = 向下，所以 `dy = -deltaY`，`dx = +deltaX`。
  - 换算（Chrome 在 Windows 上每格约 100 px，不同浏览器不同，需实测）：
    - `deltaMode = 0`（像素）：`单位 = delta × 1.2`；
    - `deltaMode = 1`（行）：`单位 = delta × 40`（3 行 = 1 格）；
    - `deltaMode = 2`（页）：`单位 = delta × 120`。
  - 累加小数余量后取整，发出整数部分。
  - 不是 120 整数倍的值，被控走系统路径，保留精细滚动（`engine.rs:646-669`）。单条消息的绝对值不能超过 120 000。
- 原生滚动前也先发当前位置（`windows_mouse.rs:260-266`）。

### 2.8 系统命令与特殊键菜单

| 菜单项 | 发送 | 被控的执行 |
|---|---|---|
| Ctrl+Alt+Del | `SystemCommand SAS (1)`：帧体 `38 01` | 被控必须以系统服务模式运行：输入由服务拥有的常驻进程执行（`backend.rs:46-60`），或经服务的输入代理执行（`src/features/host/input/broker.rs:305`）。普通用户进程里的被控会报“Ctrl+Alt+Del需要安装并启用被控服务”（`src/features/host/input/backend.rs:96-120`）。还要求注册表策略 `SoftwareSASGeneration` 为 1 或 3，否则报“系统策略未允许服务发送Ctrl+Alt+Del”（`src/platform/windows/host_service/service.rs:8-36`）。**两种失败都只记在被控本地，浏览器收不到** |
| 锁定 | `LOCK (2)`：`38 02` | `LockWorkStation`（`engine.rs:577-579`） |
| 任务管理器 | `TASK_MANAGER (3)`：`38 03` | 用与布局无关的 VK 注入 Ctrl+Shift+Esc（`engine.rs:580-582`、`system.rs:75-83`） |
| 显示桌面 | `SHOW_DESKTOP (4)`：`38 04` | 注入 Win+D |
| 任务视图 | `TASK_VIEW (5)`：`38 05` | 注入 Win+Tab |
| Win、Win+R、Alt+Tab、Alt+F4、PrintScreen 等 | 按键流，例如 `Key 0x0700E3 ↓`、`Key 0x070015 ↓↑`、`Key 0x0700E3 ↑` | 普通键注入 |

- 执行系统命令前，被控先释放该参与者按住的全部键和按钮（`engine.rs:571-573`）。原生也先在本地释放（`src/features/remote_input.rs:397-422`）。
- 原生菜单见 `src/application/viewer/stream_menu.rs:1137-1141`。原生只对 Windows 被控开放系统命令（`remote_input.rs:392-402`）。
- 不要靠键流模拟 Ctrl+Alt+Del 或 Win+L：v2 已删除“经键流识别”（`docs/protocol/v2-design.md` §7），`SendInput` 也不能触发安全注意序列。
- Alt+Tab 菜单项：发 Alt↓、Tab↓↑，保持 Alt 直到用户再次点击或超时，再发 Alt↑。按住 Alt 期间要继续发心跳。

### 2.9 字节示例（帧头均为 `46 57 02 00 01 01 00 00 00 00`）

| 事件 | 帧体 |
|---|---|
| A 键按下（usage 0x070004 = varint `84 80 1c`） | `0a 06 08 84 80 1c 10 01` |
| A 键重复 | `0a 08 08 84 80 1c 10 01 18 01` |
| A 键抬起 | `0a 04 08 84 80 1c` |
| 左 Win 按下（0x0700E3 = `e3 81 1c`） | `0a 06 08 e3 81 1c 10 01` |
| 静音键按下（0x0C00E2 = `e2 81 30`） | `0a 06 08 e2 81 30 10 01` |
| LockState{caps, num, seq 3} | `12 06 08 01 10 01 20 03` |
| 绝对坐标：屏 0，x 0.5，y 0.25 | `1a 14 08 00 11 00 00 00 00 00 00 e0 3f 19 00 00 00 00 00 00 d0 3f` |
| 相对位移 dx −5、dy 7（zigzag 9、14） | `22 04 08 09 10 0e` |
| 左键按下 / 抬起 | `2a 04 08 01 10 01` / `2a 02 08 01` |
| 右键 / 中键按下 | `2a 04 08 02 10 01` / `2a 04 08 10 10 01` |
| 滚轮向下一格（dy −120，zigzag 239） | `32 03 10 ef 01` |
| 滚轮向上一格（dy +120） | `32 03 10 f0 01` |
| SAS | `38 01` |
| 心跳 | `42 00` |

编码要点：

- sint32 的 zigzag：`((n << 1) ^ (n >> 31)) >>> 0`。
- double：小端 float64，wire type 1，即 tag 字节 `(field<<3)|1`。
- 值为 0 的 x、y、dx、dy 可以省略，但 `screen_id` 不能省略。

---

## 3. 剪贴板 v2（CLIPBOARD 0x0301）的文本子集

### 3.1 前提与两种模式

- **HELLO 必须声明 `clipboard: 1`**。被控按主控 HELLO 设定剪贴板级别：没有 `clipboard` 为 0（关闭），有 `clipboard` 为 1（文本等格式），有 `clipboard.files` 为 2（`src/features/stream_control/publisher.rs:194-204`、`:380-411`）。
  - 收到 HELLO 之前，被控按原生特性集当作 2（`publisher.rs:170-176`）。
  - 级别为 0 时，请求一律回 `unavailable`，推送被忽略。
  - 浏览器**不要**声明 `clipboard.files`。不声明时，文件格式不会出现在 Offer 里，ListFiles/ReadFile 回 `forbidden`（`src/features/clipboard/formats.rs:47-53`，`src/features/clipboard/mod.rs:820-824`）。
- 被控把网页主控当作 Windows 平台，格式按原样透传（`src/protocol/peer_platform.rs:17-42`，`src/transport/signal/host.rs:650-651`，`formats.rs:141-144`）。

| 模式 | 被控的条件 | 原生的条件 |
|---|---|---|
| **自动同步**（双向推 Offer） | `clipboard` 授权、被控剪贴板开关（默认开）、级别 ≥ 1、PeerConnection 已连接；**多人时还要有 `control` 授权并持有控制权**（`src/features/host/access.rs:1673-1688`） | 会话就绪 + 处于控制模式（不是观看）+ 用户开关“剪贴板同步”（默认开）（`src/features/stream_control/input.rs:338-352`，`src/features/viewing_settings.rs:37`、`:83`） |
| **手动获取**（Pull） | 只要 `clipboard` 授权与被控开关（`access.rs:1689-1701`） | 菜单“获取被控剪贴板”，观看模式下也可用（`stream_menu.rs:1102-1118`，`mod.rs:386-401`） |

- 不在自动模式时，对方的 Offer 推送会被忽略（`mod.rs:783-790`）。Pull 的回复即使不在自动模式也会被采用（`src/features/clipboard/native.rs:411-415`）。
- SESSION tag 4 `ClipboardPermission{enabled}` 只与文件剪贴板有关，浏览器可以忽略（`src/features/host/clipboard.rs:228-254`）。

### 3.2 消息定义（`src/features/clipboard/wire.rs:20-160`）

```proto
message ClipboardMessage { oneof body {
  Offer offer = 1; Fetch fetch = 2; Data data = 3; ListFiles list_files = 4; Files files = 5;
  ReadFile read_file = 6; Release release = 7; Abort abort = 8; Pull pull = 9; } }
message Format { uint32 id = 1; string name = 2; }            // Windows 剪贴板格式号；注册格式才带 name
message Offer  { uint32 offer = 1; repeated Format formats = 2; }   // 推送；也是 Pull 的回复
message Fetch  { uint32 offer = 1; Format format = 2; uint32 stream = 3; }   // 请求 → Data
message Data   { uint64 size = 1; bytes inline = 2; }         // 回复
message Pull   {}                                              // 请求（C→H）→ Offer
message Abort  { uint32 stream = 1; }                          // 推送
// 文件相关的 ListFiles/Files/ReadFile/Release 浏览器不用
```

- **文本格式为 `{id: 13}`（CF_UNICODETEXT），`name` 为空**。可以接受的格式号见 `formats.rs:40-58`。不要用 `text/plain` 之类的名字：它们会被注册成私有格式，Windows 程序不认识。
- **编码是原始 HGLOBAL 字节，即 UTF-16LE 加结尾的 `00 00`**。Windows 路径不转换换行，发送前要把 `\n` 改成 `\r\n`。
  - 被控→浏览器的数据是整块内存，可能带补零（`native.rs:735-803`）。浏览器取到第一个 U+0000 为止。
  - 浏览器→被控的数据原样写入（`native.rs:749-765`；测试用例的写法见 `src/features/clipboard/memory.rs:212`）。
- Offer 编号由发送方从 1 递增，跳过 0（`mod.rs:588-595`）。空的 `formats` 表示清空（`wire.rs:46-47`）。请求的 `request_id` 非 0。请求方与回复方的 id 空间分开，分片按 `(request_id, 是否回复)` 拼接（`src/protocol/peer.rs:55-92`）。

### 3.3 通道

- 被控**只在 session 通道**接收 CLIPBOARD（`src/features/host/peer/channels.rs:305-330`；control 通道的入口不处理剪贴板，见 `ingress.rs:39-97`）。被控也在 session 通道发送。
- 大于 32 KiB 的数据走 bulk 通道上的 TRANSFER（`kind = 2`）和 FLOW。被控只在 bulk 通道接收这两种帧（`channels.rs:250-262`，`src/features/host/clipboard.rs:74-88`）。

### 3.4 浏览器 → 被控（推送本机文本）

1. 读取本机文本（权限问题见 §8），换行改为 CRLF，编码为 UTF-16LE 并加 `00 00`。
2. 推送 `Offer{offer: N, formats: [{id: 13}]}`，rid = 0。被控只在自动模式下采用（§3.1）。
3. 被控**按需取数据**：只有被控上有程序粘贴时，才发 `Fetch{offer: N, format: {13}, stream: S}`（`native.rs:443-533`、`:1538-1556`）。
   - 所以浏览器要一直保留第 N 份文本，直到发出新的 Offer。
   - 回复 `RESPONSE` `Data{size, inline}`。
4. 如果 `size > 32 KiB`：
   - 回复 `Data{size}`，`inline` 为空。
   - 在 bulk 通道发送 `TRANSFER Chunk{stream: S, kind: 2, offset, data ≤ 65472 B, last, entry: 0}`。
   - 不能超过信用：初始 4 MiB，之后按对方 FLOW 给出的累计值（`mod.rs:658-667`，`src/transport/transfer.rs:11-13`、`:83-119`）。
5. 编号过期或格式不认识时，回 `RESPONSE|ERROR` `PeerError{code: "not_found"}`。收到 `Abort{S}` 就停止发送该流。
6. 防回环：刚从被控取到的文本不要再 Offer 回去。原生靠标记格式 `Farwing.Clipboard.Remote` 区分（`native.rs:52`、`:538-581`），浏览器没有这个标记，只能比较内容。

### 3.5 被控 → 浏览器（获取被控文本）

- **手动**：发 `Pull` 请求（rid ≠ 0），回复是 `Offer`。
- **自动**：自动模式下，被控本机复制时会推送 `Offer`。连接激活时被控不会主动推送当前内容（`native.rs:382-390`），要拿当前内容只能先 Pull。
- 在 `formats` 里找 `id == 13`，发 `Fetch{offer, {13}, stream: S}`。**`S` 必须非 0**，否则大于 32 KiB 时被控回 `internal`（`mod.rs:664-666`）。
  - 被控只服务自己当前的 Offer，旧编号回 `not_found`（`native.rs:1138-1164`）。
- 回复是 `Data`：`size ≤ 32 KiB` 时数据在 `inline` 里；否则 `inline` 为空，数据从 bulk 上的流 S 到达，可能早于 Data 回复（`mod.rs:877-891`）。
  - 浏览器写完数据后，按每空出四分之一窗口回一个 `FLOW{channel: 1, stream: S, credit: 累计值, kind: 2}`（`transfer.rs:124-192`）。
- 30 s 没有进展的请求作废，作废的 Data 请求要推送 `Abort{S}`（`mod.rs:160`、`:1019-1041`）。

### 3.6 限制与致命错误

- **限制**：
  - 每个 Offer 最多 256 种格式，格式名不超过 1024 字节（`mod.rs:1152-1154`）。
  - 每种格式不超过 128 MiB（`mod.rs:33`）。
  - 每个请求方最多 64 个未完成请求（`mod.rs:35`、`:1004`）。
  - 分片片长 60 KiB；最多 16 条未完成的分片消息，拼接后不超过 48 MiB（`peer.rs:34-53`、`:80`，`wire.rs:16-18`）。
  - 纯文本的 Offer 和 Fetch 都不需要分片。
- **致命错误**（被控的剪贴板后端会退出，直到 session 通道重建都不可用）：
  - 帧格式错误（`mod.rs:529`）；
  - 发送了 Offer/Release/Abort 之外的推送（`mod.rs:801`）；
  - Offer 的格式表不合法（`mod.rs:784`）；
  - 被控剪贴板线程的命令队列满（`mod.rs:788`、`:826-832`）。

  后端退出的路径见 `src/features/host/clipboard.rs:152-169`、`:270-271`。所以**不要发不合法的帧，也不要短时间内大量发 Offer**。
- **错误码**：

  | 码 | 场景 |
  |---|---|
  | `unavailable` | 同步暂停、锁屏、被控没有登录的用户代理（`mod.rs:802-809`，`src/features/host/clipboard/agent.rs:427-448`） |
  | `bad_request` | 请求类型不对 |
  | `forbidden` | 文件剪贴板，或剪贴板来源不允许读取 |
  | `not_found` | 编号过期、没有该格式 |
  | `internal` | 被控读取失败 |

### 3.7 字节示例

| 消息 | 字节 |
|---|---|
| 浏览器 Offer（N=1，文本） | `46 57 02 00 03 01 00 00 00 00` + `0a 06 08 01 12 02 08 0d` |
| 被控 Fetch（rid 1，stream 5） | `46 57 02 00 03 01 00 00 00 01` + `12 08 08 01 12 02 08 0d 18 05` |
| 浏览器回复 "hello"（12 字节，含 NUL） | `46 57 02 01 03 01 00 00 00 01` + `1a 10 08 0c 12 0c 68 00 65 00 6c 00 6c 00 6f 00 00 00` |
| Pull（rid 1） | `46 57 02 00 03 01 00 00 00 01` + `4a 00` |
| 错误 not_found | `46 57 02 03 03 01 00 00 00 01` + `0a 09 6e 6f 74 5f 66 6f 75 6e 64` |
| 大数据流的 FLOW（stream 1，credit 5 MiB） | `46 57 02 00 0a 01 00 00 00 00` + `08 01 10 01 18 80 80 c0 02 20 02` |

---

## 4. 文件（FILE 0x0401、TRANSFER 0x0402、FLOW 0x0A01）

### 4.1 前提

- **授权**：被控按 `file_access` 放行：许可有效、有 `files` 授权、被控不是游客模式、被控“允许文件传输”开关打开（`src/features/host/access.rs:1725-1730`）。
  - 不满足时，每个请求都回 `Failed{code 2 DENIED}`（`src/features/file_transfer/host/agent.rs:162-164`）。
  - PERMISSIONS 里有 `files` 也可能被拒，因为被控开关不在授权表里。
- **通道（硬性）**：被控的文件后端只在 **session 与 bulk 两个通道都打开**之后才启动（`src/features/host/files.rs:131-158`）。没有 bulk 通道时，连 `List` 也不会有回复。
- **HELLO**：被控解析主控的 `files` 特性，但不用它做门控（`publisher.rs:403-407`，`agent.rs:123-186`）。原生则要求被控声明了 `files`、PERMISSIONS 里有 `files`（`src/features/stream_control/handshake.rs:415-419`、`:436`）。
- **服务模式**：没有已登录的用户时回 `BUSY`，“被控端没有已登录的用户…”（`agent.rs:303-305`）。用户进程启动期间也可能短暂返回 BUSY，应重试。
- **多人**：每位参与者各有一套引擎、记录和配额（`src/features/host/peer/mod.rs:679-687`）。
- **被控界面**：不弹确认框，只显示“正在接收文件”等进度卡片（`src/features/file_transfer/host/notices.rs:6-34`）。
- **中途收回授权**：进行中的每个传输收到 `ItemDone{DENIED}`，之后的请求回 `Failed{DENIED}`。上传的半成品和续传记录保留（`agent.rs:128-141`，`src/features/file_transfer/host.rs:454-466`，`src/features/file_transfer/host/transfer.rs:278-296`）。

### 4.2 帧与消息（`src/features/file_transfer/wire.rs:23-368`，`src/transport/transfer.rs:10-68`）

```proto
message FileMessage { oneof body {
  List list = 1; MakeDir make_dir = 2; Rename rename = 3; Remove remove = 4; Exists exists = 5;
  Upload upload = 6; UploadStart upload_start = 7; Download download = 8; DownloadStart download_start = 9;
  Cancel cancel = 10;                                                   // C→H 请求
  Listing listing = 20; Done done = 21; Existing existing = 22; Plan plan = 23; Manifest manifest = 24;
  Accepted accepted = 25; Failed failed = 26;                           // H→C 回复
  ItemDone item_done = 30; } }                                          // H→C 推送（rid 0）
message List     { string path = 1; }                       // "" = 根（磁盘 + 常用文件夹）
message Entry    { string name = 1; int32 kind = 2; uint64 size = 3; uint64 mtime_ns = 4;
                   bool hidden = 5; string path = 6; string icon = 7; }   // path/icon 只在根列表中
message Listing  { string path = 1; repeated Entry entries = 2; }
message Item     { string path = 1; uint64 size = 2; uint64 mtime_ns = 3; bool dir = 4; } // path 用 '/' 分隔的相对路径
message Upload   { uint32 transfer = 1; string dir = 2; string folder = 3; repeated Item items = 4;
                   int32 conflict = 5; string key = 6; }
message ItemPlan { uint64 offset = 1; bytes tail_sha256 = 2; bool skip = 3; }
message Plan     { uint32 transfer = 1; repeated ItemPlan items = 2; }
message UploadStart   { uint32 transfer = 1; repeated uint64 offsets = 2; }     // packed
message Download      { uint32 transfer = 1; repeated string paths = 2; string key = 3; }
message Manifest      { uint32 transfer = 1; string folder = 2; repeated Item items = 3; }
message DownloadStart { uint32 transfer = 1; repeated ItemPlan items = 2; }
message Accepted { repeated uint64 offsets = 1; }                               // packed
message Cancel   { uint32 transfer = 1; string key = 2; bool discard = 3; }
message Done     { string path = 1; }
message ItemDone { uint32 transfer = 1; uint32 entry = 2; int32 error_code = 3; string message = 4; } // entry 0xFFFFFFFF = 整个传输
message Failed   { int32 code = 1; string message = 2; }
// bulk 通道：
message Chunk { uint32 stream = 1; int32 kind = 2; uint64 offset = 3; bytes data = 4; bool last = 5; uint32 entry = 6; } // TRANSFER
message Flow  { int32 channel = 1; uint32 stream = 2; uint64 credit = 3; int32 kind = 4; }                        // FLOW
```

**常量**（`wire.rs:101-127`，`transfer.rs:11-25`、`:65-68`）：

| 名称 | 取值 |
|---|---|
| `Entry.kind` | 1 文件、2 目录、3 磁盘、4 快捷方式 |
| `conflict` | 1 覆盖、2 重命名、3 跳过 |
| `code` / `error_code` | 0 OK、1 PROTOCOL、2 DENIED、3 NOT_FOUND、4 EXISTS、5 NO_SPACE、6 IO、7 BUSY、8 CHANGED、9 CANCELLED |
| `kind`（Chunk/Flow） | 1 FILE（剪贴板 2、拖放 3、诊断 4） |
| `channel`（Flow） | 1 BULK |

**规则**：

- **请求与错误**：FILE 请求的 rid 非 0，回复带 RESPONSE。文件引擎自己只回 `Failed`，不回 `PeerError`（`host.rs:105-120`）。没有分类的错误一律映射为 DENIED（`host.rs:77-101`）。
- **分片**：FILE 有自己的分片器，片长 65526 字节（64 KiB − 10），除最后一片外都置 MORE。最多 16 条未完成的分片消息，拼接后不超过 32 MiB（`wire.rs:17-21`、`:455-474`、`:580-609`）。根列表和大目录的 Listing/Manifest 可能分片。
- **数据块**：`Chunk.data` 不超过 65472 字节。`stream` 等于 FILE 消息里的 `transfer`。`entry` 是清单下标，`offset` 是该条目内的位置。条目的最后一块置 `last`；空文件和目录只发一个空的 `last` 块。
- **信用**（`transfer.rs:83-192`）：
  - 发送方初始 4 MiB。FLOW 的 `credit` 是累计允许发送的字节数，只增不减；比当前值小的会被忽略。
  - 接收方写盘后，每空出四分之一窗口发一次 FLOW。交付跟得上时窗口倍增，上限 64 MiB。
  - 超出信用视为协议错误：被控会以 `ItemDone{PROTOCOL}` 终止该传输（`host.rs:248-256`）。
  - 只计 `data` 的字节数。
- **通道**：FILE 走 session，TRANSFER/FLOW 走 bulk，bulk 的积压有上限（`src/transport/outbox.rs`）。被控只在 session、bulk 上接收文件消息，control 上的不处理。被控发送时 TRANSFER/FLOW 走 bulk，其余走 session。

### 4.3 路径、目标文件夹与冲突

- **根列表** `List{path: ""}`（`src/features/file_transfer/service.rs:860-886`，`src/platform/windows/file_locations.rs:5-27`）：
  - 磁盘 A–Z：`kind=3`，`name="(C:)"`，`path="C:\"`，`icon="disk"`。
  - 然后是已知文件夹：`kind=2`，`path` 为绝对路径，`icon` 依次为 `document`、`desktop`、`picture`、`video`、`music`、`download`。
- **默认目录别名**：`":/Default"` 解析为被控当前用户的“下载”文件夹，在 Upload.dir、List、Exists、MakeDir 中都能用（`src/features/file_transfer/storage.rs:103-106`）。原生不使用它。
- **路径格式**（`storage.rs:40-70`、`:103-147`）：
  - 只接受 `X:\…`、`X:/…`、`X:` 和 `:/Default`。UNC 路径与相对路径都拒绝。
  - 每一段不能是空串、`.` 或 `..`，不能以点或空格结尾，不能含控制字符或 `:<>"|?*`，不能是 Windows 保留名（CON、PRN、COM1 等）。
  - 已存在的上级目录不能是重解析点。
  - 没有根目录沙箱：被控当前用户能访问的路径都可以。磁盘操作在非 SYSTEM 的用户进程里执行（`agent.rs:385-388`）。
- **浏览器文件名要先清洗**，否则整个 Upload 会失败。
- **冲突策略**：取值必须是 1..3，否则回 PROTOCOL（`transfer.rs:168-171`）。原生默认 2（重命名，`src/features/file_transfer/ui.rs:62`）。
  - 跳过：目标已存在的条目在 Plan 里标 `skip`，不要发送它的数据（`storage.rs:403-408`）。
  - 覆盖：`MoveFileExW` 替换。
  - 重命名：落盘时改名为 `名称 (n).扩展名`（`storage.rs:500-519`、`:551-585`）。
  - `folder` 必须是单段名；为空串时，条目直接放进 `dir`，中间目录自动创建（`src/features/file_transfer/host/filesystem.rs:205-230`，`transfer.rs:218-222`）。
- **半成品与完成**：
  - 写入期间是隐藏文件 `.<名称>.<被控记录UUID>.downloading`（`storage.rs:467-472`）。
  - 完成时检查长度等于 `size`，设置 `mtime_ns`（为 0 则不改），再改为正式名（`storage.rs:475-532`）。
  - 没有整文件校验。

### 4.4 上传（浏览器 → 被控）的最小流程

原生的完整流程见 `src/features/file_transfer/tasks.rs:293-506`。浏览器最小实现：

1. 选目标目录：用 `List{""}` 里 `icon == "desktop"` 或 `"download"` 那一项的 `path`，或直接用 `":/Default"`。
2. 分配传输号 T：每个连接从 1 递增，非 0；在本连接内未结束的传输中唯一。
3. 请求 `Upload{transfer: T, dir, folder: "", items: [{path: 清洗后的文件名, size, mtime_ns: BigInt(file.lastModified) * 1_000_000n}], conflict: 2, key: crypto.randomUUID()}`，回复 `Plan`。
   - **`key` 必填**：长度 1..1024 字节，不能含 NUL。空 key 会被拒为 DENIED（`filesystem.rs:68-80`）。
   - `Plan.items` 与 `items` 一一对应，带续传偏移和尾部哈希。
4. 请求 `UploadStart{transfer: T, offsets}`，回复 `Done`。
   - `offsets` 长度必须等于条目数。每一项只能是 0（从头写）或 Plan 给的偏移（续传），否则回 PROTOCOL（`transfer.rs:334-343`）。
   - 最小实现全部填 0。
   - 续传时，要先对本地文件同一段计算 SHA-256（偏移之前最多 1 MiB），与 `tail_sha256` 一致才用 Plan 给的偏移（`storage.rs:257-288`）。
5. 在 bulk 通道按清单顺序发 `Chunk{stream: T, kind: 1, entry: i, offset, data, last}`，跳过 `skip` 的条目。
   - 不能超过信用（4 MiB + 收到的最大 FLOW 值）。
   - 用 `file.slice(o, o + 65472).arrayBuffer()` 流式读取，同时看 `dc.bufferedAmount`：设 `bufferedAmountLowThreshold = 256 KiB`，超过时等待 `bufferedamountlow`。
6. 每个非跳过的条目都会收到一个 `ItemDone{transfer: T, entry: i, error_code: 0}` 推送，这是**唯一的完成信号**（`transfer.rs:433-453`）。
   - 失败时收到 `ItemDone{error_code ≠ 0}`；`entry = 0xFFFFFFFF` 表示整个传输失败。
7. **超时**：被控等下一个事件（UploadStart 或数据块）最多 60 s，超时以 DENIED 结束（`transfer.rs:34-41`）。浏览器等回复最多 60 s（原生 `src/features/file_transfer/mod.rs:35`）。
8. **取消**：`Cancel{transfer: T, key, discard: true}`，回复 `Done`，同时删除半成品和续传记录（`host.rs:340-377`，`transfer.rs:302-308`）。只发 `Cancel{transfer: T}` 相当于暂停，可以续传。

**被控检查的错误**（`transfer.rs:329-424`）：

- 数据块早于 UploadStart；
- 条目顺序不对；
- 目录条目带数据；
- offset 不连续；
- 写入超过 `size`；
- 重复的 UploadStart。

**配额**（`storage.rs:20`、`:71-86`，`host.rs:291`、`:390-402`，`filesystem.rs:115-120`）：

| 项目 | 上限 |
|---|---|
| 每个 Upload 的条目数 | 100 000，路径不分大小写去重；目录的 size 为 0 |
| 每个连接同时进行的传输 | 8 个，否则 BUSY |
| 同时进行的操作 | 少于 32 |
| 未完成的接收记录 | 256 条 |

### 4.5 下载（被控 → 浏览器）的最小流程

原生的完整流程见 `tasks.rs:508-724`。

1. **浏览目录**：`List{path}`，回复 `Listing{path: 解析后的目录, entries}`。完整路径 = `Listing.path` + `\` + `name`（`service.rs:988-999`）。
   - 目录在前，按小写名排序（`service.rs:920-923`）。
   - `.downloading` 半成品不列出。
   - `kind=4` 的快捷方式和链接不能进入，也不能下载。
2. **先登记流 T，再发请求**：数据块可能先于 session 上的回复到达，原生会缓存它们（`mod.rs:322-339`）。
3. 请求 `Download{transfer: T, paths: [完整路径], key: ""}`，回复 `Manifest{transfer, folder, items}`。下载的 key 可以为空（`host.rs:399-402`）。
   - 单个文件：`folder = ""`，`items = [名称]`。
   - 目录：`folder` 为目录名，`items` 为其下全部条目（`storage.rs:187-240`）。
4. 请求 `DownloadStart{transfer: T, items: [ItemPlan{}] × 条目数}`，回复 `Accepted{offsets}`。条目数不符回 PROTOCOL（`transfer.rs:557-570`）。
5. 被控在 bulk 上按清单顺序发数据块（`transfer.rs:599-653`）。**超过 4 MiB 的文件必须回 FLOW**：`FLOW{channel: 1, stream: T, credit: 累计, kind: 1}`，也可以一次给足整个大小（`transfer.rs:101-108`）。60 s 收不到 FLOW，被控就以失败结束（`transfer.rs:44-61`）。
6. **完成**：收到最后一个条目的 `last` 块。成功时被控不另发消息，只有失败时才发 `ItemDone`（`transfer.rs:120-134`）。源文件的大小或修改时间变了会回 CHANGED（`storage.rs:241-256`）。
7. **取消**：`Cancel{transfer: T}`，回复 `Done`。

### 4.6 浏览器侧的约束与建议做法

- **上传**：
  - `<input type=file multiple>` 或拖放，按 `File.slice` 流式读取，内存占用只有一个块。
  - 文件夹上传用 `webkitdirectory`，`webkitRelativePath` 作为 `Item.path`（以 `/` 分隔；`folder` 取顶层目录名）。
- **下载**：没有本地路径，按以下优先级处理：
  1. `showSaveFilePicker()`（Chromium）：在用户点击时先选好保存位置，再发 Download，用 `FileSystemWritableFileStream.write()` 边收边写。收到块后要等写入完成才回 FLOW，这样被控自然受到背压。
  2. OPFS（`navigator.storage.getDirectory()` 加 `createWritable`）作中转，完成后 `URL.createObjectURL(await handle.getFile())` 交给 `<a download>`。数据在磁盘上，不占内存。
  3. 退路：在内存里收集 `Blob` 片段。只适用于小文件，界面要限制大小，例如 ≤ 1 GiB。
- 所有 uint64 字段用 BigInt。
- 最小实现可以不做 Exists、续传、MakeDir、Rename、Remove。

### 4.7 字节示例

FILE 中 tag ≥ 16 的字段用两字节键：20=`a2 01`、21=`aa 01`、23=`ba 01`、24=`c2 01`、25=`ca 01`、26=`d2 01`、30=`f2 01`。

| 消息 | 字节 |
|---|---|
| 列出根（rid 1） | `46 57 02 00 04 01 00 00 00 01` + `0a 00` |
| Upload（rid 2）：T 1，dir ":/Default"，a.txt 5 字节，conflict 2，key "k1" | `46 57 02 00 04 01 00 00 00 02` + `32 1e 08 01 12 09 3a 2f 44 65 66 61 75 6c 74 22 09 0a 05 61 2e 74 78 74 10 05 28 02 32 02 6b 31` |
| Plan 回复 | `46 57 02 01 04 01 00 00 00 02` + `ba 01 04 08 01 12 00` |
| UploadStart（rid 3）：offsets [0] | 帧体 `3a 05 08 01 12 01 00` |
| Done 回复 | `46 57 02 01 04 01 00 00 00 03` + `aa 01 00` |
| 数据块 "hello"，last | `46 57 02 00 04 02 00 00 00 00` + `08 01 10 01 22 05 68 65 6c 6c 6f 28 01` |
| ItemDone OK（entry 0） | `46 57 02 00 04 01 00 00 00 00` + `f2 01 02 08 01` |
| Download（rid 4）：T 2，`C:\a.txt` | 帧体 `42 0c 08 02 12 08 43 3a 5c 61 2e 74 78 74` |
| DownloadStart | 帧体 `4a 04 08 02 12 00` |
| Accepted{[0]} | 帧体 `ca 01 03 0a 01 00` |
| FLOW：T 2，5 MiB | `46 57 02 00 0a 01 00 00 00 00` + `08 01 10 02 18 80 80 c0 02 20 01` |

---

## 5. 麦克风

### 5.1 原生的做法

- **收发器**：建 peer 时就用 `add_transceiver_from_track(Opus 48k/2 声道, sendrecv)` 绑定 `audio_0` 轨道（`src/transport/rtc/peer.rs:182-203`）。
  - fmtp 为 `minptime=10;stereo=1;useinbandfec=1`，PT 111，带 transport-cc（`src/transport/rtc/negotiation.rs:160-191`）。
  - 不换轨，也不重新协商；不说话时就是不写 RTP。
- **编码参数**取自 answer（被控把 offer 的 Opus fmtp 原样回显）：立体声、带内 FEC、无 DTX，默认 128 kbps，20 ms（`src/media/audio/encoder.rs:13-18`、`:49-79`）。
- **采集**：原始设备，没有回声消除、降噪、自动增益（`src/media/audio/dsp.rs:6`），重采样到 48 kHz、每 10 ms 一帧（`src/media/microphone.rs:643-712`）。
- **何时发送**：只在以下条件都满足时才采集和发送（`src/media/microphone.rs:81-92`、`:231-281`）：
  - 用户开启；
  - 被控回复成功（code 0）；
  - 收到 `MicrophoneState ACTIVE`。

  关闭时先停发，再发 `policy=0`（`src/features/stream_control/microphone.rs:46-82`）。
- **与控制权无关**（`src/features/stream_control/input.rs:334`）。断线或重连后，原生只发一次清理用的 `policy=0`，用户要重新开启（`stream_control/microphone.rs:84-95`）。

### 5.2 SESSION 消息（session 通道）

```proto
message PolicyRequest   { int32 policy = 1; }      // tag 16，请求，rid ≠ 0；bit0 = 开启
message PolicyResponse  { int32 error_code = 1; }  // tag 22，回复
message MicrophoneState { int32 state = 1; }       // tag 3，推送：1 ACTIVE、2 INACTIVE、3 RESTART
```

- 被控取 `policy & 1` 作为开关（`publisher.rs:16-28`）。只在 session 通道处理（`channels.rs:361-390`）。rid 为 0 时不回复。
- **错误码**（`src/protocol/microphone.rs:3-17`）：

  | 码 | 名称 | 含义 |
  |---|---|---|
  | 0 | — | 成功 |
  | `0x0e003001` | COMPONENT_UNAVAILABLE | 虚拟声卡驱动没装或打不开 |
  | `0x0e003006` | NOT_ALLOWED | 没有 `microphone` 授权、许可失效或未连接 |
  | `0x0e003012` | INITIALIZATION_FAILED | 其他错误，包括 3 s 超时（`src/features/host/microphone.rs:63-101`、`:210-224`） |

- **MicrophoneState 何时推送**：只在 HELLO 双向完成后，并且状态变化时（`src/features/host/peer/channels.rs:513-519`、`:581-585`、`:706-732`）。
  - 从未开启过时不推送。
  - **ACTIVE 的意思是被控上有程序打开了“Farwing 虚拟麦克风”的采集流**，不是“正在播放主控声音”（`microphone.rs:581-588`）。
  - 推送可能早于 PolicyResponse 到达。
- **字节**：

  | 消息 | 字节 |
  |---|---|
  | 开启（rid 1） | `46 57 02 00 02 01 00 00 00 01` + `82 01 02 08 01` |
  | 关闭（rid 2） | 帧体 `82 01 00` |
  | 成功回复 | `46 57 02 01 02 01 00 00 00 01` + `b2 01 00` |
  | NOT_ALLOWED 回复 | 帧体 `b2 01 05 08 86 e0 80 70` |
  | 状态推送 | `46 57 02 00 02 01 00 00 00 00` + `1a 02 08 0{1,2,3}` |

- AUDIO（0x0202 JSON）只管桌面声音的音质，与麦克风无关（`src/protocol/audio_control.rs`）。

### 5.3 被控

- **门控**：`microphone` 授权 + 许可有效 + PeerConnection 已连接（`src/features/host/microphone.rs:264-269`）。不需要 HELLO 特性 `audio.microphone`（只声明，不检查），也不需要控制权。
- **收轨**：`on_track` 只接受 Opus/48000、1–2 声道的音频轨。msid、轨道 id、PT 都不检查（`src/features/host/peer/mod.rs:471-501`）。
  - `on_track` 在第一个 RTP 包到达时触发。新的轨道会取代旧的。
- **SSRC 是硬性要求**：被控的 webrtc-rs 只为 offer 中**声明了 `a=ssrc:`**、方向不是 recvonly 的媒体段建立接收（`vendor/webrtc/src/peer_connection/sdp/mod.rs:86-96`、`:164-212`）。
  - 未声明的 SSRC 只在 offer 只有一个媒体段时才能处理（`vendor/webrtc/src/peer_connection/peer_connection_internal.rs:1030-1039`）；否则记录“on_track will not be fired”（`:405-419`）。
  - **浏览器 offer 的音频段必须带 `a=ssrc`**。
- **收包条件**：同一代、被控上的采集流正在运行（ACTIVE）、已开启、有许可，否则丢弃。包长 ≤ 65535 字节，时长 10–120 ms（`src/features/host/microphone.rs:239-256`）。
  - 每位参与者各有一个 NetEq，混音后经软限幅写入 `\\.\FarwingAudioBridge`（`microphone.rs:413-424`、`:698-721`，`src/platform/windows/virtual_audio.rs:17-88`）。
- **驱动**：可选组件“Farwing Virtual Audio”（`src/platform/windows/virtual_audio/install.rs:1-16`），在被控端的组件页安装。没装时开启请求回 COMPONENT_UNAVAILABLE（`microphone.rs:69-96`）。
- **多人**：所有有授权的人可以同时说话、混音（`microphone.rs:1-8`）。
- **断开**：PeerConnection 离开 Connected、session 通道关闭、回复发送失败时，开关会被关掉（`src/features/host/peer/mod.rs:543-554`，`channels.rs:154-156`）。**重连后要重新发 `policy=1`**。
- 被控不为收到的音频回 transport-cc 反馈（`src/features/host/peer/mod.rs:178-180`），浏览器的音频码率估计拿不到反馈。

### 5.4 浏览器要做的事

1. **首个 offer**：
   - 把音频收发器改为 `pc.addTransceiver("audio", { direction: "sendrecv" })`。
   - 生成 offer 后检查音频段里有 `/a=ssrc:\d+/`。Chrome 和 Safari 对没有轨道的 sendrecv 收发器通常也会写 ssrc，以实测为准。没有就先 `replaceTrack` 一条静音轨（`new AudioContext().createMediaStreamDestination().stream.getAudioTracks()[0]`），再 `createOffer`。
   - 继续保留本地 SDP 里的 `stereo=1`（`server/web/src/lib/sdp.ts:39-45`）。`sendrecv` 不影响桌面声音：被控只在 sendonly 或 inactive 时关闭桌面声音（`src/media/audio/encoder.rs:139-149`）。
2. **开启**：
   1. `getUserMedia({audio: {echoCancellation, noiseSuppression, autoGainControl, channelCount: {ideal: 2}, sampleRate: 48000}})`。原生不做回声消除。浏览器同时在播放被控声音，建议开启 `echoCancellation`，这是产品选择。
   2. 在 session 通道发 `microphone_policy{1}`。
   3. 等到 `error_code 0` 和 `MicrophoneState 1` 都收到（顺序不定），再 `sender.replaceTrack(mic)`。
   4. `sender.setParameters({encodings: [{maxBitrate: 128000}]})`，与原生一致。被控没有反馈，码率要显式固定。
3. **状态变化**：
   - INACTIVE（2）：`replaceTrack(null)`；被控此时本来也会丢包。
   - RESTART（3）：保持轨道，继续发送。
   - 非 0 错误码：显示对应原因（`src/protocol/microphone.rs:7-17`）。
4. **关闭**：`replaceTrack(null)`，停止轨道，发 `policy{0}`。
5. 不需要重新协商，也不需要控制权。

---

## 6. 触摸

- TOUCH（0x0104）与 PEN（0x0105）只保留了类型号，没有定义帧体（`crates/protocol/src/peer/mod.rs:90-93`，`docs/protocol/peer-messages.md:727`）。特性名 `touch` 已定义，但没有一方声明或检查（`crates/protocol/src/peer/mod.rs:76-78`，`src/protocol/peer.rs:322`）。
- 被控收到 TOUCH 帧会静默忽略：control 通道上只当作 HELLO 去解析，不是 HELLO 就丢弃（`src/features/host/peer/ingress.rs:39-97`，`src/features/stream_control/publisher.rs:380-383`）；session 通道上按 SESSION 解析后丢弃（`publisher/displays.rs:112-116`）。
- **建议用鼠标模拟**（Pointer Events，画面元素设 `touch-action: none`）：

  | 手势 | 发送 |
  |---|---|
  | 单指轻点 | `MouseAbsolute` + 左键按下、抬起 |
  | 长按约 500 ms | 右键按下、抬起 |
  | 单指拖动（移动超过 8 px 才开始） | 左键按下，持续发 `MouseAbsolute`，最后抬起 |
  | 双指拖动 | `MouseWheel`（像素换算同 §2.7） |
  | 双指轻点 | 右键 |
  | 捏合 | 只在本地缩放画面（CSS transform），不发送 |

  可另提供“触控板模式”：本地维护一个虚拟光标位置，仍用绝对坐标发送。

---

## 7. 授权：让网页主控与原生同权，并保留服务器总开关

### 7.1 现状：所有裁剪点

| 位置 | 现在做什么 |
|---|---|
| `crates/protocol/src/lib.rs:156-163` | `Platform::Web.controller_caps_limit() = [view, audio]` |
| `server/src/http/sessions.rs:123-130` | 账号会话按上述常量裁剪；裁剪后不含本次用途所需能力时回 403 “网页只能观看” |
| `src/transport/signal/host.rs:744-781` | 被控先按关系算出授权（本机设备 = 全部；好友 = 服务端给的；受信任设备 = 本机记录；访客 = 本机的访客模板），再按 `controller.platform` 裁剪 |
| `src/features/host/assist.rs:228-229`、`:265-274`、`:556` | 访客请求带 `view_only`（由平台推出）；“允许并信任”时保存的授权也被裁剪 |
| `src/application/app/notifications.rs:454-455` | 被控确认弹窗的文案“网页端只能观看和收听” |

**现有漏洞**：好友改授权时，服务端 `regrant` 直接推送新授权（`server/src/hub.rs:868-916`），被控 `SESSION_PERMISSIONS` 原样 `set_grants`（`src/transport/signal/host.rs:578-591`）。两处都不按平台裁剪，所以网页好友会话可能在会话中途得到 `control`。改造时一并修正。

### 7.2 建议的改法

1. **协议 crate**：给 `ControllerInfo` 加字段：

   ```rust
   #[serde(default, skip_serializing_if = "Option::is_none")]
   pub caps_limit: Option<Vec<String>>,   // 服务端规定的能力上限
   ```

   - `ControllerInfo` 已经是 `#[serde(default)]`（`crates/protocol/src/realtime.rs:259-271`），加字段不影响现有字段的解析。
   - 它同时出现在 `session.incoming`（`realtime.rs:288-309`）和 `assist.join`（`realtime.rs:454-460`），所以账号会话和访客/受信任会话都能带上服务端的限制。
   - `controller_caps_limit()` 改为只作回退：`Web` 并且没有 `caps_limit`（旧服务端）时，仍按 `[view, audio]` 处理。（实施时如此；2026-10-19 这一回退与 `controller_caps_limit()` 已删除，上限只来自服务端的 `caps_limit`。）
2. **服务端**：
   - 在线设置 `RuntimeSettings`（`server/src/settings.rs:9-72`）加 `web_caps: Vec<String>`，“网页主控可用的能力”。管理后台“服务器设置”页与 `GET/PUT /api/v2/admin/settings`（`server/src/http/admin_api.rs:780-835`）同步增加。
   - 推荐默认值 `["view","control","clipboard","files","audio","microphone","annotation"]`，即浏览器能实现的全部；`ports`、`diagnostics`、`display` 浏览器用不到，默认不放行。管理员可以改回 `["view","audio"]`，即只看。
   - `device_controller_info()`（`server/src/http/sessions.rs:73-84`）：设备平台为 Web 时，填 `caps_limit = Some(设置值)`。它被账号会话和访客流程共用（`server/src/http/assist.rs:210`、`:342`），一处改动全覆盖。
   - `sessions.rs:125` 改为按 `caps_limit` 裁剪；不满足用途时的提示改为“服务器未允许网页端使用该功能”。
   - `hub.regrant`（`server/src/hub.rs:868-916`）对每个会话再按 `s.controller.caps_limit` 求交集（会话保存了 `controller`，`hub.rs:105-127`）。
   - 可选：设置变化后，给现有的网页会话推一次 `session.permissions`。
3. **被控**：
   - `host.rs:760` 改为：上限 = `incoming.controller.caps_limit`（实施时没有它就回退到 `controller_caps_limit()`，这一回退已于 2026-10-19 删除）。
   - 把这个上限记在参与者上，`SESSION_PERMISSIONS`（`host.rs:578-591`）也要按它求交集。
   - `assist.rs` 的 `view_only: bool` 改为 `limit: Option<Vec<String>>`：`trust()` 按上限保存；弹窗显示“网页端可用：…”。
   - 访客与受信任会话的授权仍由被控模板决定（`host.rs:750-757`，`src/features/host/access.rs:1270-1272`；访客默认模板为 `view, control, clipboard, audio, annotation`，`crates/protocol/src/lib.rs:211`），再与服务端上限求交集。服务端的限制就这样作用到了“被控做主”的会话。
4. **版本**：不提供与旧版本的兼容（Farwing 尚未发布；以后需要兼容时另行设计），服务端与被控须使用同一版本。服务端对网页主控总是带 `caps_limit`，被控只按它裁剪，没有按平台的回退。

   浏览器必须以 PERMISSIONS 帧为准，按实际授权显示或隐藏功能（观看简报 §4.10）。访客会话 ack 中的 `grants` 为空（`server/src/http/assist.rs:346`），只能等 PERMISSIONS。
5. **文档**：`docs/web-console.md` 的 §1“键鼠控制不做”与 §7.1、§7.3 的“只能观看”要同步改。

---

## 8. 风险与浏览器限制

| 风险 | 说明 | 绕过办法 |
|---|---|---|
| Keyboard Lock 只有 Chromium 支持 | `navigator.keyboard.lock()` 要求顶层上下文和用户激活；要截获 Esc、Win、Alt+Tab 等系统键，要在全屏下使用，并且以操作系统允许为准。Firefox、Safari 不支持。Ctrl+Alt+Del 任何浏览器都截获不了 | 全屏按钮里调用 `requestFullscreen()` 后 `navigator.keyboard.lock()`；较新的浏览器也可以用 `requestFullscreen({keyboardLock: "browser"})`（以运行时检测为准）。不支持时，靠特殊键菜单（§2.8）补齐 Win、Alt+Tab、Ctrl+Alt+Del、锁定、任务管理器。Keyboard Lock 下要长按 Esc 才能退出全屏，界面要提示 |
| 指针锁定 | 需要用户手势；Esc 会退出；`unadjustedMovement` 不是所有平台都支持 | 点击画面进入；监听 `pointerlockchange` 回到绝对模式；`NotSupportedError` 时不带参数重试 |
| 剪贴板权限 | `readText()` 需要权限或用户手势（Chrome 弹权限框，Firefox、Safari 弹粘贴确认）；`writeText()` 要求页面有焦点，部分浏览器还要求用户激活。浏览器监听不到本机剪贴板的变化 | 浏览器 → 被控：窗口 `focus` 时尝试 `readText()`（Chrome 已授权时成功），内容变了就 Offer；另提供“发送本机剪贴板”按钮和 `paste` 事件。被控 → 浏览器：收到 Offer 后立即 Fetch 并缓存，有焦点时 `writeText()`，失败就在工具栏提示“点击复制” |
| 自动同步的条件 | 被控只在自动模式下接受浏览器的 Offer（单人，或多人时自己持有控制权，§3.1） | 多人且不持有控制权时，界面只提供“获取被控剪贴板”（Pull） |
| 大文件占内存 | 下载时用 Blob 收集会占用整份内存 | 依次退到 `showSaveFilePicker` 流式写入、OPFS 中转、Blob（§4.6）；上传用 `File.slice` 流式读取 |
| 移动端键盘与文本 | 虚拟键盘常报 `code: ""`、`keyCode 229`；被控没有文本输入事件 | 用隐藏的 `<input>` 收 `beforeinput`/`input`。ASCII 按美式布局换成 usage（必要时加 Shift）发送，被控为其他布局时会错。非 ASCII 和长文本走剪贴板：先 Offer 文本，再发 Ctrl+V（需要 `clipboard` 授权，被控收到 Ctrl+V 后会来 Fetch） |
| 输入法组字 | 焦点在可编辑元素上时，本地输入法会截获按键 | 焦点放在不可编辑元素上；`isComposing` 为真的事件不转发（§2.4） |
| AltGr 与布局 | Windows 浏览器报 Ctrl+右 Alt；其他系统只报右 Alt | 按物理键原样转发，由被控布局解释；问题键提供菜单 |
| macOS 的锁定键与 Cmd | macOS 上 `getModifierState("NumLock")` 恒为假，发 LockState 会把被控的 NumLock 关掉；CapsLock 在 Mac 上只在开启时报 keydown、关闭时报 keyup；Cmd 报为 `MetaLeft/MetaRight` | macOS 上不发 LockState，把每个 CapsLock 事件当作一次完整的按下抬起；提供“Cmd 映射为 Ctrl”的选项（默认按物理键发 Win） |
| 按键重复与卡键 | 重复的速率由本机系统决定；后台标签页的计时器被节流后，心跳会停 | 转发 `e.repeat`；有键按住时 15 ms 检查一次，≥ 50 ms 发心跳；`blur`、`visibilitychange` 时统一释放（§2.4）。被控 150 ms 起的超时是最后一道保护 |
| control 通道的队头阻塞 | 浏览器没有 FCT，丢包重传时后续输入要等待；队列超过 64 条会让被控释放全部按键（§2.3） | 按 `bufferedAmount` 合并移动（§2.1）；弱网时提示 |
| 错误没有反馈 | 被控对输入、SAS 失败只在本地记录 | 系统命令菜单写明前提（被控服务、系统策略） |
| PrintScreen、媒体键 | 浏览器或系统常常截走这些键，收不到 keydown | 放进特殊键菜单 |
| CSS 光标尺寸 | 部分浏览器对大于 128×128 的光标图片不生效 | 回退到画面上叠一个 `<img>` 跟随指针，或打开被控的光标合成 |
| 麦克风回声 | 浏览器同时播放被控声音，关闭回声消除会把声音送回被控 | 默认开启 `echoCancellation` |
| 麦克风 SSRC | 浏览器 offer 不带 `a=ssrc` 时被控永远收不到 | 生成 offer 后检查，缺失就先挂静音轨（§5.4） |

---

## 9. 文档与代码的出入（以代码为准）

| 文档说法 | 代码事实 |
|---|---|
| `v2-design.md` §5.2：HELLO 在 session 通道；CLIPBOARD 的“数据走 TRANSFER” | HELLO 在 control 通道交换（观看简报 §4.4）；剪贴板数据 ≤ 32 KiB 时内联在 `Data` 中（`src/features/clipboard/wire.rs:14-15`） |
| `v2-design.md` §7、`peer-messages.md`：`MouseRelative` 单位是“被控像素” | 被控按相对鼠标事件或 HID 报告注入，会叠加被控的指针加速；原生发送的是原始鼠标计数（`engine.rs:520-543`，`windows_mouse.rs:449-455`） |
| `peer-messages.md`：“不认识的 usage 被忽略” | 合法页内不认识的 usage 被忽略；页不对或 id 为 0 是解析错误，记在被控本地（`src/features/host/input/wire.rs:104-110`） |
| `v2-design.md` §8.2：超过 64 KiB 分片 | CLIPBOARD 片长 60 KiB（`src/protocol/peer.rs:35`）；FILE 用自己的分片器，片长 65526 字节（`src/features/file_transfer/wire.rs:17`） |
| `v2-design.md` §8.2：请求停滞后总会发 Abort | 只有 Data 请求停滞时才发（`src/features/clipboard/mod.rs:1019-1029`） |
| `v2-design.md`、`peer-messages.md`：上传半成品名中的 `{key}` | 那是被控自己的记录 UUID，不是主控给的 key（`src/features/file_transfer/storage.rs:467-472`，`host/transfer.rs:189`） |
| `peer-messages.md`：`PeerError` 回复按 PROTOCOL 处理 | 原生按错误码映射到不同的文件错误（`src/features/file_transfer/wire.rs:611-632`） |
| `v2-design.md` §5.2：AUDIO 负责“音质与麦克风控制” | 只负责桌面声音音质（`src/protocol/audio_control.rs`） |
| `peer-messages.md`：MicrophoneState ACTIVE = “正在播放主控麦克风” | 表示被控上有程序打开了虚拟麦克风的采集流（`src/features/host/microphone.rs:581-588`） |
| 观看简报 §3.1、§5.3：被控不检查音频的 msid/ssrc | msid 确实不检查；但要**接收**主控麦克风，offer 必须声明 ssrc（§5.3）。`farwing-protocol.md` 的“即使不发声也带 msid/SSRC”反而是对的 |
| 观看简报 §3.3：bulk 通道可选；§4.4.1：不要声明 `clipboard*`、`files`、`audio.microphone`；§4.6：不发 `control_turn`、`microphone_policy` | 只适用于只看的网页。控制版需要 bulk 通道，并且要声明 `clipboard` |
| `web-console.md` §1、§7.1：网页只看不控 | 本文改为按授权控制（§7） |
| 现有实现的缺口 | 好友改授权时服务端和被控都不按平台裁剪（§7.1） |

---

## 10. 实现清单（按顺序）

1. **授权改造（服务端 + 协议 crate + 被控）**，按 §7.2：
   1. `ControllerInfo.caps_limit`；
   2. `RuntimeSettings.web_caps` 及管理页；
   3. `sessions.rs` 和 `regrant` 求交集；
   4. 被控在 `session.incoming`、`SESSION_PERMISSIONS`、访客信任三处按上限裁剪；
   5. 修改弹窗文案。

   做完后验证：网页会话的 PERMISSIONS 里出现 `control`、`clipboard`、`files`、`microphone`。
2. **编解码补齐**：`pb.ts` 增加 sint32、double、“值为 0 也要写出的整数”；`fpp.ts` 增加类型号、MORE 分片拼接（FILE 与 CLIPBOARD 各自的上限）、`PeerError` 解析。
3. **offer 调整**：音频改为 `sendrecv` 并检查 `a=ssrc`；保留 5 个 DCEP 通道（bulk 必需）；HELLO 加 `clipboard`、`files`。
4. **控制权**：在 session 通道收发 ControlTurn；跟踪 PARTICIPANTS 中的 `you` 和 `controlling`；收到 `held=true` 后才转发输入；处理控制权被收回；重连后用 `automatic` 自动恢复。
5. **键盘**：`code` → usage 表（§2.4）；按下、抬起、重复；按住的键集合；统一释放的顺序；15/50 ms 心跳；LockState 与 `seq`（macOS 例外）；焦点放在不可编辑元素上。
6. **鼠标**：
   1. 绝对坐标映射，屏幕 id 取自 Screens；
   2. 按键与滚轮的换算；
   3. 按 `bufferedAmount` 合并移动；
   4. 指针锁定的相对模式，配合 `cursor_capture` 切换；
   5. CSS 光标。
7. **特殊键菜单与全屏 Keyboard Lock**：系统命令 1–5，Win 和 Alt+Tab 等组合键。
8. **触屏模拟鼠标**（§6）；移动端文本经剪贴板加 Ctrl+V 输入。
9. **剪贴板文本**：Offer（保留到下一次 Offer）、回答 Fetch（内联，或 bulk 流加信用）；Pull 加 Fetch，大数据流回 FLOW；防回环；处理权限。
10. **文件**：上传（List/`:/Default` → Upload → UploadStart → 数据块加信用 → ItemDone）；下载（List → 登记流 → Download → DownloadStart → 收数据块并回 FLOW → `last`）；取消；文件名清洗；三种保存方式。
11. **麦克风**：`getUserMedia`，`policy{1}` → 收到 0 和 ACTIVE 后 `replaceTrack`，处理 INACTIVE/RESTART，关闭时 `policy{0}`，重连后重新开启，固定码率。
12. **实测项**：
    - Chrome、Firefox、Safari 的 `a=ssrc` 与 Opus 立体声；
    - 各浏览器的滚轮单位；
    - Keyboard Lock 能截获哪些系统键；
    - 被控为不同键盘布局和输入法时的 AltGr、日文、韩文键；
    - 弱网下的输入延迟；
    - 多人场景下控制权的转移。
