# 网页观看端（浏览器，只看不控）实现简报

本文是浏览器观看端（TypeScript，浏览器原生 `RTCPeerConnection`）的实现依据：浏览器要像原生主控一样让被控接受 WebRTC 会话并推流（画面、声音、光标、显示器列表），但不发送任何键鼠输入。所有结论均来自代码阅读，`文件:行` 为 2026-10-17 的代码快照。

- 代码与 `docs/protocol/*.md` 不一致时以代码为准，差异见 §5.3。
- 调研期间 `server/src/ws.rs`、`server/src/http/mod.rs`、`server/src/http/sessions.rs`、`crates/protocol/src/lib.rs`、`src/transport/signal/host.rs` 仍在被修改（新增了浏览器子协议入场、`Platform::Web` 与网页能力裁剪）。这几个文件的行号可能漂移，引用时以函数名为准。
- 两个“hello”不要混淆：实时面服务端 `hello`（JSON，`type:"hello"`）与对端协议 `HELLO` 帧（FPP 类型 `0x0001`，protobuf）。
- 用语：被控 = host；主控 = controller = 浏览器；“wire SDP” 指发给对端的 SDP 文本（可能经过修改），“本地 SDP” 指交给 `setLocalDescription` 的 SDP。

---

## 1. 总览

### 1.1 前置条件（本文不展开，假定已完成）

1. 浏览器拥有 Ed25519 设备密钥，设备 ID = `local_device_id(公钥)`（`crates/protocol/src/crypto.rs:219-241`，见 §2.9.4），且该公钥已在服务端登记为本设备。服务端把它作为 `ControllerInfo.public_key` 下发给被控，被控用它校验 offer 签名（`src/transport/signal/host.rs:681-692`）。
2. 已通过 HTTPS 为目标被控创建会话（`POST /api/v2/sessions`，`purpose: "desktop"`，不带 `presence`），拿到 `SessionTicket{session_id, ticket:"fwt_…", endpoints:[wss://…/api/v2/ws], connect_timeout_ms, retry_delay_ms, target{device_id, public_key, …}, bitrate_limit_mbps}`（`crates/protocol/src/api.rs` `SessionTicket`）。
   - 会话必须在入场券有效期（60 s，`crates/protocol/src/lib.rs:82` `TICKET_TTL_SECONDS`）内连上 WS，否则服务端以 `expired` 结束会话（`server/src/hub.rs:1028-1036` `sweep`）。
   - 服务端对 `platform = web` 的主控把授权裁剪为 `view` + `audio`（`server/src/http/sessions.rs:123-130`，`crates/protocol/src/lib.rs:158-163` `controller_caps_limit`）。被控收到 `session.incoming` 后按 `controller.platform` 再裁剪一次（`src/transport/signal/host.rs:759-766`）。（这是调研时的代码；后来改为服务端按“网页端可以使用的功能”下发 `ControllerInfo.caps_limit`，被控只按它裁剪，`controller_caps_limit` 已删除，见 [web-control-brief.md](web-control-brief.md) §7 与 `docs/web-console.md` §7.1。）

### 1.2 时序（从 WebSocket 连上到第一帧）

```
浏览器                                   服务器 (hub)                                 被控 (host)
  |-- GET /api/v2/ws  Sec-WebSocket-Protocol: farwing.v2, ticket.fwt_… -->|
  |<- 101  Sec-WebSocket-Protocol: farwing.v2 ------------------------------|
  |<- {"type":"hello", role:"controller", session_id, resume_token, …} -----|
  |-- session.start {id, sid, attempt_id, options, capability} ------------>|
  |                                          |-- session.incoming {…} ------------------->|  校验、准备显示与编码（几百毫秒到数秒）
  |<- ack {participant_id, session_id, ice_servers, host{device_id,public_key}, grants} -|
  |  校验 host 公钥 ↔ 设备 ID；new RTCPeerConnection；createOffer；签名
  |-- rtc.offer {gen:1, attempt_id, restart:false, network:3, sdp} ------->|--------------------------->|  未准备好时缓存（early_rtc）
  |-- rtc.candidate × N ---------------------------------------------------->|--------------------------->|
  |                                          |<-- session.setting{kind:"capability"} ----|  可能早于 answer；可忽略
  |                                          |<-- rtc.answer {gen:1, sdp} ---------------|  先验 offer 签名，再应答并签名
  |<- rtc.answer --------------------------------------------------------------|
  |  验 answer 签名 → setRemoteDescription → ontrack(audio_0, video_0..4)
  |<- rtc.candidate × N -------------------------------------------------------|<--------------------------|
  |================ ICE / DTLS / SCTP（P2P 或 TURN）========================================================|
  |================ DCEP：5 个数据通道 open ===============================================================>|
  |<====== control：HELLO 请求（flags=0, rid=0）=============================================================|  被控 control 通道 open 时发送
  |====== control：HELLO 请求（rid=0），每 500 ms 重发直到收到响应 ========================================>|
  |<====== control：HELLO|RESPONSE ==========================================================================|  → 被控 handshake_answered
  |====== control：HELLO|RESPONSE（回答被控的请求）==========================================================>|  → 被控 handshake_complete
  |<====== session：PERMISSIONS、SESSION{Screens}、AUDIO{hello}、SESSION{Quality}、PARTICIPANTS …
  |<====== control：CURSOR …
  |<~~~~~~ RTP：video_0（H.264/AV1，从关键帧开始），audio_0（Opus）
  |  <video> 出首帧
  |<- ping（每 heartbeat_ms=20 s）--|  |-- pong -->|
  |-- bye -->|  服务端以 controller_left 结束会话并以 1000 关闭 WS
```

### 1.3 出首帧所需的最少动作

1. 连 WS，回应每个 `ping`。
2. 发 `session.start`：`options.decoders` 与 `capability.video_codec_capability` 都不能为空，且至少有一种格式与被控编码器相交（§2.5）。
3. offer 包含 1 条 audio（sendrecv 或 recvonly）、5 条 recvonly video、带内 DCEP 数据通道（至少 `farwing.control.v2` 与 `farwing.session.v2`），会话级带 `a=x-farwing-identity`。offer 中**不得**出现 `a=x-farwing-fct`。
4. 验证 answer 的签名后安装 answer 与对端候选。
5. 在 control 通道上完成**双向** HELLO：发出自己的请求，并回答被控的请求。
6. 不需要任何 SESSION 请求。被控只有在 HELLO 双向完成、并把 Screens 发出之后才开始采集与发送（`src/features/host/peer/channels.rs:514-519`、`:619-638`；`src/features/host/peer/capture_worker.rs:374-380`）。

---

## 2. 每一步的确切消息与校验点

### 2.1 WebSocket 建立

**浏览器怎么做**

```ts
const ws = new WebSocket(ticket.endpoints[0], ["farwing.v2", `ticket.${ticket.ticket}`]);
// 断线恢复：new WebSocket(url, ["farwing.v2", `resume.${resumeToken}`])
```

- 地址取入场响应的 `endpoints`，形如 `wss://<public_url 主机与路径>/api/v2/ws`（`server/src/config.rs:549`）。原生主控依次尝试各个 endpoint（`src/transport/signal/transport.rs:612-626`、`:658-662`）。
- 浏览器不能设置请求头。服务端把子协议 `ticket.<入场券>` / `resume.<恢复令牌>` 当作凭据（`server/src/ws.rs:44-58` `BROWSER_PROTOCOL`/`offered`，`:68-69`），并选中 `farwing.v2` 回给浏览器（`server/src/ws.rs:105` `ws.protocols([BROWSER_PROTOCOL])`）。连上后应检查 `ws.protocol === "farwing.v2"`。
- 子协议列表含 `farwing.v2` 时，服务端跳过 `X-Farwing-Protocol` 头检查（`server/src/http/mod.rs:245-256` `browser_ws`）。原生客户端发送的是 `X-Farwing-Protocol: 2` 与 `X-Farwing-Ticket` 头（`src/transport/signal/transport.rs:998-1015`）。
- 入场券为 `fwt_` + 43 个 base64url 字符（`crates/protocol/src/ids.rs:34-42`），单次有效，60 s 过期（`server/src/hub.rs:419-427`）。

**服务端校验与失败表现**

| 情况 | 结果 | 位置 |
|---|---|---|
| 入场券无效或过期 | HTTP 401（浏览器只看到握手失败） | `server/src/ws.rs:70-97` |
| 入场券对应的会话已结束 | 升级后以关闭码 4004 关闭 | `server/src/hub.rs:540-543` |
| 同一会话已有主控连接 | 旧连接以 4001 关闭，新连接接管 | `server/src/hub.rs:544-549` |
| 二进制帧 | 4008 关闭 | `server/src/ws.rs:193-196` |
| 不是 JSON 对象 / 缺 `v` / 信封无效 | 4008 关闭 | `server/src/hub.rs:1062-1074` |
| `v != 2` | 4009 关闭 | `server/src/hub.rs:1066-1069` |
| 单帧文本 > `max_frame_bytes`（1 MiB） | 回 `error{payload_too_large}`，不关闭 | `server/src/ws.rs:181-187` |
| 超过 200 条/秒（突发 400） | 4029 关闭 | `server/src/ws.rs:177-180`，`crates/protocol/src/lib.rs:72-74` |
| 60 s 内没有收到任何帧 | 以 1001 “idle timeout” 关闭 | `server/src/ws.rs:207-210` |
| 不认识的 `type` | 回 `error{bad_request,"unknown message type"}`，不关闭 | `server/src/hub.rs:1149-1152` |

WS 消息上限为 `max_frame_bytes × 2`（`server/src/ws.rs:98-107`）。

### 2.2 信封（所有实时面消息）

```json
{"v":2,"type":"rtc.offer","id":"c-12","re":null,"sid":"<session uuid>","gen":1,"data":{}}
```

- 定义见 `crates/protocol/src/realtime.rs:12-28`。`id`、`re`、`sid`、`gen` 为空时省略。`data` 为 null 时省略；原生在 `bye` 等消息中发送 `"data":{}`。
- 浏览器发出的消息必须带 `"v":2`。需要应答的请求带 `id`，服务端以 `{"type":"ack","re":<id>,"data":…}` 或 `{"type":"error","re":<id>,"data":{"code","message","retryable"[,"details"]}}` 回复（`crates/protocol/src/realtime.rs:54-63`，`crates/protocol/src/error.rs` `ErrorBody`）。`session.start` 必须带 `id`；`rtc.*` 可带可不带（带了会得到 `ack {}`，`server/src/hub.rs:1459-1461`；原生不带）。
- `id` 只需在本连接内唯一，原生用 `"c-1"`、`"c-2"`…（`src/transport/signal/mod.rs:404-408`）。
- 消息类型常量：`crates/protocol/src/realtime.rs:77-109`。

### 2.3 服务端 `hello`（S→C，连接后的第一帧）

```json
{"v":2,"type":"hello","data":{
  "conn_id":"<uuid>","role":"controller","epoch":7,"session_id":"<sid>",
  "resume_token":"fwr_…","heartbeat_ms":20000,"idle_timeout_ms":60000,
  "limits":{"max_frame_bytes":1048576,"max_sdp_bytes":1048576,"max_candidates":256,"rate_per_s":200},
  "resumed":false,"server_time_ms":1760000000000}}
```

- 结构见 `crates/protocol/src/realtime.rs:158-175`，由 `server/src/hub.rs:444-460`（`hello`）与 `:534-571`（`attach_controller`）发出。
- 浏览器保存 `session_id`（后续所有会话消息的 `sid`）、`resume_token`、`heartbeat_ms`、`idle_timeout_ms`。
- 原生在 10 s 内收不到 `hello` 即重连（`src/transport/signal/transport.rs:30`、`:762-764`），并要求 `resume_token` 合法（`:810-812`）。收到 `hello` 之前不要发送 `session.start`（原生会等待它，`src/transport/signal/mod.rs:227-244`）。

### 2.4 心跳

- 服务端每 `heartbeat_ms` 发送 `{"v":2,"type":"ping","data":{"t":<unix ms>}}`（`server/src/ws.rs:200-205`）。
- 浏览器必须回 `{"v":2,"type":"pong","data":{"t":<同一个 t>}}`。原生的做法是把收到的信封改成 `type:"pong"` 原样回发（`src/transport/signal/transport.rs:826-829`）。服务端收到任何帧都会重置 60 s 的空闲计时（`server/src/ws.rs:174`），不检查 pong 的内容。
- 客户端在 `heartbeat_ms + idle_timeout_ms`（至少 5 s）内收不到任何帧，即判定连接失效（`src/transport/signal/transport.rs:738-741`、`:813`）。
- 浏览器也可以主动发 `ping`，服务端会回 `pong`（`server/src/hub.rs:1086-1089`）。

### 2.5 `session.start`（C→S，必须带 `id`）

**推荐的完整消息**

```json
{"v":2,"type":"session.start","id":"c-1","sid":"<hello.session_id>","data":{
  "attempt_id":"<crypto.randomUUID()，小写>",
  "purpose":"desktop",
  "options":{
    "screen_id":-1,
    "capture":{"fps":2,"quality":5,"cursor_capture":true,"resolution_type":3,
               "local_resolution":{"width":2560,"height":1440},"chosen_resolution":null,
               "chroma":1,"bitrate":0,"hdr":false,"auto_quality":2,"fps_count":60,
               "weak_network":0,"cinema":false,"codec":0},
    "decoders":[{"fps":60,"codec":1,"width":3840,"height":2160,"chroma":1}],
    "version":"web-0.1.0"
  },
  "capability":{
    "sdr_10bit":false,"session_id":"","display_info":[],
    "video_codec_capability":[
      {"video_codec":1,"width":3840,"height":2160,"chroma_sampling":1,"bit_depth":8,"codec_impl":32}
    ]
  }
}}
```

**结构定义**

- `SessionStart{attempt_id, options, capability, purpose?}`：`crates/protocol/src/realtime.rs:200-212`。`options` 必须出现（没有 `serde(default)`）；`capability` 缺省为 null。
- `ConnectOptions` / `CaptureParams` / `DecoderCapability`：`crates/protocol/src/session.rs:33-92`，全部字段 `serde(default)`。
- `capability` 服务端不解析，原样转给被控，被控按 `DeviceCapability` 解析：`src/protocol/capability.rs:12-44`。
- 原生的构造方式：`src/session/negotiation.rs:54-173`（`build_session_start` / `connect_options`）；能力探测：`src/media/decoder/mod.rs:474-569`。

**服务端校验**（`server/src/hub.rs:1157-1304` `on_session_start`）

| 检查 | 失败回复 |
|---|---|
| 控制连接上 `sid` 若出现必须等于本连接的会话 | `error{bad_request,"sid does not match this connection"}`（:1163-1170） |
| 必须带 `id` | `error{bad_request,"session.start requires id"}`（:1193-1200） |
| `data` 能解析为 `SessionStart` | `error{bad_request,"invalid session.start"}`（:1201-1207） |
| `attempt_id` 长度 1..=64 | `error{bad_request,"invalid attempt_id"}`（:1208-1211） |
| 会话仍存在 | `error{not_found,"session ended"}`（:1212-1215） |
| 被控在线且仍是会话创建时的 `epoch` | `error{device_offline,"目标设备不在线"}`（:1216-1224） |
| 游客被控只接受 `assist` 类型的会话 | `error{forbidden,…}`（:1225-1232） |

通过后，服务端记录 `attempt_id`、清零候选计数，向被控推送 `session.incoming`（purpose、类型、关系、授权与主控身份都取自服务端，`options`/`capability` 原样转发，`:1263-1279`），并向浏览器回 ack（`:1280-1295`）。

**ack（`SessionStarted`，`crates/protocol/src/realtime.rs:240-257`）**

```json
{"v":2,"type":"ack","re":"c-1","data":{
  "participant_id":"<uuid>","session_id":"<sid>",
  "ice_servers":[{"urls":["stun:x:3478"],"username":"","credential":""},
                 {"urls":["turn:x:3478?transport=udp","turns:x:5349"],"username":"<exp>:<sid>","credential":"<b64>"}],
  "force_relay":false,
  "host":{"device_id":"<16 位 base32>","public_key":"<base64url 32 字节>"},
  "relation":"own","grants":["view","audio"]}}
```

ICE 服务器的构造见 `server/src/ice.rs:88-106`。

原生收到 ack 后的校验（浏览器照做；`src/transport/signal/mod.rs:443-467`）：

1. 10 s 内必须收到 ack（`REQUEST_TIMEOUT_MS`，`crates/protocol/src/lib.rs:105`）。
2. `session_id` 是 UUID，`participant_id` 非空。
3. `host.device_id` 合法，并且 `local_device_id(host.public_key) == host.device_id`（`src/transport/signal/identity.rs:88-106` `check_key`/`check_trust`）。不相等时视为冒充，终止连接。

**被控读取的字段与推荐值**

被控处理 `session.incoming` 的代码在 `src/transport/signal/host.rs:608-990`（`incoming`）。

| 字段 | 被控怎么用 | 浏览器取值 |
|---|---|---|
| `attempt_id` | 被控要求是 UUID（`host.rs:623-628`；服务端只要求 1–64 字符） | `crypto.randomUUID()` |
| `options.screen_id` | 首位参与者：-1 = 主屏，其他值必须是存在的屏幕 ID，否则准备失败（`src/features/host/displays/initial.rs:240-246`）。后续参与者：≥0 时按 ID，否则沿用首位参与者的屏幕，再否则主屏（`initial.rs:57-75`）。缺省为 **0**，不是 -1 | **必须显式写 -1** |
| `capture.fps` | 档位 1=30 2=60 3=90 4=144，其他值 = 30（`src/features/stream_control/publisher.rs:341-354`） | 2 |
| `capture.fps_count` | 帧率上限，>0 时 `fps = min(档位, fps_count)`（`publisher.rs:317-323`） | 60（或屏幕刷新率） |
| `capture.quality` / `auto_quality` / `bitrate` | 1..=6，5 = 自动（自动档 2..4，缺省 2）；6 = 自定义码率（bit/s）（`publisher.rs:324-331`、`:355-377`） | 5 / 这台被控上次报告的自动档（缺省 2）/ 0 |
| `capture.codec` | 0 = 被控选择；1 / 2 / 5 = 双方都支持时会话从这种编码开始，换屏幕时也保留（`VideoConfig.requested_codec`） | 菜单里选过的编码，没选过为 0 |
| `capture.cursor_capture` | 画面是否合成光标（`publisher.rs:335`） | **true**（只看不控时被控光标的位置只能从画面里看到，见 §4.8） |
| `capture.resolution_type` | 0..=4。**2 会把被控物理分辨率改成 `local_resolution`**（或进入独占屏），且初始准备阶段不检查 `display` 能力；4 要求 `chosen_resolution`（`initial.rs:31-37`、`:247-293`） | **3**（跟随被控分辨率） |
| `capture.local_resolution` | 只在被控没有可用显示器、需要自动创建虚拟屏，或开启了独占屏时决定尺寸，缺省 1920×1080（`initial.rs:150-185`） | 浏览器屏幕物理像素，取整 |
| `capture.chroma` / `hdr` | 3 = 4:4:4；`hdr` = 请求 HDR（`src/features/host/desktop.rs:253-264`） | 1 / false |
| `capture.weak_network` / `cinema` | 弱网策略；观影模式（质量优先） | 0 / false |
| `decoders[]` | `Negotiated::new`：`codec` 与 `chroma`（2..=4 原样，其他值视为 1，`publisher.rs:219-226`）必须与某个 `video_codec_capability` 行一致，`width`/`height` ≥ 2；`fps` 按档位换算（≥117→144，75–116→90，45–74→60，其余 30，`publisher.rs:228-237`）。为空时拒绝（`host.rs:717-722`） | 每种 codec 一条，与 capability 对应 |
| `version` | 只记录日志 | 任意 |
| `capability.video_codec_capability` | 不能为空（`host.rs:717-722`）。`(video_codec, chroma_sampling, bit_depth)` 必须与被控某个编码器格式完全一致，`width`/`height` ≥ 2（`src/features/host/format.rs:91-118`）。`DualCapability::negotiate` 取 min(编码器, 解码器) 后找不大于它的画质档 {1280×720, 1920×1080, 2560×1440, 3840×2160}；比较方式是 **(宽, 高) 元组的字典序**，低于 720p 则该行无效（`src/protocol/capability.rs:46-47`、`:82-185`）。AV1（`video_codec` = 5）只有双方都列出时才参与 | H.264：`{1, 3840, 2160, 1, 8, 32}`；可选 AV1：`{5, 3840, 2160, 1, 8, 37}`（见 §3.4） |
| `capability.*.codec_impl` | 37 = 软件解码，其他值视为硬件解码（`format.rs:120`）。只影响被控对候选的排序（`src/media/selection.rs:13-34`：尺寸 > 硬件 > 首选格式 > 帧率 > …）以及 `preferred_hardware` | H.264 写 32，软件 AV1 写 37 |
| `capability.sdr_10bit` | 允许 10 bit SDR（`format.rs:172`、`:324`） | false |
| `capability.preferred_codec` | 1、2、5 之一，覆盖被控的首选设置（`format.rs:86-89`） | 省略，或写 1 |
| `capability.display_info` / `session_id` | 被控不读主控的这两项 | `[]` / `""` |

**被控对 `session.incoming` 的其余校验**

任何一项失败，被控都会发 `session.end`：失败信息含“上限”时原因为 `host_closed`，否则为 `media_failed`（`host.rs:534-556`）。浏览器随后收到 `session.ended{reason}` 与关闭码 4004。

- 被控仍接受连接；`sid` 是 UUID；本会话还没有参与者（**同一 `sid` 再次 `session.start` 会因“会话已存在”被以 `media_failed` 结束**，`host.rs:618-621`）；`participant_id` 非空且 ≤ 256 字符；`attempt_id` 是 UUID（`:609-628`）。
- 主控设备 ID 合法，并且 `local_device_id(controller.public_key) == controller.device_id`（`:647-692`）。
- `capability` 能解析为 `DeviceCapability`（`:696-702`，`"缺少主控解码能力"`）。
- 会话类型、`connect_type` 合法，且解码能力非空（`:712-722`）。
- 授权经网页裁剪后仍包含本次 purpose 所需的能力（desktop/audio 需要 `view`，`:759-780`）；关系与连接类型一致（`:767-774`）。
- 未超过参与者上限、被控不在更新或执行电源操作（`src/features/host/access.rs:1549-1557` `peer_lease`）。
- 媒体准备（异步，`host.rs:846-969`）：显示准备（`initial.rs:15-53`）→ 编码能力 → `Negotiated::new` 必须有交集（`format.rs:161-165`，`"没有共同的画面编码/解码能力"`）→ `apply` 必须选出格式（`format.rs:378-379`，`"请求的画面格式没有共同能力"`）。失败时以 `media_failed` 结束（`host.rs:386-394`）。

### 2.6 创建 RTCPeerConnection

```ts
const pc = new RTCPeerConnection({
  iceServers: ack.ice_servers.map(s => s.username ? s : { urls: s.urls }), // 空用户名不传
  iceTransportPolicy: ack.force_relay ? "relay" : "all",
  bundlePolicy: "max-bundle",
  rtcpMuxPolicy: "require",
});
```

- 原生在收到 ack 之后才建 peer（`src/session/controller/mod.rs:501-565`）；`force_relay` 时只用中继（`:534-538`；`src/transport/rtc/peer.rs:146-150`）。
- 收发器、数据通道与 SDP 的细节见 §3。

### 2.7 rtc.offer（C→S→H）

```json
{"v":2,"type":"rtc.offer","sid":"<sid>","gen":1,"data":{
  "attempt_id":"<同 session.start>","restart":false,"network":3,"sdp":"v=0\r\n…a=x-farwing-identity:<sig>\r\nm=audio …"}}
```

- 定义：`crates/protocol/src/realtime.rs:324-333`（`RtcDescription`）；网络取值：`:311-322`（3 = 全部候选，1 = 只用中继，2 = 全部（备用值），0 = 用于候选消息）。原生的首个 offer：`gen = 1`，`restart: false`，`network: 3`（`src/transport/signal/mod.rs:509-527`、`:545-547`）。
- 服务端校验（`server/src/hub.rs:1306-1462` `on_relay`）：必须带 `sid`（:1308-1311）；发送方是本会话的主控（:1320-1333）；主控只能发 `rtc.offer`/`rtc.candidate`（:1335-1350）；会话已经 `session.start`（:1351-1354，`conflict`）；`attempt_id` 等于当前尝试（:1362-1365，`conflict "stale attempt_id"`）；SDP ≤ 1 MiB（:1366-1369）；被控在线且 epoch 不变（:1414-1432，`device_offline "peer not connected"`）。转发时去掉 `id`/`re`，保留 `sid`、`gen`（:1449-1458）。
- 被控校验（`src/transport/signal/host.rs:992-1047` `rtc`）：
  - `attempt_id` 等于自己记录的值，否则静默丢弃。
  - `gen != 0 && gen <= 已处理的值` 时视为过期，丢弃。
  - SDP 非空且 ≤ 1 MiB。
  - **身份签名必须有效**，否则以 `identity_mismatch` 结束会话（:1014-1018）。
  - `answer()`（`src/features/host/peer/mod.rs:983-1048`）：可选的 `a=x-farwing-fct` 必须能解析；必须有 `a=ice-ufrag`；SDP 能解析；可选的 Opus 参数。
  - 被控的媒体还没准备好时，offer 与候选先进入 `early_rtc` 缓存（最多 256 条、8 MiB，`host.rs:558-577`）。

### 2.8 rtc.answer（H→S→C）

```json
{"v":2,"type":"rtc.answer","sid":"<sid>","gen":1,"data":{"attempt_id":"…","restart":false,"network":3,"sdp":"…"}}
```

浏览器的处理（照搬原生，`src/transport/signal/negotiation.rs:112-139`，`src/transport/signal/mod.rs:643-645`、`:714-728`）：

1. `sid` 不是本会话、或 `attempt_id` 不等于本次尝试时忽略。
2. `gen` 不为 0 且不等于当前 offer 的代次时忽略。
3. SDP 非空且 ≤ 1 MiB。
4. **验证 answer 的身份签名**（§2.9）。失败即终止：原生直接结束本次连接（`IdentityFailure`，`mod.rs:705-707`）；浏览器应关闭 PC 并发送 `bye`。
5. `setRemoteDescription({type: "answer", sdp})`。原生在 answer 被拒绝时不立即终止，而是等待后续 answer 或连接超时（`mod.rs:721-724`）。

answer 中有浏览器不认识的会话级属性（`a=x-farwing-identity`、`a=x-farwing-nat`），浏览器会忽略。被控在 answer 中加入 `a=extmap-allow-mixed`、`a=msid-semantic: WMS audio_0 video_0 … video_4`、`a=max-message-size:524288`；只有 offer 带 `a=x-farwing-fct` 时，answer 才会带它（`src/transport/rtc/core.rs:96-115`，`src/features/host/peer/mod.rs:1039-1042`）。

### 2.9 DTLS 身份绑定（`a=x-farwing-identity` / `a=x-farwing-pake`）

代码：`src/transport/signal/identity.rs:102-193`，`crates/protocol/src/crypto.rs:38-48`、`:110-117`、`:136-145`、`:219-273`。

#### 2.9.1 指纹集合

```ts
function fingerprints(sdp: string): string[] {
  const set = new Set<string>();
  for (const raw of sdp.split("\n")) {
    const line = raw.replace(/\s+$/, "");                 // 对应 trim_end（含 \r）
    if (line.startsWith("a=fingerprint:"))
      set.add(line.slice("a=fingerprint:".length).trim().toLowerCase());
  }
  return [...set].sort();                                  // 按字节序（ASCII）排序、去重
}
```

- 取 SDP 中**全部** `a=fingerprint:` 行（会话级与媒体级都算），值包含算法名，例如 `sha-256 4a:1b:…`。转小写、排序、去重（`identity.rs:155-165`）。
- 至少要有一个指纹，否则拒绝（`identity.rs:111`、`:132`）。

#### 2.9.2 被签名的字节

```
"FWDTLS2\n" + session_id + "\n" + attempt_id + "\n" + role + "\n" + fingerprints.join(",")
```

- 以 UTF-8 编码（`crypto.rs:138-145`）。
- `session_id` = ack 的 `session_id`；`attempt_id` = 本次 `session.start` 的 `attempt_id`。
- `role`：浏览器签自己的 offer 时用 `"controller"`；验证被控的 answer 时用 `"host"`（`identity.rs:23-37`、`:133-137`）。

#### 2.9.3 签名、插入与验证

- 签名：Ed25519（设备私钥）对上述字节签名，得到 64 字节，编码为 **base64url 无填充**（86 个字符，`crypto.rs:38-48`、`:84-86`）。
- 插入：在 wire SDP 的**第一个 `m=` 行之前**（会话级）加入一行 `a=x-farwing-identity:<sig>`，换行风格与原 SDP 一致（`identity.rs:109-127`、`:176-193`）。
- 签名基于“将要发出的 SDP”中的指纹计算：本地 SDP 与 wire SDP 的指纹相同即可。被控也只看指纹，不关心其他行。
- 验证 answer：取第一行 `a=x-farwing-identity:` 的值（trim），base64url 解码为 64 字节；用 `ack.host.public_key`（base64url 解码为 32 字节）做 Ed25519 验证，消息按上面的公式、`role = "host"` 计算（`identity.rs:130-152`）。被控端用的是 `verify_strict`（`crypto.rs:110-117`），WebCrypto 或 `@noble/ed25519` 产生的标准签名都满足。
- 每一个 offer（包括 ICE 重启的 offer）都要重新签名，被控每次都验证（`host.rs:1012-1018`）。

#### 2.9.4 设备 ID 推导（`local_device_id`，`crypto.rs:219-241`）

`base32_lower(SHA-256(公钥 32 字节))` 的前 16 个字符。字母表为 `abcdefghijklmnopqrstuvwxyz234567`，按大端位流每 5 位取一个字符，也就是摘要前 10 字节的 base32、无填充。浏览器用它校验 `ack.host`；被控用它校验浏览器（`host.rs:692`）。

#### 2.9.5 访问码访客（可选，本期不需要）

使用访问码（SPAKE2，密钥 K）的会话还要求 `a=x-farwing-pake:<mac>`：

- `mac = base64url(HMAC-SHA256(K, "FWDTLS2\n" + session_id + "\n" + "" + "\n" + role + "\n" + fps))`，即 `attempt_id` 位置为空串（`crypto.rs:256-273`）。
- 被控在 `relation = assist` 且本机持有 PAKE 密钥时要求并校验它（`host.rs:655-680`，`identity.rs:120-125`、`:145-150`）。
- 仅确认模式（没有输入访问码）不带这一行。

### 2.10 rtc.candidate（双向）

```json
{"v":2,"type":"rtc.candidate","sid":"<sid>","gen":1,"data":{"attempt_id":"…",
  "candidate":{"candidate":"candidate:1 1 udp 2122260223 192.168.1.5 54321 typ host …","sdpMid":"0","sdpMLineIndex":0,"usernameFragment":"AbCd"}}}
```

- 定义：`crates/protocol/src/realtime.rs:335-359`。字段名正是 `RTCIceCandidate.toJSON()` 的输出。
- **发出**：
  - 不发 `null` 或 `candidate === ""` 的结束候选（原生会跳过空候选，`src/transport/signal/mod.rs:617-619`）。
  - `sdpMid` 为空时原生补 `"0"`（`negotiation.rs:166-182`）。
  - 信封带当前 `gen`（`mod.rs:629-639`）。
  - 服务端限制：每个候选序列化后 ≤ 2 KiB；每次尝试、每个方向 ≤ 256 个（`server/src/hub.rs:1371-1402`）。
- **被控收到后**：按 ufrag 归属；offer 还没到时先缓存，最多 256 个（`src/features/host/peer/mod.rs:1049-1073`）。被控的 ICE 默认接受 mDNS（`.local`）候选（`vendor/webrtc-ice/src/mdns/mod.rs:16-26`，默认 `QueryOnly`）。
- **收到被控候选**：`attempt_id` 不符时忽略（`negotiation.rs:140-145`）。answer 安装之前收到的候选，原生会丢弃（`mod.rs:742-744`）；**浏览器应缓存，等 answer 安装后再 `addIceCandidate`**。被控只发送带 ufrag、属于当前本地描述的候选，`sdpMid` 缺失时按 m-line 序号补全（`peer/mod.rs:945-982`）。
- 原生的协商时限：从发出 offer 到 PeerConnection `connected` 为 12 s（`mod.rs:34`、`:550-579`）。被控首次准备媒体可能需要数秒（编码器探测有磁盘缓存，`src/features/host/desktop.rs:33-82`），浏览器建议放宽到 15–20 s。

### 2.11 其他 S→C 消息

| type | data | 处理 |
|---|---|---|
| `session.setting` | `{kind:"capability", capability: DeviceCapability}` / `{kind:"sender_info", infos:[{video_track_index, capture_impl, encoder_impl}]}` | 可全部忽略。`capability` 是被控编码能力，可用于界面展示，且可能早于 answer 到达（`host.rs:372`）。 |
| `session.permissions` | `{grants:[…]}`（`realtime.rs:403-408`） | 只用于展示；生效的能力以对端 `PERMISSIONS` 帧为准 |
| `session.ended` | `{reason}`，原因见 `realtime.rs:385-401` | 会话结束，随后以 4004 关闭（`server/src/hub.rs:810-817`） |
| `error` | `ErrorBody`，`re` 对应请求的 `id` | 见 §2.5、§2.7 |

### 2.12 结束

- **主动离开**：发 `{"v":2,"type":"bye","data":{}}`，再关闭 WS。服务端以 `controller_left` 结束会话，通知被控，并以 1000 `"bye"` 关闭（`server/src/hub.rs:1113-1122`、`:782-784`；原生做法见 `src/transport/signal/transport.rs:932-953`）。同时关闭 PeerConnection。
- **被动结束**：服务端发 `session.ended{reason}` 后以 4004 关闭。原因包括：被控 `session.end` 只会转成 `revoked`、`kicked`、`identity_mismatch`、`media_failed`、`host_closed` 之一（`hub.rs:1516-1531`）；另有 `replaced`（同一设备新建了会话）、`host_offline`、`permission_revoked`、`expired`、`server_shutdown`（关闭码 1001）。
- **关闭码**（`crates/protocol/src/realtime.rs:111-129`）：
  - 1000、4001、4003、4004、4009、4401：不重连。
  - 1001、4008、4029、1006 等：可以用恢复令牌重连（`close::reconnectable`）。

### 2.13 恢复（可选）

1. **实时连接断开但 PC 仍通**：用 `["farwing.v2", "resume.<resume_token>"]` 在 300 s 内重连（`server/src/hub.rs:573-605`）。服务端回 `hello{resumed:true}`，**不重放**断线期间的消息；原生按退避 `retry_delay_ms × 1.5^n`、最多 25 s 重试（`transport.rs:628-663`）。如需长时间保持，每 30 s 检查一次，令牌超过 300 s 未刷新时发 `{"v":2,"type":"resume.refresh","id":"r-1","data":{}}`，ack 返回 `{resume_token}`（`transport.rs:769-779`，`hub.rs:1091-1112`）。
   - 没有恢复令牌时，可以申请同一会话的新入场券（`POST /api/v2/sessions/{sid}/ticket`，`server/src/hub.rs:723-749`）后重连。**不要**再发 `session.start`，否则被控会以“会话已存在”结束会话（§2.5）。
2. **ICE 断开**：原生在 `disconnected` 持续 2 s 后做 ICE 重启，最多 3 次（`src/transport/signal/mod.rs:837-841`、`:1046-1080`、`:1094-1104`）。浏览器的做法：`pc.restartIce()` → `createOffer` → 重新签名 → `rtc.offer{restart:true, network:3, gen:+1}`，只接受 `gen` 相同的 answer。被控对 `restart:true` 的 offer 会重新应用 ICE 策略（`peer/mod.rs:910-944`、`:1000-1005`）。
3. **会话丢失**（`host_closed`、`host_offline`、媒体失败、网络错误）：原生在 5 分钟内以 1、2、4、8、15 s 退避**新建会话**（重新 `POST /sessions`），被控离线时每 3 s 轮询其状态（`src/session/controller/mod.rs:95-164`，`src/session/controller/viewing.rs:302-332`、`:547-622`）。`kicked`、`revoked`、`permission_revoked`、`identity_mismatch` 不恢复。同一设备的新会话会替换旧会话（`server/src/hub.rs:651-665`）。

---

## 3. SDP 要求与浏览器实现

### 3.1 原生 offer 的结构（参照）

代码：`src/transport/rtc/peer.rs:177-221`、`src/transport/rtc/core.rs:74-94`、`src/transport/rtc/negotiation.rs:73-139`、`:160-330`。

```
v=0 …
a=group:BUNDLE 0 1 2 3 4 5 6
a=extmap-allow-mixed                         ← 本地 SDP 没有时，在 wire SDP 中补
a=msid-semantic: WMS audio_0                 ← 替换为这一行（被控不检查）
a=x-farwing-nat:mapping=…;filtering=…;delta=…   ← 可选（core.rs:92，§7.2）
a=x-farwing-identity:<sig>                   ← 必需
m=audio … 111                (mid 0, a=sendrecv, msid audio_0 audio_0 + ssrc)
   a=rtpmap:111 opus/48000/2, a=fmtp:111 minptime=10;stereo=1;useinbandfec=1, rtcp-fb transport-cc
   extmap: transport-wide-cc, sdes:mid
m=video … (mid 1..5, a=recvonly，共 5 条)
   AV1 104/rtx 105 (profile=0;level-idx=17;tier=0)、AV1 106/rtx 107 (profile=1…)、
   H265 96/rtx 97、H264 98/rtx 99 (level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f)、
   red 100/rtx 101、ulpfec 102、flexfec-03 35、x-farwing-rsfec 36
   rtcp-fb：goog-remb transport-cc ccm fir nack nack pli rrtr
   extmap（7 项）：transport-wide-cc、sdes:mid、video-timing、urn:farwing:rtp-hdrext:sending-delay、color-space、
                 urn:farwing:rtp-hdrext:capture-index、urn:farwing:rtp-hdrext:new-frame
m=application … UDP/DTLS/SCTP webrtc-datachannel (mid 6)
   a=sctp-port:…  a=max-message-size:524288  a=x-farwing-fct:1   ← 浏览器不得带 fct
```

- 原生把 library 生成的 SDP 原样装入本地描述，只在 wire 副本上加 Farwing 属性（`core.rs:84-93`）。浏览器照此办理：可以只修改 wire 副本，Opus stereo 例外（§3.4）。
- 原生代码注释说“没有 MSID/SSRC 的 audio_0 不被视为桌面主控”（`peer.rs:182-185`），但当前被控代码**不检查** audio 的 msid 或 ssrc：全仓库搜索 `audio_0`/`msid` 只有发送侧和主控侧的命中。被控只读 audio m-line 的方向与 Opus 参数（`src/media/audio/encoder.rs:127-178`）。

### 3.2 被控如何匹配浏览器 offer（决定能否出画面）

被控的媒体引擎注册（`src/features/host/peer/mod.rs:181-364`）：

- Opus 111：`minptime=10;stereo=1;useinbandfec=1` + transport-cc。
- H.264 98：`level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f`，RTX 99（**总是注册**）。
- H.265 96/97、AV1 104/105、AV1 106/107：**只有被控编码器支持、且浏览器 capability 也声明了该编码**时才注册；106 还要求 4:4:4。
- `x-farwing-rsfec` 36。

扩展头：音频注册 transport-cc、mid；视频注册 video-timing、sending-delay、transport-cc、mid、color-space、capture-index、new-frame。

webrtc-rs 的匹配规则（`vendor/webrtc`）：

- **H.264**（`src/rtp_transceiver/fmtp/h264/mod.rs`）：`packetization-mode` 必须相等，`profile-level-id` 的前两个字节（profile_idc 与约束字节）必须相等，level 不比较。浏览器必须提供 `profile-level-id=42e01f;packetization-mode=1` 这一项；`42001f`、`4d001f`、`64001f` 都不算精确匹配。Chrome、Edge、Safari 和带 OpenH264 的 Firefox 默认都会提供 42e01f pm=1。
- **其他编码**（`fmtp/generic/mod.rs:40-62`）：双方都有的键必须相等。AV1 另外要求 `profile` 相等（缺省视为 0）。**Chrome 新版的 AV1 fmtp 形如 `level-idx=5;profile=0;tier=0`，与被控的 `level-idx=17` 冲突，因此不是精确匹配**（以实际 SDP 为准）。
- **精确匹配优先**：某种媒体只要有精确匹配，就**只采用精确匹配**，部分匹配（只有 mime 相同）全部丢弃；而且只看**第一个**视频 m-section（`src/api/media_engine/mod.rs:722-768`）。因此在 Chrome 上，H.264 精确匹配时 AV1 会被丢掉，除非修改 AV1 的 fmtp（§3.4）。
- **RTX**：`apt` 必须指向已匹配的编码（`media_engine/mod.rs:588-655`）。
- **Opus**：Chrome 的 `minptime=10;useinbandfec=1` 与被控一致，是精确匹配。
- VP8、VP9、red、ulpfec、flexfec、G.722 等都不匹配，被忽略。

收发器配对（`src/rtp_transceiver/mod.rs:486-516` `satisfy_type_and_direction`）：

- 被控先 `add_track` 加入 audio（`peer/mod.rs:404-417`），再按 0..4 的顺序加入 5 条视频（`src/features/host/peer/screens.rs:297-330`），方向都是 sendrecv。
- 远端为 recvonly 时，本地取 sendonly 或 sendrecv 的收发器；远端为 sendrecv 时，本地取 recvonly 或 sendrecv 的收发器。所以浏览器的 audio 用 sendrecv 或 recvonly 都可以；5 条 recvonly 视频依次对应 `video_0..video_4`。
- **以 answer 中的 msid 为准**：每条视频 m-section 都有 `a=msid:video_N video_N`（`src/features/host/track.rs:26-33`）。原生从 answer 的 `msid` 与 `ssrc … msid:` 行解析出可用的轨道序号（`src/transport/rtc/peer.rs:543-571`）。

发送侧：

- 被控按协商出的负载类型写 RTP，所以浏览器自己的 PT 编号没有问题（`track.rs:47-88`）。
- 只写协商过的扩展头（`src/features/host/peer/media.rs:294-423`）。capture-index、new-frame、sending-delay 浏览器无法协商，被控就不写，不影响解码。
- 当协商出的 SDP 不允许被控初始选定的格式时，采集线程会按 SDP 重新选择格式（`src/features/host/peer/capture_worker.rs:102-119`；`src/features/host/format.rs:219-254` `bind_codecs`/`permits_format`）。所以 capability 写了 AV1、SDP 却没有 AV1 时，会自动退回 H.264。
- 没有协商 RS-FEC 时不发修复包（`src/features/host/transport.rs:486-498`），丢包只靠 NACK/RTX。
- SCTP 单条消息上限取 `min(浏览器的 a=max-message-size, 524288)`（`vendor/webrtc/src/sctp_transport/mod.rs:416-425`）。

### 3.3 数据通道（带内 DCEP，不预协商）

| 标签（`crates/protocol/src/peer/mod.rs:22-32`） | `RTCDataChannelInit` | 浏览器是否需要 |
|---|---|---|
| `farwing.control.v2` | `{ordered: true}` | **必需**（HELLO、CURSOR；声明 `display.config` 时 Screens 也走这里） |
| `farwing.session.v2` | `{ordered: true}` | **必需**（SESSION、AUDIO、HOST_EVENT、PERMISSIONS、PARTICIPANTS；Screens 默认走这里） |
| `farwing.bulk.v2` | `{ordered: true}` | 可选（建了也只会忽略） |
| `farwing.tunnel.v2` | `{ordered: true}` | 可选 |
| `farwing.dgram.v2` | `{ordered: false, maxRetransmits: 0}` | 可选 |

- 定义见 `src/transport/rtc/channels.rs:619-639`（`data_channel_init`）。原生另设了 priority（control/bulk 高、session 中、其余低），被控不读取。
- **不能用 `negotiated: true`**：被控本身不创建任何通道（全仓库只有主控 `src/transport/rtc/peer.rs:217` 调用 `create_data_channel`），只在 `on_data_channel`（DCEP）时按标签绑定（`src/features/host/peer/mod.rs:728-761`，`src/features/host/peer/channels.rs:52-115`）。预协商的通道永远不会触发 `on_data_channel`，被控收不到 HELLO，也就不会推流。
- `protocol` 用缺省的空串，`binaryType = "arraybuffer"`。**只发二进制**（`ArrayBuffer`/`Uint8Array`），不要发字符串。
- 原生按 `DATA_CHANNEL_LABELS` 的顺序创建 5 个通道（`peer.rs:214-221`）。被控不要求 bulk、tunnel、dgram 存在，但建议照建，与原生保持一致。

### 3.4 浏览器生成等价 offer 的方法

```ts
const audioTx = pc.addTransceiver("audio", { direction: "sendrecv" }); // 不加 track；用 "recvonly" 也可以
const videoTx = Array.from({ length: 5 }, () => pc.addTransceiver("video", { direction: "recvonly" }));

// 可选：把视频编码限制为 H.264 42e01f pm=1（+ AV1）+ RTX，减小 SDP 并确保精确匹配
const caps = RTCRtpReceiver.getCapabilities("video")!.codecs;
const keep = caps.filter(c =>
  (c.mimeType === "video/H264" && /profile-level-id=42e01f/i.test(c.sdpFmtpLine ?? "")
                               && /packetization-mode=1/.test(c.sdpFmtpLine ?? "")) ||
  (WANT_AV1 && c.mimeType === "video/AV1") ||
  c.mimeType === "video/rtx");
videoTx.forEach(t => t.setCodecPreferences(keep));

const LABELS = ["farwing.control.v2", "farwing.session.v2", "farwing.bulk.v2", "farwing.tunnel.v2", "farwing.dgram.v2"];
const dcs = Object.fromEntries(LABELS.map(l => {
  const dc = pc.createDataChannel(l, l === "farwing.dgram.v2" ? { ordered: false, maxRetransmits: 0 } : { ordered: true });
  dc.binaryType = "arraybuffer";
  return [l, dc];
}));

const offer = await pc.createOffer();
const local = addOpusStereo(offer.sdp!);           // 见下：Opus fmtp 追加 stereo=1;sprop-stereo=1
await pc.setLocalDescription({ type: "offer", sdp: local });
let wire = pc.localDescription!.sdp;
if (WANT_AV1) wire = stripAv1LevelIdx(wire);       // 仅 wire：删除 AV1 fmtp 中的 level-idx=…，使其与被控精确匹配
wire = wire.replace(/^a=x-farwing-fct:.*\r?\n/gm, "");   // 保险：绝不能带 fct
wire = insertSessionAttr(wire, `a=x-farwing-identity:${await signDtls(sid, attemptId, "controller", fingerprints(wire))}`);
send({ v: 2, type: "rtc.offer", sid, gen: 1, data: { attempt_id: attemptId, restart: false, network: 3, sdp: wire } });
pc.onicecandidate = e => { if (e.candidate && e.candidate.candidate) send(candidateEnvelope(e.candidate.toJSON())); };
```

- **Opus 立体声**：被控按 **offer** 中的 `stereo=1` 决定立体声编码（Application::Audio），没有时编码为单声道、Application::Voip（`src/media/audio/encoder.rs:50-80`、`:96-125`）。Chrome 默认 fmtp 不含 `stereo=1`。Chrome 的接收解码按**本地描述**的 fmtp 配置，所以需要在 `setLocalDescription` **之前**的本地 SDP 中加入（这是被广泛使用的 SDP 修改；Firefox 不需要）。被控还读取 `usedtx`、`cbr`、`maxplaybackrate`、`maxaveragebitrate`、`ptime`（缺省 20 ms，`encoder.rs:50-80`）。
- **AV1**：建议首版只声明 H.264。若要 AV1：
  1. capability 与 decoders 都加入 `video_codec` = 5（4:2:0、8 bit）；
  2. wire SDP 中删掉 AV1 fmtp 的 `level-idx=…`，避免与被控的 `level-idx=17` 冲突，使之成为精确匹配；
  3. 被控要有 AV1 编码器才会使用（`src/protocol/capability.rs:85-97`）。

  answer 中 AV1 的 fmtp 是 wire SDP 的回声（被控把远端编码参数放进 answer，`media_engine/mod.rs:743-768`），Chrome 只按 profile 比较 AV1，应能接受。**需要实测**。
- **不要声明** H.265（多数浏览器不能可靠解码）、4:4:4、10 bit、HDR。
- 实施结果：网页端按浏览器的接收能力声明 AV1 profile 0 与 H.265 Main（`profile-id=1`），只在 `getCapabilities` 列出时声明；Chrome 155 实测 AV1、H.265、H.264 都能出画面并能用 `codec_type` 切换（见 `docs/web-console.md` §7.2）。4:4:4、10 bit、HDR 仍不声明。
- 原生会在 wire SDP 中改写 `a=max-message-size:524288`（`negotiation.rs:73-139`）。浏览器**不要**这样改：被控会按浏览器申报的值限制发送，申报的值必须是浏览器真实能收的。
- 原生还加入 `a=msid-semantic: WMS audio_0` 与 `a=extmap-allow-mixed`。前者被控不读；Chrome 默认已带后者（被控对 HDR 色彩元数据或 ID > 14 的扩展会使用两字节扩展头，`media.rs:313`、`:358-365`）。

### 3.5 answer 之后

- **轨道与序号的对应**：解析 answer，取每个 `m=video` 段的 `a=mid` 与 `a=msid:<stream> <track>`，得到 `mid → video_N`。在 `ontrack` 中用 `event.transceiver.mid` 或 `event.streams[0].id`（等于 `video_N`）判断。不要依赖 `track.id`，浏览器可能改写它。
- `ontrack` 在 `setRemoteDescription` 时就会为 5 条视频与 1 条音频触发，但只有被采集的轨道才有 RTP。默认只有 `video_0`（`screens.rs:335` `registered: vec![0]`、`:352-354`）。
- 音频：`audio_0`（mid 0，Opus）。被控只在以下条件同时满足时发送声音：授权含 `audio`、PeerConnection 已连接、offer 的 audio m-line 方向不是 sendonly/inactive、Opus 协商成功（`src/features/host/audio.rs:215-221`、`:307`）。浏览器不需要额外请求。
- 建议把视频与音频放进同一个 `MediaStream` 交给 `<video>`；自动播放策略要求先静音，用户点击后再开声音。可设 `receiver.jitterBufferTarget`（Chrome）降低延迟。

---

## 4. 对端协议（FPP v2）子集

### 4.1 帧格式（`crates/protocol/src/peer/mod.rs:4-13`、`:217-254`）

```
偏移 0  1  | 2      | 3     | 4   5        | 6  7  8  9         | 10 …
     "F" "W" | ver=2 | flags | type u16 BE  | request_id u32 BE  | body
```

- `flags`（`:35-40`）：`0x01` RESPONSE，`0x02` ERROR（帧体为 `PeerError`），`0x04` MORE（分片未完）。
- 请求：`request_id` 非 0（**HELLO 例外：原生用 0**）。响应：同一个 id，置 RESPONSE。推送：id 为 0。
- 接收方先校验：总长 ≤ 524288（`MAX_PEER_FRAME`），≥ 10 字节，魔数为 `FW`，版本为 2；不符则丢弃（`:235-254`）。
- **按帧头类型分发，不看是从哪个通道来的**（`src/transport/rtc/channels.rs:229-336`），未知类型忽略。
- 带 MORE 的分片只出现在 CLIPBOARD、FILE 等请求或响应中（`src/protocol/peer.rs:34-96`），观看端不会遇到；遇到时可以丢弃。
- 帧体编码：大多为 protobuf（prost，proto3 语义：零值与空值不编码，未知字段忽略，枚举按 int32）。`AUDIO`（0x0202）为 UTF-8 JSON。

### 4.2 观看端用到的帧类型（`crates/protocol/src/peer/mod.rs:84-111`）

| 类型 | 值 | 方向 | 被控实际使用的通道 | 观看端动作 |
|---|---|---|---|---|
| HELLO | 0x0001 | 双向 | **control**（`src/features/host/peer/channels.rs:173-219`，`src/features/host/peer/ingress.rs:72-96`） | 必需 |
| ERROR | 0x0002 | — | 任意 | 记录日志 |
| CURSOR | 0x0103 | H→C | control（`src/features/host/peer/cursor.rs`；`src/features/host/peer/participants.rs:97-123`） | 可选：其他参与者的指针 |
| POINTER | 0x0106 | C→H | control | 可选（指示光标，不注入输入） |
| SESSION | 0x0201 | 双向 | session；声明 `display.config` 时 Screens 走 control（`channels.rs:619-625`） | 解析 Screens 等；可选请求 |
| AUDIO | 0x0202 | 双向 | session（`channels.rs:531-543`、`:331-341`） | 可选 |
| HOST_EVENT | 0x0203 | H→C | session（`participants.rs:66-77`） | 提示 |
| PERMISSIONS | 0x0901 | H→C | session（`channels.rs:570-580`） | 记录 |
| PARTICIPANTS | 0x0902 | H→C | session（`participants.rs:78-90`） | 可选 |
| 其他（ANNOTATION hello、DRAG、DIAGNOSTICS Status、CLIPBOARD、FILE、TRANSFER、FLOW、PORT…） | | H→C | session、bulk 等 | 忽略 |

- FCT/KCP（`a=x-farwing-fct`，`src/transport/fct.rs`）只在双方 SDP 都带该属性时启用（`src/transport/rtc/negotiation.rs:141-158`）。浏览器不带，被控的 control 帧就直接走 SCTP（`src/transport/fct.rs:140-154`）。
- TRANSFER/FLOW（bulk）、PORT/PORT_DGRAM（tunnel/dgram）与观看无关，可以不监听这些通道。

### 4.3 protobuf 定义（与 prost 结构逐字段对应，可直接给 protobufjs 或 ts-proto 使用）

```proto
syntax = "proto3";
package farwing.peer;

// crates/protocol/src/peer/mod.rs:256-341
message Profile { uint32 color = 2; }                               // color: 0xRRGGBB，0 = 由被控分配
message Hello {
  uint32 protocol_version = 1;        // 2
  uint32 min_protocol_version = 2;    // 2
  string client_version = 4;
  map<string, uint32> features = 5;   // 特性名 → 1
  Profile profile = 8;                // optional，只有主控填写
}
message PeerError { string code = 1; string message = 2; }

// src/features/stream_control/wire.rs:39-414
message SessionMessage {
  oneof body {
    PbScreenSources screens = 1;                    // H→C 推送
    PbReportQosStats quality = 2;                   // H→C 推送
    MicrophoneState microphone = 3;                 // H→C 推送
    ClipboardPermission clipboard_files = 4;        // H→C 推送
    CaptureControl capture = 5;                     // C→H 推送（不回复）
    ReceiverStats receiver_stats = 7;               // C→H 推送（每秒一次）
    PbCaptureSettingRequest capture_setting = 10;   // C→H 请求 → 20
    // 11..14 为显示拓扑请求（需要 display 能力，观看端不用）
    PbSendVideoTrackRequest video_tracks = 15;      // C→H 请求 → 21
    PolicyRequest microphone_policy = 16;           // C→H 请求 → 22（观看端不用）
    TurnRequest control_turn = 17;                  // C→H 请求 → 23（观看端不用）
    PbCaptureSettingResponse capture_setting_result = 20;
    PbDisplayResult display_result = 21;
    PolicyResponse microphone_result = 22;
    TurnResponse control_turn_result = 23;
  }
}
message PbScreenSources { repeated PbScreen screens = 1; int32 current_screen_id = 2; }
message PbScreen {
  int32 id = 1; int32 fps = 2; repeated PbWinRect resolutions = 3; PbWinRect current_resolution = 4;
  int32 screen_type = 5; PbWinRect init_resolution = 6; bool is_primary_screen = 7;
  PbDpiScale dpi_scale = 9; string display_name = 10; int32 resolution_type = 11;
  int32 video_track_index = 12;                     // 对应 answer 中的 video_N；-1 表示未采集
  PbDisplayGeometry display_geometry = 100;
}
message PbWinRect { int32 left = 1; int32 top = 2; int32 width = 3; int32 height = 4; }
message PbDpiScale { int32 current_dpi = 1; int32 recommended_dpi = 2; repeated int32 dpis = 3; }
message PbDisplayGeometry { uint32 width = 2; uint32 height = 3; }
message PbReportQosStats { string encoder_type = 1; string capture_type = 2; int32 video_quality = 4; }
message MicrophoneState { int32 state = 1; }
message ClipboardPermission { bool enabled = 1; }
message CaptureControl { int32 screen_id = 1; bool start = 2; }
message ReceiverStats { float receive_fps = 1; float decode_fps = 2; float render_fps = 3; float decode_delay_ms = 4; uint32 dropped_frames = 5; }
message PbSendVideoTrackRequest { repeated int32 video_track_index = 1; }   // prost 打包编码（packed）
message PbCaptureSettingRequest {
  int32 fps = 1; int32 frame_quality = 2; bool cursor_capture = 3; int32 screen_id = 4;
  int32 resolution_width = 5; int32 resolution_height = 6; int32 chroma_format = 7; int32 max_custom_bitrate = 8;
  int32 dpi_scale = 9; int32 resolution_type = 10; bool enable_hdr = 11; int32 auto_frame_quality = 12;
  int32 codec_type = 13; int32 max_scale_width = 14; int32 max_scale_height = 15;
  int32 fps_count = 18;
  int32 weak_network = 19; bool cinema = 20;
}
message PbCaptureSettingResponse { repeated PbError errors = 1; }
message PbError { int32 error_code = 1; string error_message = 2; string error_detail = 3; }
message PbDisplayResult { int32 error_code = 1; }                           // display_topology.rs:634-637
message PolicyRequest { int32 policy = 1; } message PolicyResponse { int32 error_code = 1; }  // microphone.rs
message TurnRequest { bool take = 1; bool automatic = 2; } message TurnResponse { bool held = 1; string message = 2; } // control_turn.rs:8-27

// src/features/remote_cursor.rs:156-189；src/protocol/participants.rs:15-120
message CursorMessage { CursorShape shape = 1; repeated ParticipantPointer participants = 2; bool pointers = 3; }
message CursorShape {
  int32 pos_x = 1; int32 pos_y = 2; int32 width = 3; int32 height = 4; bytes byte_value = 5;  // PNG
  int32 cursor_type = 6;              // -1 = 隐藏
  double coordinate_x_scale = 7; double coordinate_y_scale = 8; int32 screen_id = 9;
}
message Click { int32 button = 1; int32 phase = 2; }      // button 1 左 2 右 3 中；phase 1 按下 2 抬起
message Pointer { uint32 seq = 1; int32 screen_id = 2; float x = 3; float y = 4; bool visible = 5; Click click = 6; }
message ParticipantPointer { uint64 id = 1; int32 screen_id = 2; float x = 3; float y = 4; bool visible = 5; uint32 idle_ms = 6; Click click = 7; uint32 click_seq = 8; }
message Participant { uint64 id = 1; string nickname = 2; string device_name = 3; string relation = 4; fixed32 color = 5; bool control = 6; bool you = 7; int64 joined_ms = 8; }
message Participants { repeated Participant participants = 1; uint64 controlling = 2; }

// src/protocol/host_event.rs:10-58
message HostEvent { int32 kind = 1; string actor = 3; }
message Permissions { repeated string grants = 1; }
```

### 4.4 HELLO 交换（观看端必须正确实现）

#### 4.4.1 规则

- **通道**：必须在 `farwing.control.v2` 上。被控只有在 control 通道上回答了主控的请求后，才置 `handshake_answered`（`src/features/host/peer/ingress.rs:79-96`、`:146-166`）。session 通道上的 HELLO 虽然也会被回答（`channels.rs:398-402`），但**不会**置这个标志。
- **主控请求**：原生在 control 通道打开且 PeerConnection 已连接后立即发送，此后每 500 ms 重发，最多 30 次，直到收到 RESPONSE（`src/transport/rtc/channels.rs:367-433`）。帧头 `flags = 0`，`request_id = 0`（`src/protocol/peer.rs:232-234` → `message()` → `framed(kind, 0, 0, …)`）。被控对每个请求都会回复，重复发送无害。
- **被控请求**：被控在 control 通道 `on_open` 时发送一次 HELLO 请求（flags 0，rid 0；`channels.rs:209-218`，`src/features/stream_control/publisher.rs:270-272`）。浏览器必须以 `HELLO|RESPONSE`（flags `0x01`，`request_id` = 收到的 id，即 0）带上自己的 `Hello` 回复，同样走 control 通道（原生：`src/features/stream_control/handshake.rs:441-447`、`:473-477`，`src/protocol/peer.rs:242-244`）。被控收到这个 RESPONSE 才置 `handshake_complete`（`publisher.rs:391-394`，`src/features/host/peer/mod.rs:86-105`）。
- **区分请求与响应只看 RESPONSE 标志，不看 id**。
- **兼容性**：`protocol_version >= 2 && min_protocol_version <= 2`，不满足时对方拒绝握手（`crates/protocol/src/peer/mod.rs:323-327`，`src/protocol/peer.rs:259-276`）。**两个字段都必须写 2**：省略即为 0，会被判为不兼容。
- **门控**：只有 `handshake_complete && handshake_answered` 都为真时，被控才发布状态。而 Screens 发送成功之后，才置 `media_ready` 并开始采集、编码、发送 RTP（`src/features/host/peer/channels.rs:513-519`、`:619-638`；`capture_worker.rs:374-380`）。因此**没有双向 HELLO，就没有画面**。
- **features**：被控只读三项：`display.config`（Screens 改走 control 通道）、`clipboard`/`clipboard.files`（剪贴板级别）、`files`（`publisher.rs:380-411`，`src/features/host/peer/mod.rs:86-105`）。其余特性名只供主控自用。
  - 建议观看端声明：`video.h264`（以及 `video.av1`）、`cursor.shape`、`cursor.participants`、`permissions`、`participants`、`host_event`、`audio.quality`，值都为 1。
  - **不要**声明 `display.config`（Screens 走 session 通道，更简单）、`clipboard*`、`files`、`drag`、`annotation`、`ports.*`、`diagnostics`、`fct`、`audio.microphone`。
  - 原生声明的完整集合见 `src/protocol/peer.rs:155-184`。
- **profile**：被控只读 `profile.color`（≠ 0 时作为本参与者在其他人画面上的指针颜色，`ingress.rs:72-78`）；昵称以服务端为准。
- 时序：被控在 HELLO 双向完成之前，按完整的 v2 主控特性集准备会话（`publisher.rs:174-175`）。

#### 4.4.2 字节示例

最小的主控 HELLO 请求：`features = {"video.h264": 1}`，`client_version = "web"`。

```
帧头: 46 57 02 00 00 01 00 00 00 00
帧体 (25 字节):
  08 02                    protocol_version = 2
  10 02                    min_protocol_version = 2
  22 03 77 65 62           client_version = "web"
  2a 0e                    features 条目 (field 5, len 14)
     0a 0a 76 69 64 65 6f 2e 68 32 36 34   key = "video.h264"
     10 01                                  value = 1
```

回答被控请求时内容相同，只是帧头第 4 字节（flags）为 `01`：`46 57 02 01 00 01 00 00 00 00 …`。

### 4.5 握手后被控主动推送的内容（无需请求）

`publish_state` 每次状态变化或每秒运行一次（`src/features/host/peer/channels.rs:450-676`）。

| 内容 | 帧 | 通道 | 何时 |
|---|---|---|---|
| 生效能力 | PERMISSIONS `{grants}`（`src/protocol/host_event.rs:49-62`） | session | 握手后，以及每次变化（`:570-580`） |
| 桌面声音音质 | AUDIO JSON `{"kind":"hello","revision","kbps","effective_bps"}` | session | 音频已协商且允许时（`:531-543`，`src/features/host/audio.rs:156-168`） |
| 屏幕列表 | SESSION `screens`（tag 1） | 未声明 `display.config` 时走 session | 握手后，以及每次变化；内容相同不重发（`:619-625`；构造见 `src/features/stream_control/publisher/displays.rs:10-85`） |
| 画质 | SESSION `quality`（tag 2）`{encoder_type, capture_type, video_quality}` | session | 编码器就绪后、变化时（`:639-674`） |
| 麦克风状态 | SESSION `microphone`（tag 3） | session | 观看端可忽略 |
| 被控光标 | CURSOR `{shape}` | control | 形状或所在屏幕变化时（`src/features/host/peer/cursor.rs`）。**不是实时位置**，`coordinate_x_scale/y` 只是形状变化时采样到的屏内归一化位置 |
| 参与者列表与控制权 | PARTICIPANTS | session | 列表或持有者变化时（`src/features/host/peer/participants.rs:78-90`） |
| 其他参与者的指针 | CURSOR `{participants, pointers: true}` | control | 最多每 30 ms 一次；只有本人时发一次空列表（`:91-123`） |
| 生命周期事件 | HOST_EVENT | session | 见 §4.10 |
| 其他扩展的 hello | ANNOTATION hello、DRAG hello、DIAGNOSTICS Status | session | 忽略 |

**Screens 的用法**：`current_screen_id` 是当前选中的屏幕；每个 `PbScreen.video_track_index` 给出该屏幕所在的轨道 `video_N`（-1 = 未采集，`displays.rs:77`）；`current_resolution` 为实际尺寸；`display_name`、`is_primary_screen`、`fps`、`dpi_scale` 等可用于界面展示。

### 4.6 SESSION：观看所需的最少交互与可选请求

**出首帧**：不需要任何 SESSION 消息。被控的初始参数来自 `session.start.options.capture`（`src/features/stream_control/publisher.rs:312-340`，`src/features/host/desktop.rs:237-279`）。默认只登记了轨道 0，初始屏幕在 `video_0` 上采集（`src/features/host/peer/screens.rs:335`、`:352-354`）。

被控对 SESSION 的处理在 `src/features/stream_control/publisher/displays.rs:112-387`（`receive_session`）：

- 收到的是响应时忽略（`:117-119`）。
- 除 `Capture` 与 `ReceiverStats` 外，请求的 `request_id` 必须非 0（`:125-126`）。
- 不认识的请求回 `RESPONSE|ERROR{unsupported}`（`:373-381`）。

**可选请求与推送**（`request_id` 由发送方递增，从 1 开始，0 保留给推送；`src/features/stream_control/wire.rs:33-37`）：

1. **切换显示器**（推荐的单轨道做法）：在 session 通道推送 `SESSION{capture: {screen_id: <目标 id>, start: true}}`，rid = 0，被控不回复（`displays.rs:128-146`）。
   - 已登记的轨道只有 `[0]` 时，被控会把 0 号槽改为采集新屏幕（`screens.rs:443-490`：没有空闲槽、只登记了一个 → 复用该槽），画面仍在 `video_0`，从新的关键帧开始，分辨率可能变化。
   - 随后被控重新发布 Screens（`refresh_state`）。
   - 字节（screen_id = 1）：`46 57 02 00 02 01 00 00 00 00 2a 04 08 01 10 01`。
   - `screen_id` 为 int32；负数编码为 10 字节 varint，例如 -1 为 `08 ff ff ff ff ff ff ff ff ff 01`。
2. **原生的多轨道做法**（可选）：先发 `video_tracks{video_track_index: [0,1,2,3,4]}` 请求（rid ≥ 1），等 `display_result{error_code: 0}`（`displays.rs:252-255`，`screens.rs:424-442`）；再按 Screens 中的 `video_track_index` 用 `capture{start: true/false}` 开关各屏幕（原生：`src/features/stream_control/tracks.rs:68-120`、`:177-212`，`src/session/controller/mod.rs:316-331`）。
   - 这种模式下 `capture{start: true}` 打开另一块屏幕会**同时**推送两路画面。单画面观看只用第 1 种做法。
   - 请求字节（rid 1）：`46 57 02 00 02 01 00 00 00 01 7a 03 0a 01 00`（video_track_index = [0]）。
   - 响应：`46 57 02 01 02 01 00 00 00 01 aa 01 00`（`display_result{}`，error_code 0 不编码）。
3. **画面设置** `capture_setting`（tag 10，请求 → tag 20 `capture_setting_result{errors}`；`src/features/stream_control/publisher.rs:417-521`，`displays.rs:256-367`）。原生的写法（`src/features/stream_control/settings.rs:923-948`，常量在 `src/features/stream_control/mod.rs:52-70`）：

   | 字段 | 值 |
   |---|---|
   | `fps` | 1..4 |
   | `frame_quality` | 1..6 |
   | `cursor_capture` | bool |
   | `screen_id` | **-2**（本会话全部轨道） |
   | `resolution_width` / `resolution_height` | **-1**（不变） |
   | `chroma_format` | 1 |
   | `max_custom_bitrate` | bit/s |
   | `dpi_scale` | 0 |
   | `resolution_type` | 1 |
   | `enable_hdr` | false |
   | `auto_frame_quality` | 2..4 |
   | `codec_type` | 0 = 任意，1 = H.264，5 = AV1 |
   | `max_scale_width` / `max_scale_height` | 0 = 不限，否则必须 ≥ 2 |
   | `fps_count` | 帧率上限 |
   | `weak_network` | 0 |
   | `cinema` | bool |

   - 带分辨率（>0）、DPI 或 `resolution_type ∈ 2..=4` 的请求属于显示变更，需要 `display` 能力，网页没有这项能力，会得到 -1（`displays.rs:260-277`）。
   - 结果码（`docs/protocol/peer-messages.md` “CaptureSetting 结果码”）：0、-3、-6 视为成功，-1、-2 视为失败。
   - 多人观看（§4.11）时，画质、帧率、观影模式、光标都会被统一的观看流覆盖。
4. **接收统计** `receiver_stats`（tag 7），每秒推送一次（`src/transport/rtc/channels.rs:403-409`）。**可选**。
   - 被控连续 3 次判定“慢”（`decode_fps < receive_fps × 0.85`，或 `decode_delay_ms > 1.5 × 帧间隔`）时，会把帧率上限压到 `decode_fps` 向下取 5 的倍数（`src/features/host/parameters.rs:360-401`）。`receive_fps < 5` 时不处理。
   - 浏览器若发送，`decode_delay_ms` 只填纯解码耗时：`Δ totalDecodeTime / Δ framesDecoded × 1000`。**不要**填 `jitterBufferDelay`，否则会被当成解码跟不上。
5. **不需要也不应发送**：`control_turn`（获取键鼠控制权）、`microphone_policy`、显示拓扑请求 11..14（需要 `display` 能力）。

**SessionBody 标签（解码用）**：

| tag | 消息 | 方向 |
|---|---|---|
| 1 | screens | H→C |
| 2 | quality | H→C |
| 3 | microphone | H→C |
| 4 | clipboard_files | H→C |
| 5 | capture | C→H |
| 7 | receiver_stats | C→H |
| 10 | capture_setting | C→H |
| 15 | video_tracks | C→H |
| 20 | capture_setting_result | H→C |
| 21 | display_result | H→C |
| 22 | microphone_result | H→C |
| 23 | control_turn_result | H→C |

失败的回复带 `RESPONSE|ERROR`，帧体为 `PeerError`（`wire.rs:177-202`）。

### 4.7 关键帧

- FPP **没有**关键帧请求消息。关键帧走 RTCP PLI/FIR：原生在开始播放时主动发一次 PLI（`src/session/controller/mod.rs:825`），被控对 PLI/FIR 按 RTT 节流后出关键帧（`docs/protocol/farwing-protocol.md` §6.5）。
- 浏览器的 libwebrtc 在等不到关键帧或解码出错时会自动发送 PLI/FIR，无需应用层处理。JS 没有标准 API 可以手动发 PLI（Encoded Transform 的 `RTCRtpScriptTransformer.sendKeyFrameRequest()` 视浏览器支持情况可用）。
- 丢包恢复（2026-10-18，`farwing-protocol.md` §6.6）：网页端在 Chromium 内核浏览器（Chrome、Edge）中为每个视频收发器请求 Dependency Descriptor（`setHeaderExtensionsToNegotiate`；其他浏览器未验证，不请求），answer 接受后每 100 ms 推送 `VideoFeedback{ssrc, rtp_timestamp}`（`server/web/src/lib/viewer.ts` 的 `startAcknowledgements`）。被控据此在修不回来的丢包后发恢复帧（H.264、AV1），libwebrtc 按描述符声明的依赖直接解码，不再等关键帧；H.265 与未协商描述符时，被控在拒绝重传时立即出关键帧。
- 应用层兜底：对当前屏幕发送 `capture{start:false}` 再发 `capture{start:true}`，会重建采集与编码器，从关键帧重新开始（`screens.rs:443-490`、`:592-625`）。新成员加入、重新协商等也会触发整组关键帧（`docs/protocol/v2-design.md` §4.2）。

### 4.8 CURSOR（0x0103，control）

- `pointers = false`：按 `shape` 处理。`cursor_type == -1` 表示被控光标隐藏；否则 `byte_value` 为 PNG，尺寸 ≤ 2048，热点 `pos_x/pos_y`（`src/features/remote_cursor.rs:74-154`）。
- `pointers = true`：`participants` 为其他参与者的指针（屏内归一化坐标）；`idle_ms` 超过 3 s 后淡出。
- 由于 CURSOR 不带实时位置（§4.5），只看不控的观看端应在 `session.start` 中设 `cursor_capture: true`，让被控把光标合成进画面（`src/features/stream_control/publisher.rs:335`）。多人观看流总是合成光标（`src/features/host/shared_view.rs:98`）。

### 4.9 PARTICIPANTS（0x0902）与 POINTER（0x0106）

- `Participants{participants[], controlling}`：`controlling` 为持有控制权者的 `id`，0 = 无人；`you` 标出接收方自己；最多 16 人（`src/protocol/participants.rs:57-89`、`:144-158`）。
- 可选发送 POINTER（control 通道，推送）：`{seq 递增, screen_id, x, y ∈ [0,1], visible, click?}`。被控会校验坐标范围，并转发给其他参与者，**不注入输入**（`src/features/host/peer/ingress.rs:50-63`，`src/protocol/participants.rs:121-139`）。

### 4.10 PERMISSIONS（0x0901）与 HOST_EVENT（0x0203）

- PERMISSIONS `{grants:[…]}`：网页通常为 `["view","audio"]` 或其子集；未知名称丢弃（`src/protocol/host_event.rs:48-62`）。
- HOST_EVENT `{kind, actor}`（`host_event.rs:10-32`）：

  | kind | 含义 |
  |---|---|
  | 1 | UPDATING |
  | 2 | RESTARTING（预计 90 s；约 400 ms 后会话以 `host_closed` 结束） |
  | 3 | SLEEPING |
  | 4 | SHUTTING_DOWN |
  | 5 | SESSION_SWITCH（预计 5 s） |
  | 6 | DISPLAY_CHANGING（2 s，`message` 为“<昵称> 修改了被控端的显示设置”） |

  收到 2、3、4 后应进入“等待设备”状态，被控恢复后按 §2.13 第 3 点重建会话（`src/transport/signal/host.rs:180-197`，`src/features/host/participants.rs:121-134`、`:254-282`）。

### 4.11 只看不控：控制权与多人观看流

- 浏览器从不发送 `ControlTurn`，因此永远不持有控制权。被控只注入持有者的键鼠（`src/features/host/participants.rs:337-371`）。
- **被控照常推流**，不需要任何额外请求：
  - **只有浏览器一位参与者**：使用浏览器自己的参数（`session.start` 的 capture 与后续 CaptureSetting）。`shared_viewer` 在参与者 ≤ 1 人时为 false（`participants.rs:406-412`）。
  - **两位及以上参与者、且浏览器不是持有者**：采集线程每轮自动把参数替换为“多人观看流”（`src/features/host/peer/capture_worker.rs:121-126`，`src/features/host/shared_view.rs:87-108`）：画质档 2（1080p 档）、60 fps、观影模式、合成光标、SDR、尺寸上限 3840×2160。格式取所有非持有者都能解码的最优 4:2:0 格式：AV1 10 bit > AV1 8 bit > HEVC 10 bit > HEVC 8 bit > H.264（`shared_view.rs:20-28`、`:52-66`、`:76-85`）。浏览器的设置在此期间不生效，只剩一人或持有者离开后恢复。
- 影响：浏览器只能解码 H.264 时，**所有**非持有者的共享流都会退到 H.264。能声明 AV1 时声明 AV1，可以让其他观看者保持更好的格式。

### 4.12 AUDIO（0x0202，session，JSON）

`src/protocol/audio_control.rs`，被控处理见 `src/features/host/audio.rs:156-200`。

- H→C：`{"kind":"hello","revision":3,"kbps":128,"effective_bps":128000}`。
- C→H（可选）：`{"kind":"set","request":7,"kbps":192}`，`kbps ∈ {64,128,192,256}`（`src/media/audio/encoder.rs:18`）。需要 `audio` 能力且音频已协商。
- 回复：`{"kind":"applied","request":7,"revision":4,"kbps":192,"effective_bps":192000}` 或 `{"kind":"rejected","request":7,"message":"…"}`。
- `request` 只是 JSON 内的关联号，FPP 帧头的 rid 为 0；帧体 ≤ 2048 字节。

### 4.13 保活与超时汇总

| 对象 | 规则 | 位置 |
|---|---|---|
| 实时 WS（服务端） | 60 s 内必须收到任意帧，否则 1001；服务端每 20 s 发 ping | `server/src/ws.rs:200-210` |
| 实时 WS（客户端） | 80 s（heartbeat + idle，至少 5 s）收不到任何帧即视为失效 | `src/transport/signal/transport.rs:738-741`、`:813` |
| 服务端 hello | 10 s | `transport.rs:30` |
| session.start ack | 10 s | `crates/protocol/src/lib.rs:105` |
| 入场券 / 未开始的会话 | 60 s | `lib.rs:82`，`server/src/hub.rs:1028-1036` |
| 恢复窗口 | 300 s | `lib.rs:84`，`hub.rs:994-1027` |
| 协商到 connected | 原生 12 s（建议 15–20 s） | `src/transport/signal/mod.rs:34` |
| 对端 HELLO 重发 | 500 ms × 30 次 | `src/transport/rtc/channels.rs:367-368` |
| 数据通道 | 观看端**没有**应用层心跳。INPUT `Heartbeat` 只在有键按下时使用，与观看无关；ICE consent 与 SCTP 心跳由浏览器处理 | `docs/protocol/farwing-protocol.md` §5.8 |

---

## 5. 浏览器做不到或有风险的地方，以及绕过办法

### 5.1 无法直接做到的事

1. **WebSocket 自定义请求头**：已由服务端的子协议入场解决（§2.1）。注意：
   - 必须同时提供 `farwing.v2`，否则不会跳过 `X-Farwing-Protocol` 检查，升级请求会以 400 失败。
   - 失败只表现为握手失败（1006），浏览器拿不到 401 的 JSON 内容。
   - 恢复令牌同样走子协议 `resume.<token>`。
2. **FCT/KCP 控制快速通道**：浏览器无法在 DTLS 层复用 KCP 记录。**不要**在 offer 中带 `a=x-farwing-fct`：被控会把 control 帧改走 FCT 记录，浏览器收不到 HELLO，也就永远不出画面（`src/features/host/peer/mod.rs:990`、`:1044-1045`；`src/transport/fct.rs:140-154`）。不带时自动回落到 SCTP。
3. **RS-FEC**（`x-farwing-rsfec`/36）与自定义扩展头（capture-index、new-frame、sending-delay）：浏览器无法协商。被控因此不发 FEC，也不写这些扩展头。抗丢包只靠 NACK/RTX；弱网下画质会差于原生。
4. **手动发 PLI**：没有标准 API，依赖浏览器自动发送。兜底办法：`capture` 先停后启（§4.7），或 ICE 重启。
5. **`a=max-message-size:524288`**：浏览器申报自己的上限（Chrome 通常为 262144），被控取两者的较小值。大于该值的 CURSOR 帧（光标 PNG 接近 512 KiB 时）会发送失败，只影响光标，概率很低。
6. **H.264 只能按 42e01f pm=1 协商**：被控只注册了这一种（`src/features/host/peer/mod.rs:200-236`）。实际码流可能是 Main 或 High profile（编码器使用 Main/High，4:4:4 时为 High 4:4:4，`src/platform/windows/nvenc.rs:330-342`）。Chrome、Edge、Safari 能解码 Main/High。Firefox 的 WebRTC H.264 依赖 OpenH264 插件，能否解码 High 要实测。**绝不能声明 4:4:4**。
7. **AV1 fmtp 冲突**（Chrome 的 `level-idx=5` 与被控的 `level-idx=17`）：只改 wire SDP，删掉 `level-idx`（§3.4），或首版不声明 AV1。
8. **Opus 立体声**：需要修改本地 SDP（加 `stereo=1`，§3.4）。Chrome 逐步限制 SDP 修改，若被禁止，声音会退化为单声道 Voip 编码，但仍可播放。
9. **H.265、HDR、10 bit**：多数浏览器的 WebRTC 不支持，不要声明（H.265 已改为按浏览器能力声明，见 §3.4 末尾）。
10. **数据通道 `priority`**：只有部分浏览器支持，被控不读取，可以不设。

### 5.2 风险点（需要实测）

1. **被控的隐含要求**（`docs/web-console.md` §7.6 也提到）。已核实的硬性条件：
   - `attempt_id` 必须是 UUID；
   - `screen_id` 必须显式写 -1；
   - `decoders` 与 `video_codec_capability` 都非空，且格式能相交；
   - offer 带合法签名；
   - DCEP（非预协商）的 control 与 session 通道；
   - control 通道上的双向 HELLO，`protocol_version` 与 `min_protocol_version` 都为 2；
   - offer 不带 fct。

   代码中**没有**发现对 audio msid/ssrc、m-line 数量、`a=x-farwing-nat`、`max-message-size` 的检查。
2. **`resolution_type`**：写 2 会改变被控的物理分辨率，而初始准备阶段**不检查** `display` 能力（`src/features/host/displays/initial.rs:247-293`）。网页必须写 3。这处权限缺口已在被控补上（2026-10-17）：没有 `display` 能力时，初始准备把 2 / 4 改为 3 并记录日志（`src/features/host/displays/initial.rs`）。
3. **同一 `sid` 重复 `session.start`**：被控会以 `media_failed` 结束会话（`src/transport/signal/host.rs:618-621` 与 `:534-556`）。恢复时用新会话，或刷新入场券后重连（§2.13）。
4. **mDNS 主机候选**：浏览器在没有媒体权限时用 `.local` 隐藏局域网地址。被控默认接受并解析 mDNS（`QueryOnly`），但跨网段时可能解析失败；此时 srflx 与中继仍可用。
5. **TLS 证书**：原生客户端可以固定自签证书的指纹，浏览器不行，wss 需要浏览器信任的证书（或用户已在同源页面接受过证书）。
6. **轨道序号映射**：用 mid 与 answer 的 msid，或 `streams[0].id`，不要用 `track.id`（§3.5）。
7. **ReceiverStats 的语义**：误填会让被控压低帧率（§4.6）。不确定时不要发。
8. **共享观看流的格式**：浏览器只支持 H.264 时，会把其他观看者的共享流拉到 H.264（§4.11）。
9. **WebCrypto Ed25519 的可用性**：Chrome/Edge 137+、Firefox 129+、Safari 17+ 支持；更早的版本需要 `@noble/ed25519`。密钥必须与服务端登记的公钥一致，设备 ID 必须由公钥推导。
10. **首次连接耗时**：被控首次为某个显示源准备媒体时要探测编码器（有磁盘缓存），可能耗时数秒。浏览器不要用 12 s 以内的超时。
11. **代码仍在变动**：网页相关的服务端与被控代码在调研期间仍在修改（见文首）。实现前请再核对 `server/src/ws.rs`（子协议）、`server/src/http/sessions.rs`（网页能力裁剪）与 `src/transport/signal/host.rs`（`controller_caps_limit`，现已删除，被控按 `ControllerInfo.caps_limit` 裁剪）。

### 5.3 文档与代码不一致（以代码为准）

| 文档说法 | 代码事实 |
|---|---|
| `docs/protocol/v2-design.md` §5.3：数据通道“全部预协商，`negotiated: true`，id 0/2/4/6/8” | 带内 DCEP，不预协商（`src/transport/rtc/channels.rs:619-639`，`src/features/host/peer/mod.rs:728-761`）。`docs/protocol/farwing-protocol.md` §5.3 的写法正确 |
| `v2-design.md` §5.4：“主控在 session 通道打开后发送 HELLO，只交换一次，5 s 超时” | HELLO 在 **control** 通道上双向交换，主控每 500 ms 重发、最多 30 次（`channels.rs:367-433`，`src/features/host/peer/ingress.rs`）。`farwing-protocol.md` §5.5 的写法正确 |
| “请求的 request_id 非 0” | HELLO 请求实际使用 rid 0（`src/protocol/peer.rs:232-234`），必须靠 RESPONSE 标志区分请求与响应 |
| `peer-messages.md`：`display_geometry` 只在主控声明 `display.geometry` 时发送 | 被控默认总是发送（`SessionExtensions::default().display_geometry = true`，`src/transport/signal/host.rs:1189-1200`）。观看端忽略即可 |
| `farwing-protocol.md` §6.1：audio_0 “即使不发声也带 msid/SSRC” | 被控不检查 |

### 5.4 实现清单（按顺序）

1. **WS 客户端**：子协议入场；回应 ping；处理 ack/error 与关闭码；`bye`；（可选）resume 与 `resume.refresh`。
2. **`session.start`**：按 §2.5 填写；校验 ack 中 host 公钥与设备 ID。
3. **PeerConnection**：1 条 audio + 5 条 recvonly video + 5 个 DCEP 通道；可选的编码偏好；修改 Opus stereo；签名；`rtc.offer` gen 1；发候选（跳过空候选）。
4. **处理 answer**：验签 → 安装 → 补装缓存的候选；建立 mid → `video_N` 映射；`ontrack` → `<video>`/`<audio>`。
5. **FPP 编解码**：10 字节帧头；按 §4.3 的 protobuf 解析；按帧类型分发，不看通道。
6. **HELLO**：control 通道 open 后每 500 ms 发请求直到收到响应；收到被控请求立即回 RESPONSE。
7. **解析推送**：Screens、PERMISSIONS、PARTICIPANTS、HOST_EVENT、AUDIO、CURSOR、Quality；其余忽略。
8. **可选功能**：用 `capture{start:true}` 切换显示器；`capture_setting`；`receiver_stats`；AUDIO set；POINTER。
9. **恢复**：ICE 重启（重新签名，gen + 1）；实时连接恢复；会话丢失时退避新建会话。
