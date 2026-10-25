# Farwing Protocol v2

Farwing Protocol 是 Farwing Client 与 Farwing Server 之间、以及两个 Farwing Client 之间的通信协议。本文是 v2 的规范；对端协议各帧的消息体定义见 [peer-messages.md](peer-messages.md)，设计取舍、实现状态与验证记录见 [v2-design.md](v2-design.md)。v2 与 v1 不兼容，客户端与服务端须同时升级。

| 层 | 承载 | 作用 |
|----|------|------|
| 控制面 HTTPS | HTTPS + JSON，`/api/v2/...`，设备签名 | 设备登记、账号、设备与好友授权、配置、会话入场、访客与受信任设备入场、电源 |
| 实时面 WSS | WebSocket over TLS + JSON，`/api/v2/ws` | 设备在线、会话协调、SDP/ICE 中转、服务端推送 |
| 直连端点 | TLS 1.3 over TCP + 长度前缀 JSON | 不经服务端的握手、授权与会话信令（§8） |
| 局域网发现 | UDP 组播/广播 + JSON | 发现局域网内的被控（§8.6） |
| 对端协议 FPP | WebRTC 数据通道（SCTP/DTLS），可选 FCT | 两个客户端之间的输入、串流控制、剪贴板、文件、拖放、批注、端口转发、诊断 |
| 媒体 | WebRTC RTP/RTCP/SRTP | 视频、桌面声音、麦克风（§6） |

服务端只做信令与凭据，不接触、不重编码媒体；对端的媒体与数据通道经 DTLS 端到端加密。谁可以连接一台被控由被控自己判定（本账号设备、好友、受信任设备、访客），概览见 [access-model.md](../access-model.md)。

线上类型与常量定义在 `crates/protocol`（crate `farwing-protocol`，客户端与服务端共用）：`lib.rs`（版本、请求头、限制、平台、能力名）、`api.rs`（HTTPS 主体与路径）、`realtime.rs`（实时面）、`session.rs`（会话协商载荷）、`error.rs`（错误码）、`ids.rs`（标识格式）、`crypto.rs`（签名串、PAKE）、`direct.rs`（直连与局域网发现）、`peer/mod.rs`（FPP 帧层）。代码与本文不一致时以代码为准，并修正本文。

---

## 1. 版本、标识与通用约定

### 1.1 版本

协议名 `farwing`，主版本 `2`（`PROTOCOL_VERSION = MIN_PROTOCOL_VERSION = 2`）。

| 层 | 版本标识 | 不符时 |
|----|----------|--------|
| HTTPS | 请求头 `X-Farwing-Protocol: 2`；服务端在每个响应中回写 | 缺少该头：`400 bad_request`；版本不在支持区间：`426 protocol_unsupported`，`details: {min, max}` |
| WSS | 升级请求同样带 `X-Farwing-Protocol: 2`（同上检查）；每个 JSON 帧 `"v": 2` | 帧版本不符以关闭码 `4009` 结束，缺少 `v` 以 `4008` 结束 |
| FPP | 帧头版本字节 `2`；HELLO 交换 `protocol_version` / `min_protocol_version` | 帧被丢弃；HELLO 不兼容时握手失败 |
| 直连 | TLS ALPN `farwing-direct/2`；`Envelope.v = 2` | 握手失败 |
| 局域网发现 | 报文 `"v": 2` | 忽略 |

`GET /api/v2/server` 返回服务端支持的版本区间，不需要协议头与认证。`GET /healthz` 不属于 `/api/` 前缀，不检查协议头。

### 1.2 平台

字符串枚举 `windows | macos | linux | android | ios`；收到不认识的值按 `unknown` 处理。

### 1.3 标识

| 名称 | 格式 | 来源 | 说明 |
|------|------|------|------|
| `user_id` | UUID | 服务端 | 账号 |
| `public_key` | base64url（无填充）32 字节 Ed25519 公钥 | 客户端 | 设备身份；每个安装（profile）一个密钥对 |
| `device_id` | 16 位小写 base32（`a-z2-7`） | 由公钥推导 | `base32(sha256(公钥字节))` 的前 16 个字符（80 位，`crypto::local_device_id`）。服务端以它登记设备，有无服务器、在哪台服务器都相同；也是设备的指纹，界面按四位一组显示 |
| `installation_id` | 字符串，≤ 64 字符 | 客户端 | 仅作资料记录，设备身份以公钥为准 |
| `access_token` | `fwa_` + 43 字符 base64url | 服务端 | 账号凭据，绑定登录时签名的设备 |
| `guest_token` | `fwg_` + 43 字符 base64url | 服务端 | 未登录设备（被控或主控）的凭据，默认 24 小时 |
| `ticket` | `fwt_` + 43 字符 base64url | 服务端 | 单次使用的 WSS 入场券，60 秒有效 |
| `resume_token` | `fwr_` + 43 字符 base64url | 服务端 | WSS 恢复凭据，断开后 300 秒内有效，可刷新 |
| `connect_id` | 9 位数字（首位非零） | 服务端 | 短号：经服务端寻址用的拨号别名，不是身份；首次 `POST /assist/identity` 时分配 |
| `session_id` | UUID | 服务端（直连时被控） | 一个主控参与者与被控之间的会话 |
| `participant_id` | UUID | 服务端（直连时被控） | 会话中的参与者 |
| `attempt_id` | 字符串，1–64 字符（客户端用 UUID） | 主控 | 一次 `session.start` 尝试，会话重建时更换 |
| `request_id` | UUID | 服务端 | 入场（`assist/join`）、电源、唤醒请求 |

令牌主体均为 32 字节随机数的 base64url（无填充）；服务端只保存其 SHA-256。

### 1.4 代次

- **在线代次 `epoch`**：被控每次新建（非恢复）在线连接时服务端递增，在 `hello` 中返回；旧 `epoch` 的会话以 `host_offline` 结束，发往旧代次被控的消息不再转发。
- **协商代次 `gen`**：主控的 `rtc.offer` 从 `gen = 1` 开始，每次 ICE 重启加 1；被控在 `rtc.answer` 中回写。被控丢弃 `gen` 不大于已处理值的 offer，主控只接受 `gen` 等于当前值的 answer（`0` 或缺省表示不检查）；候选按 `attempt_id` 归属。服务端只转发 `gen`，不校验。
- **账号代次**：客户端本地以登录凭据区分登录代次，丢弃过期的异步结果（不上线路）。

### 1.5 限制

| 项 | 值 |
|----|----|
| HTTPS JSON 请求体 | 64 KiB（超出 `413 payload_too_large`） |
| 壁纸上传 | 4 MiB |
| WSS 单帧 / SDP | 1 MiB / 1 MiB |
| 单个候选（序列化后） | 2 KiB |
| 每次尝试、每个方向的候选数 | 256 |
| WSS 消息速率 | 每连接 200 条/秒，突发 400（超出以 `4029` 关闭） |
| 每被控参与者数 | 默认 8，可设 1–16（`policy.max_participants`） |
| FPP 单帧 | 512 KiB（SCTP `max-message-size` 524288）；各特性另有更小的上限（§5） |
| 直连帧 | 1 MiB |
| 局域网发现报文 | 1200 字节 |

时间戳为整数：Unix 毫秒（`*_ms`、`timestamp`）或 Unix 秒（`*_at`、`since`、`ts`）。

---

## 2. 设备身份与密码学

### 2.1 设备密钥

每个安装生成 Ed25519 密钥对；私钥与设备身份保存在系统凭据存储（安装后台服务后为 DPAPI 保护的服务保险库），不写明文文件。同一安装重新登记时沿用原密钥；密钥丢失即为新设备。设备 ID 由公钥推导（§1.3），凡是同时出现设备 ID 与公钥的地方（`direct.hello`、局域网通告、服务端下发的对端身份），接收方都校验两者一致，不一致即拒绝。设备令牌已取消，设备认证全部由请求签名完成（§3.2）。

### 2.2 签名与 MAC 串

所有密钥、签名、PAKE 消息与 MAC 在线路上均为 base64url（无填充）。`hex` 为小写十六进制。

| 用途 | 算法 | 被签/被 MAC 的字节 |
|------|------|------------------|
| 设备登记持有证明（`devices/init.proof`） | Ed25519 | `"FWINIT2\n" + public_key + "\n" + timestamp` |
| HTTPS 请求签名 | Ed25519 | `"FWREQ2\n" + METHOD(大写) + "\n" + path_and_query + "\n" + timestamp + "\n" + hex(sha256(body))` |
| DTLS 指纹绑定（`a=x-farwing-identity`） | Ed25519 | `"FWDTLS2\n" + session_id + "\n" + attempt_id + "\n" + role + "\n" + fingerprints` |
| 访问码 PAKE 指纹绑定（`a=x-farwing-pake`） | HMAC-SHA256(K) | 同上，`attempt_id` 为空串 |
| 访问码密钥确认（经服务端） | HMAC-SHA256(K) | `"FWAK2\n" + label + "\n" + request_id` |
| 直连身份证明（`direct.hello.proof`） | Ed25519 | `"FWDIRECT2\n" + role + "\n" + hex(ekm)` |
| 访问码密钥确认（直连） | HMAC-SHA256(K) | `"FWPAIR2\n" + label + "\n" + hex(ekm)` |
| 局域网通告（`sig`） | Ed25519 | `"FWLAN2\n" + 除 sig 外按键排序的 JSON` |

- `role` / `label` 取 `controller` 或 `host`。
- `fingerprints`：SDP 中全部 `a=fingerprint:` 的值（如 `sha-256 AB:CD:…`），去首尾空白、转小写、排序、去重后以 `,` 连接。
- `ekm`：TLS 导出密钥 `export_keying_material(32, "EXPORTER-farwing-direct-v2")`。
- 设备 ID 即公钥的指纹（§1.3），界面不另列公钥指纹。

### 2.3 PAKE

SPAKE2（`spake2` crate，Ed25519 群，对称模式），口令为访问码的 UTF-8 字节，共享密钥 K 为 32 字节。

| 用途 | 身份串 | 槽位 |
|------|--------|------|
| 访问码（经服务端 §3.11、直连 §8.3） | `farwing-assist-v2` | 2：槽 0 临时访问码，槽 1 固定访问码；主控两槽都用输入的访问码生成；被控未配置的槽用随机口令应答 |

错误的访问码同样得到一个 K，只能由密钥确认发现。

### 2.4 DTLS 指纹绑定与身份校验

- offer 与 answer 都在会话级（第一个 `m=` 之前）加入 `a=x-farwing-identity:<签名>`；有 PAKE 密钥的会话（输入了访问码的连接，经服务端或直连）另加 `a=x-farwing-pake:<MAC>`，仅确认模式与按设备密钥授权的会话不带。
- 对端公钥来源：主控取 `session.start` 的 `ack.host.public_key`（直连时为 `direct.hello.public_key`），被控取 `session.incoming.controller.public_key`。
- 校验前先确认对端公钥能推导出对端的设备 ID（§1.3），否则拒绝连接（“对端出示的密钥与设备 ID 不符”）。不需要首次信任记录。
- 缺少属性、签名无效或 MAC 不符一律终止：被控以 `session.end{reason: "identity_mismatch"}` 结束会话，主控直接结束本次连接。

---

## 3. 控制面 HTTPS API

基础地址为服务器配置的 `public_url`（如 `https://farwing.example.com:8443`，可带路径前缀）。只用 HTTPS。

### 3.1 请求头与认证级别

| 头 | 说明 |
|----|------|
| `X-Farwing-Protocol: 2` | 必需（`/api/v2/server` 除外） |
| `X-Farwing-Request-Id` | 可选，1–64 个 `[A-Za-z0-9_-]`；服务端在响应中回写（无效或缺省时自行生成） |
| `X-Farwing-Device-Id` / `X-Farwing-Timestamp` / `X-Farwing-Signature` | 设备签名（§3.2） |
| `Authorization: Bearer <access_token 或 guest_token>` | 账号或游客凭据 |

| 认证级别 | 要求 |
|----------|------|
| 无 | 不需要签名与令牌 |
| 设备 | 设备签名 |
| 账号 | 设备签名 + 绑定该设备的 `access_token` |
| 任一 | 设备签名 + `access_token` 或该设备的 `guest_token`（服务端开启游客时） |

### 3.2 请求签名

- 客户端以 `X-Farwing-Timestamp`（Unix 毫秒）与请求的原始字节计算签名（§2.2），`path_and_query` 为请求行中的路径与查询串。服务端反向代理剥掉 `public_url` 路径前缀时，服务端同时接受对带前缀路径的签名。
- 服务端校验：设备存在、签名有效、时间偏差 ≤ ±300 秒、签名在 600 秒内未被使用过（重放缓存只在内存中）。任何失败返回 `401 device_unauthorized`。
- 客户端在同一进程内保证签名时间戳严格递增，并以 `GET /server` 的 `time_ms` 校准时钟偏差。被拒的请求未执行：客户端重新校准时钟后重签并重发一次。
- 账号令牌绑定登录时签名的设备：其他设备的签名返回 `401 device_mismatch`；令牌吊销、30 天未使用、账号禁用或设备已解绑返回 `401 session_revoked`；令牌无效返回 `401 unauthorized`。每次使用滑动续期。

### 3.3 响应与错误

成功：HTTP 2xx，JSON 主体即数据（无数据时 `{}`）。失败：

```json
{"error": {"code": "forbidden", "message": "two-step verification required", "retryable": false, "details": {"reason": "totp_required"}}}
```

客户端按 `code` 处理，不解析也不显示 `message`（英文，供日志与排查）。同一 `code` 有多种情况时，`details.reason` 给出稳定的原因（`farwing_protocol::error::reason`，例如 `totp_required`、`username_taken`、`password_too_short`），需要的参数也在 `details` 中（如 `details.min`）；客户端与网页端按 `code` 与 `reason` 显示自己语言的文字（`docs/i18n.md`）。`rate_limited` 另带 `Retry-After` 头（秒）与 `details.retry_after_ms`。

| code | HTTP | 可重试 | 含义 |
|------|------|--------|------|
| `bad_request` | 400 | | 参数无效、缺少协议头 |
| `unauthorized` | 401 | | 缺少或无效的账号/游客凭据、登录失败 |
| `device_unauthorized` | 401 | | 设备签名或登记证明无效、时间偏差过大、重放、未知设备 |
| `device_mismatch` | 401 | | 令牌属于另一台设备 |
| `session_revoked` | 401 | | 登录已吊销、过期、账号禁用或设备已解绑 |
| `forbidden` | 403 | | 无权访问 |
| `assist_code_invalid` | 403 | | 访问码错误 |
| `assist_rejected` | 403 | | 被控用户拒绝本次连接 |
| `registration_disabled` | 403 | | 保留；当前服务端不返回 |
| `not_found` / `device_not_found` / `user_not_found` | 404 | | |
| `timeout` | 408 | 是 | 等待对端超时 |
| `device_offline` | 409 | | 目标不在线 |
| `host_disabled` | 409 | | 目标未开启所需的主人策略 |
| `busy` | 409 | | 被控参与者已满 |
| `conflict` | 409 | | 状态已变化（如入场请求已取消、目标在线时请求唤醒） |
| `assist_unavailable` | 409 | | 对方没有开启访问码，也没有信任这台设备，或暂时不接受 |
| `assist_code_required` | 409 | | 需要访问码；`details.mode` |
| `already_friends` | 409 | | |
| `payload_too_large` | 413 | | |
| `protocol_unsupported` | 426 | | `details: {min, max}` |
| `rate_limited` | 429 | 是 | `details.retry_after_ms` |
| `internal` | 500 | 是 | |
| `unavailable` | 503 | 是 | 服务端正在关闭 |
| `setup_required` | 503 | | 服务端尚未初始化（还没有管理员），只有 `GET /server` 与初始化接口应答（[server-admin-api.md](../server-admin-api.md) §3） |
| `network` | — | 是 | 仅客户端本地：请求未送达或无响应 |

写操作结果不确定时客户端不自动重放。

### 3.4 接口一览

路径前缀均为 `/api/v2`。

| 方法 路径 | 认证 | 请求 | 响应 |
|-----------|------|------|------|
| `GET /server` | 无 | — | `ServerInfo` |
| `POST /devices/init` | 无（主体内证明） | `DeviceInitRequest` | `{device_id, alias}` |
| `POST /auth/login` | 设备 | `{username, password}` | `{access_token, user_id, username, nickname}` |
| `POST /auth/logout` | 账号 | — | `{}` |
| `GET /auth/sessions` | 账号 | — | `{sessions: [AuthSession]}` |
| `DELETE /auth/sessions/{device_id}` | 账号 | — | `{}` |
| `GET /me` | 账号 | — | `{user_id, username, nickname}` |
| `POST /guest/session` | 设备 | — | `{guest_token, expires_at}` |
| `GET /devices` | 账号 | — | `{current_device, own: [Device], shared: [Device]}` |
| `PATCH /devices/{id}` | 账号 | `{alias}` | `{device_id, alias}` |
| `DELETE /devices/{id}` | 账号 | — | `{}` |
| `GET /devices/{id}/detail` | 账号 | — | `DeviceDetail` |
| `GET /devices/{id}/wallpaper` | 账号 | `If-None-Match` | 图像字节（`ETag`，或 `304`） |
| `POST /devices/{id}/power` | 账号 | `{action}` | `PowerReceipt` |
| `PUT /devices/self/policy` | 任一 | `DevicePolicy` | 规范化后的 `DevicePolicy` |
| `GET /devices/self/friend-access` | 账号 | — | `FriendAccessList`（§3.12） |
| `PUT /devices/self/wallpaper` | 账号 | PNG/JPEG/BMP 原始字节（≤ 4 MiB） | `{wallpaper_url}` |
| `DELETE /devices/self/wallpaper` | 账号 | — | `{}` |
| `PUT /devices/self/wol` | 账号 | `{mac, inner_ip, subnet_mask}` | `{supported}` |
| `POST /power/{request_id}/result` | 任一 | `{code, message, state}` | `{}` |
| `GET /config` | 任一 | — | `{bitrate_limit_mbps, network}` |
| `GET /updates/{platform}?channel=stable\|beta` | 无 | — | `{latest: ReleaseInfo \| null}` |
| `POST /presence/ticket` | 任一 | 空 | `PresenceTicket` |
| `POST /sessions` | 账号 | `{target_device_id, purpose, presence?}` | `SessionTicket` |
| `POST /sessions/{session_id}/ticket` | 任一（该会话的主控设备） | — | `SessionTicket` |
| `GET /friends` | 账号 | — | `{friends: [Friend]}` |
| `DELETE /friends/{user_id}` | 账号 | — | `{}` |
| `GET /friends/requests` | 账号 | — | `{incoming: [FriendRequest], outgoing: [FriendRequest]}` |
| `POST /friends/requests` | 账号 | `{username}` | `{"sent": {request}}` 或 `{"accepted": {friend}}` |
| `POST /friends/requests/{id}/accept` | 账号 | — | `Friend` |
| `POST /friends/requests/{id}/reject` | 账号 | — | `{}` |
| `DELETE /friends/requests/{id}` | 账号 | — | `{}`（撤回自己发出的申请） |
| `GET /grants/{user_id}` | 账号 | — | `Grants` |
| `PUT /grants/{user_id}` | 账号 | `Grants` | 规范化后的 `Grants` |
| `POST /assist/join` | 任一 | `AssistJoinRequest` | `AssistPending` |
| `POST /assist/requests/{id}/accept` | 任一（发起者） | `{key_confirm}` | 长轮询 → `AssistJoin` |
| `POST /assist/requests/{id}/cancel` | 任一（发起者） | — | `{}` |
| `POST /assist/requests/{id}/pake` | 任一（目标被控） | `AssistPakeReply` | `{}` |
| `POST /assist/requests/{id}/decision` | 任一（目标被控） | `AssistDecision` `{allowed, refusal?, trusted}` | `{}` |
| `POST /assist/identity` | 任一 | — | `{connect_id}` |
| `GET /assist/saved/{recent\|favorites}` | 账号 | — | `{saved: [SavedDevice]}` |
| `PUT /assist/saved/favorites` | 账号 | `{favorites: [{connect_id, remark, favorited_at}]}`（≤ 500） | `{saved: [..]}` |
| `DELETE /assist/saved/recent` | 账号 | — | `{}` |
| `DELETE /assist/saved/{kind}/{device_id}` | 账号 | — | `{}` |
| `POST /sfu/rooms` | 任一（被控） | `CreateRoomRequest` `{sessions, mode, offer}` | `RoomCreated` `{room, mode, answer, node?}` |
| `PUT /sfu/rooms/{room}` | 任一（房间的被控） | `{sessions?, offer?}` | `{answer?}` |
| `DELETE /sfu/rooms/{room}` | 任一（房间的被控） | — | `{}` |
| `POST /sfu/rooms/{room}/subscribe` | 任一（该会话的主控） | `SubscribeRequest` `{session, connection, offer, tracks?}` | `Subscription` `{answer, mode}` |
| `DELETE /sfu/rooms/{room}/subscribe/{session}/{connection}` | 任一（该会话的主控或被控） | — | `{}` |
| `GET /ws` | 入场券或恢复令牌（§4.1） | — | WebSocket 升级 |

中转节点的连接（`/edge/ws`）与管理见 [server-admin-api.md](../server-admin-api.md) §5。网页端与管理接口（`/web/info`、`/me/*`、`/admin/*`、`/setup*`）见 [web-console.md](../web-console.md) §8 与 [server-admin-api.md](../server-admin-api.md)。

### 3.5 服务器信息

```json
{
  "protocol": {"name": "farwing", "min": 2, "max": 2},
  "server": {"name": "Farwing Server", "version": "0.1.0", "instance_name": "home"},
  "features": {"guest_hosts": true, "turn": true, "force_relay": false, "updates": false, "friends": true,
               "nat_discovery": ["stun:203.0.113.10:3479", "stun:203.0.113.10:3480"]},
  "time_ms": 1760000000000
}
```

`nat_discovery` 为 RFC 5780 行为探测端点（§7.1），未配置时省略。

### 3.6 设备登记

`POST /devices/init` 不需要签名；持有证明在主体内。

```json
{
  "installation_id": "…", "name": "DESKTOP-1", "platform": "windows", "os": "Windows 11 …", "version": "0.1.0",
  "public_key": "<base64url>", "timestamp": 1760000000000, "proof": "<base64url 签名>",
  "capabilities": ["host", "files", "ports", "power", "wake_on_lan", "assist"],
  "policy": {"control": false, "files": true, "ports": false, "power": false, "wol": false, "assist": false, "max_participants": 8},
  "hardware": {"cpu": "…", "base_board": "…", "video": ["…"], "memory": 34359738368, "screen": "2560x1440", "mac": "…"}
}
```

- 服务端校验公钥格式、时间偏差（±300 秒）、证明签名与证明是否重放；失败返回 `401 device_unauthorized`。
- 已登记的公钥：更新同一设备的资料、能力与策略（变化时推送 `device.changed{updated}`），返回其 `device_id`。新公钥：以由公钥推导的 `device_id`（§1.3）登记新设备（按来源 IP 限速），设备不属于任何账号，登录后绑定。
- 服务端数据库没有迁移：只打开当前结构版本的数据库，其他版本拒绝启动（见 `server/README.md`“升级”）。
- `capabilities`（设备能做什么，会话前判断用）：`host`（可被控）、`files`、`ports`、`power`（远程关机/重启）、`wake_on_lan`、`assist`。
- `policy`（主人允许什么，服务端在会话前执行，被控在会话内再次执行）：`control`（允许连接本机，所有目的的前提）、`files`、`ports`、`power`、`wol`、`assist`（接受访客或受信任设备经服务端连接：访问码已开启，或被控有受信任设备）、`max_participants`（规范化到 1–16）。
- `hardware` 可选，只对设备主人展示。

### 3.7 账号与登录

- 账号只能由管理员创建。登录失败统一返回 `unauthorized`，按来源 IP 与用户名限速。
- 登录令牌绑定发起登录的设备；同一设备的新登录吊销其所有旧登录，并把设备绑定到该账号（其他账号的实时连接以 `4003` 关闭）。
- `logout` 吊销令牌并把设备从账号解绑。`DELETE /auth/sessions/{device_id}` 效果同该设备注销：令牌吊销、设备解绑、实时连接以 `4003` 断开、其他设备收到 `device.changed{removed}`。
- `AuthSession`：`{device_id, device_alias, platform, created_at, last_used_at, current}`，每台设备一条，`current` 标出发起请求的设备。
- `POST /guest/session`：服务端未开启游客时 `403 forbidden`。

### 3.8 设备

`Device`：

```json
{
  "device_id": "…", "alias": "…", "status": "online" | "offline", "platform": "windows", "version": "0.1.0",
  "capabilities": ["host", …], "policy": DevicePolicy, "wol_supported": false,
  "wallpaper_url": "/api/v2/devices/<id>/wallpaper?v=<etag 前 16 位>" | "",
  "participants": [{"participant_id", "session_id", "user_id", "nickname", "device_id", "device_alias",
                    "relation": "own|friend|assist|trusted", "purpose", "started_at"}],
  "last_seen_at": 0, "owner": UserSummary | null, "granted": ["view", …], "public_key": "<base64url>"
}
```

- `GET /devices`：`own` 为本账号的其他设备（`granted` 为全部能力，`owner` 为空）；`shared` 为好友的、本账号至少持有一项能力的设备（带 `owner{user_id, username, nickname}` 与按本账号计算的 `granted`），好友只看到自己在该设备上的参与者。
- `relation`（`Relation`）：`own` 本账号设备，`friend` 好友设备（按授权），`trusted` 被控信任的设备，`assist` 访客（访问码或本机确认，仅本次连接）。
- `status` 由在线连接决定；`wol_supported` 为策略允许且已登记 WoL 信息。
- `DeviceDetail`：`{device_id, alias, platform, os, version, online, registered_at, last_seen_at, connect_id, hardware, owner}`；好友设备不含 `connect_id` 与 `hardware`。
- 改名（1–64 个可打印字符）与删除只限本账号设备（好友设备 `403`）。删除即解绑：令牌吊销、实时连接以 `4003` 断开。
- `PUT /devices/self/policy` 设置主人策略；变化时推送 `device.changed{updated}`。
- 壁纸：上传按内容识别 PNG/JPEG/BMP；读取对本账号与好友可见的设备有效，支持 `ETag` / `If-None-Match`。

### 3.9 配置与更新

- `GET /config`：`{bitrate_limit_mbps: u32 | null}`。
- `GET /updates/{platform}`：管理员发布的版本公告（网页端“客户端更新”或管理接口，不在配置文件中）`{version, prerelease, published_at, notes, url}`；`beta` 渠道同时包含 `stable` 条目，取 `published_at` 最新者；未配置或平台未知时 `{latest: null}`。

### 3.10 会话入场

**被控在线**：`POST /presence/ticket`（空主体）→

```json
{"ticket": "fwt_…", "endpoints": ["wss://farwing.example.com:8443/api/v2/ws"],
 "connect_timeout_ms": 10000, "retry_delay_ms": 1000, "max_reconnect_delay_s": 300}
```

**主控创建会话**：`POST /sessions {"target_device_id", "purpose": "desktop|audio|files|ports", "presence": false}` →

```json
{"session_id": "uuid", "ticket": "fwt_…", "endpoints": ["wss://…"], "connect_timeout_ms": 10000, "retry_delay_ms": 1000,
 "target": {"device_id", "alias", "platform", "version", "capabilities", "relation": "own|friend", "granted": [..], "public_key"},
 "bitrate_limit_mbps": null}
```

- 目标须为本账号设备，或持有 `purpose` 所需能力的好友设备（看得到但缺能力 `403 forbidden`；看不到 `404 device_not_found`；不能控制自己 `400`）。所需能力：`desktop`、`audio` 需 `view`，`files` 需 `files`，`ports` 需 `ports`。
- 目标须在线（`409 device_offline`），具备 `host` 能力且主人策略允许：`policy.control`，以及 `files` 目的需 `policy.files`、`ports` 目的需 `policy.ports`（否则 `409 host_disabled`）。游客被控只接受 §3.11 的会话（`connect_type = assist`）。
- 每台被控最多 `max_participants` 个会话；同一主控设备对同一被控只保留一个会话，新会话以 `replaced` 结束自己的旧会话；超出上限 `409 busy`，从不顶替其他主控。
- `presence: true`：主控用自己的在线连接发信令（§4.7），响应中 `ticket` 为空串。
- 会话须在入场券有效期（60 秒）内开始，否则以 `expired` 结束。`POST /sessions/{session_id}/ticket` 为同一会话签发新入场券（断线重建），只限该会话的主控设备，账号或游客凭据均可（§3.11 的会话主控可以没有账号，`target.relation` 为 `trusted` 或 `assist`）；会话已结束时 `404 not_found`。

### 3.11 受信任设备与访客（`assist`）

本账号设备与好友用 `POST /sessions`（§3.10）；其他设备经下面的流程请求被控放行，由被控判定（v2-design §2）：被控信任的设备按设备密钥放行（`relation = trusted`），访客须知道访问码或由被控用户确认（`relation = assist`）。访问码从不经过服务端；服务端只中转 SPAKE2 消息与密钥确认（§2.2、§2.3）。主控有无账号均可（认证级别“任一”）。

`AssistJoinRequest = {connect_id, device_id, pake}`：`connect_id`（9 位短号）与 `device_id` 恰好给一个，另一个为空串；`pake` 为 0 或 2 条 SPAKE2 消息（用户未输入访问码时为空）。

1. **主控** `POST /assist/join`：服务端按短号或设备 ID 找到被控（两者都给、都不给或格式无效 `400`，目标是自己 `400`，不存在 `404 device_not_found`，不在线 `409 device_offline`，`policy.assist` 为假 `409 assist_unavailable`），推送 `assist.join{request_id, controller, pake}`（无账号主控的 `controller` 账号字段为空），最多等待 10 秒。
2. **被控** `POST /assist/requests/{id}/pake`，主体 `AssistPakeReply{allowed, mode, trusted, pake, key_confirm}`，`mode` 为 `password | confirmation | password_confirmation`：
   - 请求未带 `pake`、主控公钥在被控的受信任设备中且被控允许连接：`allowed: true, trusted: true`，`pake`、`key_confirm` 为空。
   - 否则 `pake: [msgB_0, msgB_1]`，`key_confirm: [HMAC(K_i, "FWAK2\nhost\n"+request_id)]`；仅确认模式 `pake` 为空。
3. **服务端**返回主控 `AssistPending{request_id, mode, trusted, pake, key_confirm, host{device_id, alias, platform, version, public_key}}`。被控 `allowed: false` → `409 assist_unavailable`；非 `trusted` 且需要访问码而请求未带 `pake` → `409 assist_code_required`（`details.mode`）并推送 `assist.cancelled`；被控未应答 → `408 timeout`。
4. **主控**：`trusted` 时直接 `accept`（`key_confirm` 为空串）。否则逐槽计算 K 并核对 `key_confirm`；都不匹配即访问码错误：本地报错并 `POST …/cancel`。匹配（或仅确认模式）则 `POST /assist/requests/{id}/accept {key_confirm: HMAC(K, "FWAK2\ncontroller\n"+request_id)}`（长轮询，最多 65 秒；确认模式为空串）。
5. **服务端**推送 `assist.accept{request_id, key_confirm}`；被控对受信任的请求重新核对该设备仍受信任，对访客校验确认值并在需要时弹出本机确认，然后 `POST …/decision`，主体 `AssistDecision{allowed, refusal?, trusted}`，`refusal ∈ code_invalid | code_required | rejected | unavailable`。
6. 允许：服务端建立会话（`purpose = desktop`，`connect_type = assist`，`decision.trusted` 时 `relation = trusted`、否则 `assist`，`grants` 为空：被控套用为该受信任设备记录的能力或访客默认权限），返回 `AssistJoin` = `SessionTicket` 字段（`target.relation` 同上）+ `host`。拒绝映射：`code_invalid → 403 assist_code_invalid`，`code_required → 409 assist_code_required`，`rejected → 403 assist_rejected`，`unavailable → 409 assist_unavailable`。被控未决定 → `408 timeout` 并推送 `assist.cancelled`；请求已被取消 → `409 conflict`。
7. 使用访问码的会话双方 SDP 增加 `a=x-farwing-pake`（§2.4）；被控按会话保存 K，重连同一会话时继续使用。

- 被控已应答但主控 120 秒内未 `accept` 的请求过期，服务端推送 `assist.cancelled`。`accept` 与 `cancel` 只对发起该请求的主控（同一账号或同一无账号设备）有效。
- 限速：每次 `join` 预先计数（每主控 + 目标每分钟 `assist_failures_per_minute` 次，主控为账号、无账号时为设备 `device:<id>`，目标为短号或设备 ID；每目标跨主控每小时 50 次），只有最终建立会话才返还；请求到达被控之前的失败不计数。被控另在本地计数（v2-design §2）。
- 这类会话不经在线连接承载信令（总是签发入场券）；断线重建用 `POST /sessions/{id}/ticket`。
- 最近列表只为有账号的主控、且被控有短号时记录；最近与收藏接口只对账号有效。
- `SavedDevice`：`{publisher_device_id, connect_id, remark, is_favorite, last_connected_at, favorited_at, public_key}`；`public_key` 为被连接设备的公钥（设备已不存在时为空），主控据此显示其设备 ID。

### 3.12 好友与授权

- 能力名（`caps`）：`view`、`control`、`clipboard`、`files`、`ports`、`power`、`audio`、`microphone`、`display`、`diagnostics`、`annotation`（含义见 v2-design §3.2）。未知名称被丢弃。
- `Friend = {user: UserSummary, since}`；`FriendRequest = {id, user, created_at}`（`user` 为对方）。
- `POST /friends/requests {username}`：对方已向我申请时直接成为好友并返回 `{"accepted": {"friend": …}}`，否则 `{"sent": {"request": …}}`；未知用户 `404 user_not_found`，已是好友 `409 already_friends`，申请自己 `400`；每账号每小时 20 次。
- 成为好友时双方各自自动授予对方“所有设备 = `view`, `annotation`”。
- `Grants = {default: [caps], devices: {device_id: [caps]}}`：某好友在某设备上的能力 = 该设备的条目，否则 `default`。`PUT` 整体替换；设备键必须是本账号设备；只对好友有效（否则 `404 not_found`）。
- 授权变化时：好友在相关设备上的活动会话收到 `session.permissions{grants}`（被控与主控都收到）；不再具备会话目的所需能力时会话以 `permission_revoked` 结束。解除好友删除双向授权并同样结束会话。
- `GET /devices/self/friend-access`（`path::SELF_FRIEND_ACCESS`）：调用设备可以被哪些好友设备控制，`FriendAccessList = {devices: [FriendAccess]}`，`FriendAccess = {device_id, public_key, alias, user_id, nickname, grants}`，列出在调用设备上持有非空授权的每位好友（未禁用账号）的全部设备，`grants` 为该好友在调用设备上的能力。被控缓存它，在直连时按 `friend` 方法授权（§8.3）。
- 推送：`friend.changed{kind, user}`，`kind ∈ request_received | request_accepted | request_rejected | request_cancelled | removed | grants_changed`；可见性变化以 `device.changed`（`shared: true`）告知。

### 3.14 多人转发

服务器开启多人转发时，`ServerInfo.features.forwarding = {threshold, modes}`：观看人数达到 `threshold` 时被控可以改为经转发器发送（`deployment-design.md` §5），`modes` 为允许的模式（`e2ee` 总在其中，`compatible` 由管理员决定）。

- 被控 `POST /sfu/rooms {sessions, mode, offer}`：`sessions` 为它当前承载的会话（任一不是则 `404`），`offer` 只含发送的媒体（每路编码一个视频轨、共享的系统声音一个音频轨，候选收集完毕），扩展与负载类型取自共享表（`crates/rtc-wire`）。回执 `answer` 与房间 ID；`node` 为转发所在的边缘节点名称（缺省为本服务器）。拒绝：`403 forwarding_disabled`、`403 forwarding_mode_not_allowed`、`503 forwarding_unavailable`。
- `PUT /sfu/rooms/{room} {sessions?, offer?}`：`sessions` 替换可订阅的会话（被移出的会话随即退订）；`offer` 重新发布（轨道变化），所有订阅结束，观看者需重新订阅。`DELETE` 关闭房间。会话结束后自动移出房间，最后一个会话结束时房间关闭。
- 主控 `POST /sfu/rooms/{room}/subscribe {session, connection, offer, tracks?}`：只能订阅自己作为主控的会话（否则 `404`）。`connection` 是主控在这次会话中的第几条转发连接（从 1 起）：同一会话可以同时有两条（新连接接上前旧的继续收），同一编号再次订阅替换之前的那条。`offer` 只收不发；`tracks` 按每种媒体的段落顺序指定要收的轨道 ID（缺省按发布顺序）。回执 `answer` 与房间的 `mode`。`DELETE /sfu/rooms/{room}/subscribe/{session}/{connection}` 退订其中一条。
- 转发器原样传递被控的 RTP 包（SSRC、序号、负载类型与头扩展不变，只按连接重写 transport-cc 序号），回应观看者的 NACK、合并关键帧请求（每路每 300 ms 一次；新连接建立后为它的视频轨另请求一次）、转发发送端报告。它每 250 ms 在上行连接上向被控发一个复合 RTCP 包（SSRC 0）：REMB 为转发流的上限（没有上限时为 10¹² bit/s），随后的 APP 包（名称 `FWLO`）列出应改看低码率一路的会话 ID（以换行分隔、补零到 4 字节；`deployment-design.md` §5.4）。转发连接只提供主机候选（共用 UDP 端口，默认 3481），客户端直接或经自己的中转连到它，不需要交换候选。
- 被控如何告诉观看者房间 ID 与轨道，见 §6.7。

### 3.13 电源与网络唤醒

- `POST /devices/{id}/power {action: "wake"|"reboot"|"shutdown"}`：本账号设备总是允许，好友需要 `power` 能力（`403`）。
- `reboot` / `shutdown`：目标须在线，具备 `power` 能力且 `policy.power` 为真（`409 host_disabled`）；服务端推送 `power.request{request_id, action, requester{user_id, username, nickname}, requester_device}`，回执 `{request_id}`。目标执行后 `POST /power/{request_id}/result {code, message, state}`（只能由目标设备提交，否则 `403`）。
- `wake`：目标须离线（`409 conflict`）且 `wol_supported`（`403`）。服务端向目标主人的、与目标登记的公网地址相同的其他在线设备推送 `wol.wake{request_id, target{mac, inner_ip, subnet_mask}}`；请求设备与目标在同一局域网（公网地址相同且登记的内网地址在目标子网内）时，回执带 `current_device_in_same_network: true` 与 `target`，由它自己发送魔术包。回执：`{request_id, current_device_in_same_network, assist_count, target}`。
- `PUT /devices/self/wol`：MAC、内网地址与掩码有效时 `supported: true`；服务端同时记录请求的来源公网地址。

---

## 4. 实时面 WSS

地址为入场响应中的 `endpoints`（`wss://<public_url 主机与路径>/api/v2/ws`）。主机与客户端保存的服务器地址不同时，客户端确认 `https://<该主机与端口>` 可以连上（信任方式不变）后改用它作为服务器地址（公开地址在客户端离线时改过，`server.changed` 没有收到），否则拒绝连接。所有帧为 UTF-8 JSON 文本帧；二进制帧以 `4008` 关闭。

### 4.1 建立连接

升级请求头：`X-Farwing-Protocol: 2`，以及二选一：

- `X-Farwing-Ticket: fwt_…`：首次连接（单次有效，60 秒）。被控入场券对应在线连接（`role: host`），会话入场券对应该会话的主控连接（`role: controller`）。
- `X-Farwing-Resume: fwr_…`：断线恢复。

入场券或恢复令牌无效时返回 HTTP `401 unauthorized`（升级后才发现时以 `4401` 关闭；会话入场券对应的会话已结束时以 `4004` 关闭）。兑换被控入场券时服务端重新检查其账号/游客凭据仍有效。连接建立后服务端先发送：

```json
{"v":2,"type":"hello","data":{
  "conn_id":"uuid","role":"host"|"controller","epoch":7,"session_id":"(仅主控连接)",
  "resume_token":"fwr_…","heartbeat_ms":20000,"idle_timeout_ms":60000,
  "limits":{"max_frame_bytes":1048576,"max_sdp_bytes":1048576,"max_candidates":256,"rate_per_s":200},
  "resumed":false,"server_time_ms":0}}
```

恢复时 `resumed: true`：逻辑连接（在线状态、会话、承载关系）保留，但**不**重放断线期间的消息；双方重新发送当前快照（被控重新发送 `session.setting{kind:"capability"}`）。新的被控在线连接（非恢复）取代旧的（旧连接以 `4001` 关闭），递增 `epoch` 并以 `host_offline` 结束旧连接上的被控会话。

在线连接（`role = host`）的 `hello` 可带 `probe_nodes: [{id, stun}]`：服务器的边缘中转节点及其 STUN 端点（`host:port`）。设备向每个端点并行发 3 次 STUN Binding（间隔 100 ms，最多等 2 秒），取最小往返，以 `nodes.rtt {rtt: {<节点 ID>: 毫秒 | null}}` 在同一连接上报告，连接存续期间每 30 分钟重测一次。服务器据此为会话两端各选最近的节点（farwing-protocol 之外的说明见 deployment-design §4.5）；中转凭据的用户名带节点 ID：`<过期>:<会话 ID>:<节点 ID>`。

### 4.2 信封

```json
{"v":2,"type":"rtc.offer","id":"c-12","re":null,"sid":"session uuid","gen":1,"data":{}}
```

| 字段 | 说明 |
|------|------|
| `v` | 协议主版本，必须为 2 |
| `type` | 消息类型 |
| `id` | 可选；发送方唯一的请求标识。带 `id` 的客户端消息由服务端回复 `ack`（`re` = 该 `id`）或 `error` |
| `re` | 回复所对应的 `id` |
| `sid` | 会话 ID（会话消息必需） |
| `gen` | 协商代次（`rtc.*`，§1.4） |
| `data` | 消息体 |

`ack`：`{"v":2,"type":"ack","re":"c-12","data":{…}}`；`error`：`{"v":2,"type":"error","re":"c-12","data":{"code","message","retryable"}}`，`code` 同 §3.3。`session.start` 与 `resume.refresh` 必须带 `id`，发送方 10 秒超时。

### 4.3 心跳、恢复与重连

- 服务端每 `heartbeat_ms` 发送 `ping{t}`；任一方收到 `ping` 立即回 `pong{t}`。服务端在 `idle_timeout_ms` 内未收到任何帧即以 `1001` 关闭；客户端在 `heartbeat_ms + idle_timeout_ms`（至少 5 秒）内未收到任何帧即判定失效并重连。
- 客户端每 30 秒检查一次，恢复令牌超过 300 秒未刷新时发送 `resume.refresh{}`（带 `id`），`ack.data.resume_token` 为新令牌。
- 物理连接断开（未 `bye`）后逻辑连接保留 300 秒：被控仍算在线，会话保留。过期后被控会话以 `host_offline`、主控会话以 `controller_left` 结束。
- 重连退避 `retry_delay_ms × 1.5^n`，上限 25 秒；重试预算为 `endpoints` 数 × 2（至少 2），连接成功后清零；只能用恢复令牌重连（入场券已用掉）。物理重连期间媒体连接保留。
- `bye{}`：主动离开。主控连接上表示离开其会话（`controller_left`），在线连接上表示下线（承载的主控会话以 `controller_left`、被控会话以 `host_offline` 结束），服务端以 `1000` 关闭。

### 4.4 关闭码

| 码 | 含义 | 客户端处理 |
|----|------|-----------|
| 1000 | 正常关闭（`bye`、主控会话以 `controller_left` 结束） | 结束 |
| 1001 | 空闲超时、服务端关闭 | 重连 |
| 4001 | 被同设备的新连接取代（含会话改由在线连接承载、恢复到新连接） | 结束，不重连 |
| 4003 | 凭据被吊销（注销、设备移除、账号禁用、改绑账号） | 结束，退休账号代次 |
| 4004 | 会话已结束（主控连接） | 结束 |
| 4008 | 协议错误（非 JSON、缺少 `v`、信封无效、二进制帧） | 重连 |
| 4009 | 版本不支持 | 结束，提示升级 |
| 4029 | 超出速率限制 | 退避后重连 |
| 4401 | 入场券或恢复令牌无效/过期 | 结束，重新获取入场券 |
| 1006/1011 等 | 网络中断、服务器重启 | 重连 |

### 4.5 消息类型

通用：`hello`（S→C）、`ping`/`pong`（双向）、`ack`/`error`（S→C）、`resume.refresh`（C→S）、`bye`（C→S）。服务端收到不认识的类型回 `error{bad_request}`，不关闭连接。

| type | 方向 | 连接 | data |
|------|------|------|------|
| `session.start` | C→S（带 `id`、`sid`） | 主控连接，或承载该会话的在线连接 | `{attempt_id, options: ConnectOptions, capability, purpose?}` → `ack: SessionStarted` |
| `session.incoming` | S→H | 被控在线连接，带 `sid` | `SessionIncoming` |
| `rtc.offer` | C→S→H | 带 `sid`、`gen` | `{attempt_id, restart, network, sdp}` |
| `rtc.answer` | H→S→C | 带 `sid`、`gen` | `{attempt_id, restart, network, sdp}` |
| `rtc.candidate` | 双向 | 带 `sid` | `{attempt_id, candidate: {candidate, sdpMid, sdpMLineIndex, usernameFragment}}` |
| `session.setting` | H→S→C | 带 `sid` | `{kind: "capability", capability}` / `{kind: "sender_info", infos: [{video_track_index, capture_impl, encoder_impl}]}` |
| `session.permissions` | S→H、S→C | 带 `sid` | `{grants: [caps]}` |
| `session.relay_lost` | S→H、S→C | 带 `sid` | `{ice_servers}`：服务器的内置中继重启时会话在它上面有分配；双方换用这些中继（换密钥时为新凭据），经中继的主控立即重启 ICE |
| `session.end` | H→S | 被控在线连接，带 `sid` | `{reason}` |
| `session.leave` | C→S | 承载该会话的在线连接，带 `sid` | `{}` |
| `session.ended` | S→C、S→H | 带 `sid` | `{reason}` |
| `device.changed` | S→在线连接 | | `{kind: "added"\|"updated"\|"removed", device: Device, shared}` |
| `friend.changed` | S→在线连接 | | `{kind, user: UserSummary}`（§3.12） |
| `assist.join` | S→H | 在线连接 | `{request_id, controller: ControllerInfo, pake}` |
| `assist.accept` | S→H | 在线连接 | `{request_id, key_confirm}` |
| `assist.cancelled` | S→H | 在线连接 | `{request_id}` |
| `power.request` | S→H | 在线连接 | `{request_id, action, requester, requester_device}` |
| `wol.wake` | S→协助设备 | 在线连接 | `{request_id, target: {mac, inner_ip, subnet_mask}}` |
| `server.changed` | S→所有在线连接 | | `{url, previous, sha256?}`：公开地址或服务器自己的证书变了。保存的地址是 `previous`（或 `url`）的客户端确认 `url` 可以连上后改用它；固定了证书指纹的改为 `sha256`（反向代理时没有）。账号与设备登记不变 |

- `SessionStarted`（`session.start` 的 `ack`）：`{participant_id, session_id, ice_servers, force_relay, host: {device_id, public_key}, relation, grants}`。
- `SessionIncoming`：`{participant_id, attempt_id, purpose, connect_type: "account"|"assist", relation: "own"|"friend"|"trusted"|"assist", grants, controller: ControllerInfo, options, capability, ice_servers, force_relay}`。目的、类型、关系、授予的能力与主控身份以服务端为准；`options` 中的同类信息不作授权依据。经服务端时 `own`、`friend` 的 `connect_type` 为 `account`，`trusted`、`assist` 为 `assist`（§3.11）。`grants`：本账号设备为全部能力，好友为生效授权，受信任设备与访客为空：被控对 `trusted` 重新核对本机受信任设备记录（设备 ID 与公钥都须匹配，否则拒绝）并套用为它记录的能力，对 `assist` 套用访客默认权限。
- `ControllerInfo`：`{device_id, alias, user_id, username, nickname, platform, version, public_key, caps_limit?}`；无账号的主控 `user_id`、`username`、`nickname` 为空。`caps_limit` 是服务端规定的能力上限：网页主控（`platform: "web"`）总是带（服务器设置“网页端可以使用的功能”），被控对所有关系的授权都按它裁剪；原生主控不带，不受此限制。
- `IceServer`：`{urls, username, credential}`。TURN 凭据为 TURN REST 临时凭据：`username = "<过期 Unix 秒>:<session_id>"`，`credential = base64(HMAC-SHA1(shared_secret, username))`，默认 12 小时。NAT 探测端点以无凭据的 `stun:` 地址附加在列表中。
- `network`（`rtc.offer`/`rtc.answer`）：`3` 全部候选；`1` 仅中继（服务器提供的全部中转方式：UDP、TCP、TLS）。主控在观看窗口切换线路时以重启 offer 发送；被控按它重新配置 ICE，其他取值不改配置。中继与直连之间的自动切换由 ICE 按实测路径质量完成（§7.5），不经信令。
- `session.start.purpose` 只供直连端点使用（§8.4），服务端忽略它。

**客户端约定的载荷（服务端只校验大小并转发）**

- `ConnectOptions`：`{screen_id, capture: CaptureParams?, decoders: [DecoderCapability], force_virtual, virtual_modes: [{width, height, vsync}], virtual_initial: {width, height}?, version}`。
  - `CaptureParams`：`{fps(1:30 2:60 3:90 4:144), quality(1 低码率 2 高清 3 超清 4 原画 5 自动 6 自定义), cursor_capture, resolution_type, local_resolution, chosen_resolution, chroma(1 4:2:0 3 4:4:4), bitrate(bit/s，0 自动), hdr, auto_quality, fps_count, weak_network(0 先降码率 1 先降帧率), cinema, codec(0 由被控选择，1 H.264 2 H.265 5 AV1：双方都支持时会话从这种编码开始)}`。
  - `DecoderCapability`：`{fps, codec(1 H.264 2 H.265 5 AV1), width, height, chroma}`。
- `capability`（`session.start` 与 `session.setting{capability}`，`src/protocol/capability.rs`）：`{preferred_codec?, sdr_10bit, session_id, display_info: [{id, fps, type, hdr}], video_codec_capability: [{video_codec, width, height, chroma_sampling, bit_depth, codec_impl}]}`。

### 4.6 会话流程与服务端校验

1. 主控通过 HTTPS 创建会话（§3.10），用会话入场券建立主控连接（或在在线连接上，§4.7），发送 `session.start`。
2. 服务端检查被控在线且仍是会话创建时的 `epoch`（否则 `error{device_offline}`），游客被控只接受 `connect_type = assist` 的会话（否则 `forbidden`）；记录 `attempt_id`、清零候选计数，向被控推送 `session.incoming`，向主控回 `ack`。同一会话可多次 `session.start`（会话重建），每次更换 `attempt_id`。
3. `rtc.*` 与 `session.setting` 只在会话双方之间转发：主控只能发 `rtc.offer`/`rtc.candidate`，被控只能发 `rtc.answer`/`rtc.candidate`/`session.setting`（否则 `forbidden`）；`sid` 必须是该连接所属的会话；会话尚未开始时 `conflict`；`attempt_id` 不是当前尝试时 `conflict`；SDP、候选大小与候选数超限时 `payload_too_large`。带 `id` 的转发消息在转发后回 `ack {}`。服务端转发时去掉 `id`/`re`，保留 `sid` 与 `gen`。
4. 被控在收到 `session.incoming` 后即可应答 offer（媒体轨道预先协商）；SDP 带 §2.4 与 §7.2 的会话级属性。
5. 结束：被控 `session.end{reason}`（服务端只转告 `revoked`、`kicked`、`identity_mismatch`、`media_failed`，其他按 `host_closed`）；主控 `bye`（主控连接）或 `session.leave`（在线连接）。

### 4.7 在线连接承载主控信令

主控进程已有本设备、本账号且处于连接状态的在线连接时，会话信令复用它：

- `POST /sessions` 带 `"presence": true`，不签发入场券。
- 在线连接上的 `session.start` 必须带 `sid`，且该会话的主控设备是本连接的设备、创建会话的账号与本连接的账号相同（否则 `error{forbidden}`）。此后该会话的 `rtc.offer`/`rtc.candidate` 从这条连接接受，发往主控的 `rtc.answer`/`rtc.candidate`/`session.setting`/`session.permissions`/`session.ended` 也发往这条连接。会话原来的主控连接（以入场券建立的）以 `4001` 关闭。
- 主控离开发送 `session.leave{}`（带 `sid`）；会话结束时只发 `session.ended`，不关闭在线连接。在线连接 `bye`、被新连接取代、恢复窗口过期或凭据吊销时，它承载的主控会话以 `controller_left` 结束；暂时断开（可恢复）时保留。
- §3.11 的会话（受信任设备与访客）总是使用单独的主控连接。

### 4.8 会话结束原因

| 原因 | 触发 | 主控收到 | 被控收到 |
|------|------|----------|----------|
| `replaced` | 同一主控设备对同一被控新建会话 | ✓（关闭码 4004） | ✓ |
| `host_closed` / `revoked` / `kicked` / `identity_mismatch` / `media_failed` | 被控 `session.end` | ✓（4004） | — |
| `host_offline` | 被控在线连接取代、`bye` 或恢复窗口过期 | ✓（4004） | — |
| `controller_left` | 主控 `bye`、`session.leave`、主控连接恢复窗口过期、承载的在线连接结束 | —（主控连接以 1000 关闭） | ✓ |
| `revoked` | 主控或被控的凭据被吊销 | ✓（主控凭据吊销时 4003，否则 4004） | ✓（被控自身被吊销时不发） |
| `permission_revoked` | 好友授权不再允许会话目的、解除好友 | ✓（4004） | ✓ |
| `expired` | 入场券有效期内未开始（通常双方都还没有连接） | 已连接时 ✓ | 已开始时 ✓ |
| `server_shutdown` | 服务端关闭 | ✓（连接以 1001 关闭） | — |

被控只在会话已开始（收到过 `session.incoming`）后收到 `session.ended`。由在线连接承载的主控会话收到 `session.ended` 但连接不关闭。

### 4.9 会话状态（服务端）

```
POST /sessions        session.start            首个 rtc.answer
───────────► ticketed ─────────────► started ────────────────► active
                │ 60 秒内未开始             │ 任何结束原因（§4.8）
                ▼                           ▼
             expired ──────────────────► ended（从设备 participants 中移除）
```

会话与实时连接状态只在内存中；服务端重启结束所有会话（`server_shutdown`），恢复令牌随之失效。

---

## 5. 对端协议 FPP v2

### 5.1 帧格式

```
0      2       3       4        6               10
+------+-------+-------+--------+---------------+------------------+
| "FW" | ver=2 | flags | type   | request id    | body …           |
| 2 B  | 1 B   | 1 B   | u16 BE | u32 BE        |                  |
+------+-------+-------+--------+---------------+------------------+
```

- `flags`：bit0 `RESPONSE`（0x01），bit1 `ERROR`（0x02），bit2 `MORE`（0x04，分片未完）；其余为 0。
- **请求**：`request id` 非 0，由发送方在本连接内分配（递增）。**响应**：同一 `request id` 并置 `RESPONSE`；失败时置 `RESPONSE|ERROR`，帧体为 `PeerError`。**推送**：`request id` 为 0，不回复。
- 帧体编码由类型决定：大多为 protobuf（[peer-messages.md](peer-messages.md)），`POINTER_POLICY` 与 `AUDIO` 为 JSON。
- `PeerError{code, message, detail}`，`code ∈ unsupported | bad_request | forbidden | not_found | busy | unavailable | internal | timeout`。收到 `ERROR` 回复的一方按“请求失败”处理。
- 接收方按帧头类型分发，不嗅探帧体；通道只决定服务质量。魔数或版本不符、超过 512 KiB 的数据被丢弃。未知帧类型被忽略；SESSION 中不认识的请求回 `RESPONSE|ERROR{unsupported}`。

### 5.2 分片

请求与响应的帧体超过单帧预算时，拆成同一 `request id`、同一方向的连续帧，除最后一帧外置 `MORE`；推送不分片。接收方按 `(request id, 是否响应)` 重组（同一 id 的请求与响应可以交错）；每个接收方同时未完成的分片消息最多 16 个，重组后的大小受各特性上限约束（CLIPBOARD 48 MiB、FILE 32 MiB）。分片预算：CLIPBOARD 60 KiB，FILE 64 KiB − 10。

### 5.3 数据通道

主控创建全部通道（带内 DCEP，非预协商），被控按标签接受；被控收到其他标签的通道不使用。

| 标签 | 可靠性 | SCTP 优先级 | 帧类型 |
|------|--------|------------|--------|
| `farwing.control.v2` | 有序可靠；协商 FCT 后经 FCT（§5.6） | 高 | HELLO、INPUT、POINTER、CURSOR、POINTER_POLICY；以及主控声明 `display.config` 时被控的 SESSION `Screens` 推送、主控的 SESSION `VideoFeedback`（§6.6） |
| `farwing.session.v2` | 有序可靠 | 中 | SESSION、AUDIO、HOST_EVENT、CLIPBOARD、FILE、DRAG、ANNOTATION、DIAGNOSTICS、PERMISSIONS、PARTICIPANTS |
| `farwing.bulk.v2` | 有序可靠 | 高 | TRANSFER、FLOW（文件、剪贴板、拖放、诊断包共用） |
| `farwing.tunnel.v2` | 有序可靠 | 低 | PORT、FLOW（`channel = TUNNEL`） |
| `farwing.dgram.v2` | 无序，`maxRetransmits = 0` | 低 | PORT_DGRAM |

- 主控在 SDP 中声明 `a=max-message-size:524288`。
- bulk 通道的所有发送方共用一个写出闸门：同一个 `on_buffered_amount_low` 回调（低水位 128 KiB）唤醒所有等待者，空间检查与入队串行进行，合计在 SCTP 中排队不超过 256 KiB；单帧 30 秒内无法入队则该发送失败。tunnel 通道同样限制在 256 KiB；dgram 排队超过 1 MiB 时丢弃新数据报。

### 5.4 帧类型

| 类型 | 值 | 通道 | 方向 | 帧体 | 用途 |
|------|----|------|------|------|------|
| `HELLO` | 0x0001 | control | 双向，请求/响应 | `Hello` | 版本与特性协商（§5.5） |
| `ERROR` | 0x0002 | 任意 | — | `PeerError` | 保留；错误通常以 `RESPONSE\|ERROR` 回复，收到此类型只记录 |
| `INPUT` | 0x0101 | control（FCT） | C→H 推送 | `InputMessage` | 键盘、锁定键、鼠标、系统命令、保活 |
| `POINTER_POLICY` | 0x0102 | control | H→C 推送 | JSON | 指针模式（绝对 / 相对 / 恢复） |
| `CURSOR` | 0x0103 | control | H→C 推送 | `CursorMessage` | 被控光标形状与隐藏；其他参与者的指针 |
| `TOUCH` | 0x0104 | control（FCT） | C→H 推送 | `Touch` | 触摸：本参与者此刻的全部触点，每个带自己的阶段；空表取消全部 |
| `PEN` | 0x0105 | control（FCT） | C→H 推送 | `Pen` | 笔：1–16 个样本（压力、倾斜、旋转、笔杆键、橡皮擦、悬停） |
| `POINTER` | 0x0106 | control | C→H 推送 | `Pointer` | 参与者指针（观看者也发送，不注入输入） |
| `SESSION` | 0x0201 | session（`Screens` 见 §5.3） | 双向 | `SessionMessage` | 屏幕、画质、采集、显示设置、麦克风、接收统计 |
| `AUDIO` | 0x0202 | session | 双向 | JSON | 桌面声音码率 |
| `HOST_EVENT` | 0x0203 | session | H→C 推送 | `HostEvent` | 被控更新、重启、睡眠、关机、会话切换、显示设置变化 |
| `CLIPBOARD` | 0x0301 | session | 双向 | `ClipboardMessage` | 剪贴板控制面，内容走 TRANSFER |
| `FILE` | 0x0401 | session | C→H 请求，H→C 响应/推送 | `FileMessage` | 远程文件管理与传输控制面 |
| `TRANSFER` | 0x0402 | bulk | 流的发送方 → 接收方 | `Chunk` | 统一数据块（§5.7） |
| `DRAG` | 0x0501 | session | 双向 | `Packet` | 原生拖放控制面，文件数据走 TRANSFER |
| `ANNOTATION` | 0x0601 | session | 双向 | `Packet` | 批注 |
| `PORT` | 0x0701 | tunnel | 双向 | `PortMessage` | 端口转发控制与 TCP 数据 |
| `PORT_DGRAM` | 0x0702 | dgram | 双向 | `Datagram` | UDP 数据报 |
| `DIAGNOSTICS` | 0x0801 | session | 双向 | `DiagnosticsMessage` | 诊断包控制面，内容走 TRANSFER |
| `PERMISSIONS` | 0x0901 | session | H→C 推送 | `Permissions` | 本参与者生效的能力 |
| `PARTICIPANTS` | 0x0902 | session | H→C 推送 | `Participants` | 参与者列表与当前操作者 |
| `FLOW` | 0x0A01 | bulk / tunnel | 流的接收方 → 发送方 | `Flow` | 累计信用（§5.7） |

v1 的 `MOUSE_POLICY`、`AUDIO_QUALITY`、`FILE_BLOCK`、`DRAG_DATA`（0x0502）与 `farwing.diag` 诊断通道已删除。

### 5.5 HELLO 与特性

- 双方各自在 control 通道上发送 `HELLO` 请求，并以 `HELLO|RESPONSE`（带自己的 `Hello`）回答对方的请求：
  - 主控在 control 通道打开且 PeerConnection 已连接后立即发送，之后每 500 ms 重发，最多 30 次，直到收到响应；超时后串流设置不可用，画面继续播放。
  - 被控在 control 通道打开时发送一次。
- 被控只有在收到自己请求的响应、并且已回答主控的请求之后，才开始发布屏幕列表、画质、光标、权限、参与者等状态。
- `Hello{protocol_version: 2, min_protocol_version: 2, role, client_version, features, max_message_bytes: 524288, platform, profile?}`。`protocol_version ≥ 2` 且 `min_protocol_version ≤ 2` 才兼容，否则握手失败。主控可在 `profile.color` 中指定自己的指针颜色（0xRRGGBB，0 由被控分配）。
- `features` 为特性名 → 版本（当前均为 1），只声明运行时具备的；对端未声明视为不支持。门控只看特性名。两端同为 v2 构建、声明相同集合；被控在 HELLO 到达之前按完整的 v2 主控特性集合准备会话。
- v2 声明的特性：

| 类别 | 特性名 |
|------|--------|
| 画面与显示 | `video.h264`、`video.hevc`、`video.av1`、`video.444`、`video.hdr`、`display.config`、`display.virtual`、`display.exclusive`、`display.geometry` |
| 音频 | `audio.quality`、`audio.microphone` |
| 光标 | `cursor.shape`、`cursor.participants` |
| 数据 | `clipboard`、`clipboard.files`、`files`、`drag`、`annotation`、`ports.tcp`、`ports.udp`、`diagnostics` |
| 会话 | `permissions`、`participants`、`host_event` |
| 传输 | `fct` |
| 丢包恢复 | `video.recovery`（§6.6） |

`input.keyboard_usage`、`input.pointer` 已定义名称但当前不声明。`touch`、`pen`：被控只在运行时具备合成指针（Windows 10 1809 起）时声明，主控只向声明了的被控发送 TOUCH / PEN，否则用鼠标模拟。

### 5.6 FCT（Farwing Control Transport）

可选的控制快速通道，降低弱网下输入的队头阻塞：

- 双方 SDP 的 `m=application` 段都包含 `a=x-farwing-fct:1` 时启用（取 1 版本）。
- 在 DTLS 应用数据中与 SCTP 复用：记录以 4 字节 `"FCT"` + 版本 `0x01` 开头，其余记录交给 SCTP。接收方在 SCTP 之前按前缀分流，以 `"FCT"` 开头的记录不得交给 SCTP（未知版本直接丢弃）。
- 内部为 KCP（会话号 `0x46574354`，线上 MTU 1191，收发窗口 256 段），附带 Reed–Solomon（CM256）前向纠错与恢复信息命令；每条消息尾部附 2 字节 SCTP 流号与 2 字节消息类型（1 = 二进制）。冗余按块恢复失败概率 ≤ 1.5% 的最小修复段数（`src/transport/fec_policy.rs`）。
- 只承载 `farwing.control.v2` 的帧；未协商时 control 帧直接走 SCTP。

### 5.7 统一传输：TRANSFER 与 FLOW

- **流**：由各特性的控制面消息（FILE、CLIPBOARD、DRAG、DIAGNOSTICS）发起。`stream` 由发起请求的一方在本连接内分配，同一 `kind` 内唯一；`(kind, stream)` 决定接收方的所有者。收到 `TRANSFER` 的一方总是该流的接收方，收到 `FLOW` 的一方总是发送方。CLIPBOARD、DRAG、DIAGNOSTICS 的请求方即数据接收方，它先登记流再发请求，数据块可能先于（session 通道上的）响应到达。
- **条目**：一个流按清单顺序依次发送各条目；`entry` 为清单下标，`offset` 为条目内的位置，必须连续；条目的最后一块置 `last`（空条目只发一个空的 `last` 块）。单块数据不超过 64 KiB − 64。
- **信用**：按流累计（所有条目合计，只计数据字节）。接收方初始授予 4 MiB；数据真正交付（写入磁盘、交给剪贴板）后才归还，空出四分之一窗口时发送 `FLOW{credit: 累计允许的字节总数}`。一个窗口的数据在 500 ms 内交付完时窗口倍增，上限 64 MiB。发送方不得超出信用；数据不连续或超出信用是协议错误，接收方终止该流。
- **FLOW 的通道**：`channel = BULK(1)` 时带 `kind`；`channel = TUNNEL(2)` 用于端口转发（每个 TCP 流初始 4 MiB，写入本地套接字后归还）。
- `kind`：`FILE = 1`、`CLIPBOARD = 2`、`DRAG = 3`、`DIAGNOSTICS = 4`。

### 5.8 各帧的交互规则

消息字段见 [peer-messages.md](peer-messages.md)。

- **SESSION**：请求带非 0 的 `request id`，被控以同一 id 并置 `RESPONSE` 回复；推送不回复。推送 `Screens`、`Quality`、`Microphone`、`ClipboardFiles`（H→C）与 `Capture`、`ReceiverStats`（C→H，`ReceiverStats` 每秒一次）。请求 `CaptureSetting`、`CreateVirtualDisplay`、`RemoveVirtualDisplay`、`EnterExclusive`、`QuitExclusive`、`VideoTracks`、`MicrophonePolicy`、`ControlTurn`（C→H）。显示变更需要 `display` 能力，麦克风需要 `microphone` 能力，获得控制权需要 `control` 能力。被控在屏幕列表或任一屏幕变化时发送 `Screens`（内容相同不重发）。
- **INPUT**：键按物理位置传 HID usage；锁定键状态以 `LockState` 纠正；系统命令只由主控菜单显式发送，被控先释放该参与者按住的键和按钮再执行。有键按下时主控在 50 ms 内没有发出任何 INPUT 帧就发一次 `Heartbeat`；被控在 `max(150 ms, 3×RTT + 50 ms)`（RTT 上限按 3 秒计，无 RTT 时取 150 ms）内收不到该参与者的 INPUT 即释放其按住的键。
- **多人**：每位参与者各自一个 PeerConnection。同一时刻只有一位参与者持有控制权：开启键鼠控制时以 SESSION `ControlTurn` 获取，先到先得（重连后的自动恢复只在独自一人时生效）；只在持有者关闭键鼠控制、离开或失去 `control` 能力时释放。被控只注入持有者的键鼠，其他人的键鼠被丢弃。其他参与者共用一路多人观看流（1080p、60 fps、观影模式，见 v2-design §4.4）。被控以 `PARTICIPANTS` 广播列表与控制权持有者（列表或持有者变化时），以 `CURSOR`（`pointers = true`）把其他参与者的指针转发给每位参与者（接收方空闲 3 秒后淡出）。
- **PERMISSIONS**：握手完成后与生效能力变化时由被控发送（服务端 `session.permissions`、被控本机开关或“禁止控制”）；生效能力 = 授予能力（本账号设备与好友由服务端给出，直连时由被控按缓存；受信任设备为被控为它记录的能力；访客为访客默认权限）∩ 被控本机上限。主控据此启停文件、剪贴板等功能。
- **HOST_EVENT**：被控在本机更新（`UPDATING`）、远程重启/关机前（`RESTARTING` 预计 90 秒 / `SHUTTING_DOWN`，约 400 ms 后以 `host_closed` 结束会话）、即将睡眠（`SLEEPING`）、活动 Windows 会话变化（`SESSION_SWITCH`，预计 5 秒）时发送；参与者修改显示设置后被控向其他参与者发送 `DISPLAY_CHANGING`（`expected_downtime_ms = 2000`，`message` 为提示文字）。
- **AUDIO**：被控发送当前音质 `hello`；主控以 `set{request, kbps}` 请求修改，被控回 `applied` 或 `rejected`（`request` 关联请求，不使用 FPP 的 `request id`）。
- **CLIPBOARD**：两端都可发请求。内容按需渲染：先推送 `Offer`（格式清单），应用粘贴时才 `Fetch`；内容 ≤ 32 KiB 在 `Data.inline` 中，更大时以 `TRANSFER{kind: CLIPBOARD, stream: 请求中的 stream, entry: 0}` 发送，按请求方的 `FLOW` 信用（单个格式最多 128 MiB；30 秒无进展即放弃并推送 `Abort`）。文件以 `ListFiles` 固定清单、`ReadFile` 按下标读取（每次 ≤ 1 MiB），结束后 `Release`。自动同步：被控只对单人会话中拥有 `clipboard` 的参与者、或多人会话中同时拥有 `clipboard` 与 `control` 的参与者这样做；主控只在控制模式且开启“剪贴板同步”时自动同步。`Pull`（C→H）让拥有 `clipboard` 的参与者手动获取被控当前剪贴板。失败码：`not_found`、`forbidden`、`unavailable`、`bad_request`、`internal`。
- **FILE**：主控发请求，被控以 `RESPONSE` 回复，失败回复 `Failed`（`RESPONSE|ERROR` + `PeerError` 按 `Failed` 处理）；请求 60 秒未获响应视为失败。数据走 `TRANSFER{kind: FILE, stream: transfer}` 与 `FLOW{channel: BULK, kind: FILE}`。上传：`Upload` → `Plan` → `UploadStart` → 主控发送；下载：`Download` → `Manifest` → `DownloadStart` → `Accepted` → 被控发送。续传以接收方已有的字节数与末尾（≤ 1 MiB）的 SHA-256 核对。被控的磁盘操作在被控用户的普通权限进程中执行，拒绝 `..`、设备名、链接与重解析点。
- **DRAG**：每次拖放有独立的文件交换，不读写系统剪贴板：`Packet.Files` 中携带一个完整的 CLIPBOARD 帧（`Offer`/`ListFiles`/`Files`/`ReadFile`/`Data`/`Release`/`Abort`），文件内容为 `TRANSFER{kind: DRAG}` / `FLOW{kind: DRAG}`。只接受文件格式（`Fetch`、`Pull` 回复 `forbidden`）。单个 DRAG 帧不超过 64 KiB。能力协商：被控发 `Hello(1)`，主控回 `HelloAck(1)`，`Packet.capabilities` 中的 `NATIVE_RETURN` 表示支持拖回主控。
- **ANNOTATION**：被控在 session 通道打开时发送 `Hello(1)`（带本连接的 `token`）；主控以 `Command` 请求绘制，被控以 `Result` 回复（`request` 关联，不使用 FPP 的 `request id`）。单帧不超过 512 KiB。
- **PORT**：主控分配 `stream` 并发 `Open`；目标可以是任意地址，主机名在被控侧解析（超时 5 秒），TCP 连接超时 10 秒；被控只连接单播地址。TCP 两个方向各自 `ShutdownWrite`，`Reset` 中止（`CLOSED` 表示正常结束）。UDP 数据报走 dgram 通道，超出预算只丢弃单个数据报；UDP 关联空闲 60 秒结束。转发规则只存在于主控，不上线。需要 `ports` 能力与被控本机端口转发开关。
- **DIAGNOSTICS**：被控在 session 通道打开时与变化时推送 `Status{allowed}`（主控声明 `diagnostics` 且本参与者拥有 `diagnostics` 能力时为真）。主控登记流后发 `Request{stream}`，被控生成期间推送 `Progress`（约 200 ms 一次），以 `Ready{size, sha256, …}` 响应后以 `TRANSFER{kind: DIAGNOSTICS}` 发送；主控收到最后一块后核对大小与 SHA-256。被控只发送自己的脱敏诊断包，不接受路径。

---

## 6. 媒体

### 6.1 SDP 结构

- 主控 offer：`audio_0`（Opus，sendrecv，用于麦克风上行与桌面声音下行，即使不发声也带 msid/SSRC）+ 5 个 recvonly 视频 m-line + `m=application`（数据通道）。被控在 answer 中以 msid `video_N` 标明轨道序号，SESSION `Screens` 中的 `video_track_index` 与之对应；主控用 `VideoTracks` 声明已协商的轨道（0..4）。
- 会话级属性：`a=extmap-allow-mixed`、`a=x-farwing-identity`、`a=x-farwing-pake`（§2.4）、`a=x-farwing-nat`（§7.2）。`m=application` 段：`a=max-message-size:524288`、`a=x-farwing-fct:1`。

### 6.2 负载类型

| PT | 编码 | fmtp | 说明 |
|----|------|------|------|
| 111 | Opus/48000/2 | `minptime=10;stereo=1;useinbandfec=1` | 反馈 transport-cc |
| 96 / 97 | H.265 / RTX `apt=96` | | |
| 98 / 99 | H.264 / RTX `apt=98` | `level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f` | |
| 104 / 105 | AV1 profile 0 / RTX | `profile=0;level-idx=17;tier=0` | |
| 106 / 107 | AV1 profile 1（4:4:4）/ RTX | `profile=1;level-idx=17;tier=0` | |
| 36 | `x-farwing-rsfec`/90000 | `max-k=109;repair-window=10000000;rtx-as-source=1` | Farwing RS-FEC（§6.4） |
| 100 / 101 | RED / RTX、102 ULPFEC、35 FlexFEC-03 | | 仅主控接收端注册；Farwing 被控不发送 |

H.264 按 RFC 6184、H.265 按 RFC 7798（单 NAL 与 FU，不用 DONL）、AV1 按 AV1 RTP 规范打包。视频反馈：`nack`、`nack pli`、`ccm fir`、`transport-cc`、`rrtr`（主控另注册 `goog-remb`）。被控只注册它能编码的格式。

### 6.3 RTP 头扩展

音频只协商 transport-cc 与 mid；视频协商下表 8 项。共 8 个扩展 URI（音频的两项与视频共用 ID），始终使用一字节扩展头（ID 1–14）。

| URI | 用途 |
|-----|------|
| `http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01` | transport-cc 序号（拥塞控制） |
| `urn:ietf:params:rtp-hdrext:sdes:mid` | BUNDLE 路由 |
| `http://www.webrtc.org/experiments/rtp-hdrext/video-timing` | 帧内各阶段计时 |
| `urn:farwing:rtp-hdrext:sending-delay` | 发送端排队时延 |
| `http://www.webrtc.org/experiments/rtp-hdrext/color-space` | 色彩空间（HDR） |
| `urn:farwing:rtp-hdrext:capture-index` | 采集源序号，解码端据此切源 |
| `urn:farwing:rtp-hdrext:new-frame` | 新帧标记 |
| `urn:farwing:rtp-hdrext:frame-meta` | 每帧首包 6 字节：标志（bit0 关键帧、bit1 恢复帧、bit2 本帧标为长期参考、bit3 参考的是长期参考、bit4–5 槽号）、纪元 u8、帧号 u16 BE、参考帧号 u16 BE（§6.6） |

abs-send-time、toffset、playout-delay（播放缓冲由主控本地决定）、CVO、video-content-type、rid 不协商。视频方向（CVO）按 3GPP / WebRTC 标准含义解释（接收端需顺时针旋转的角度）；Farwing 被控发送已摆正的画面，不写 CVO。

### 6.4 前向纠错（RS-FEC）

- 每帧一个块：块内为该帧的 RTP 包（`k ≤ max-k`，序号跨度 ≤ 109），修复包紧跟该帧发出；`rtx-as-source=1` 时 RTX 恢复出的包也作为源包。源包中发送时才确定的扩展（transport-cc、sending-delay 全部，video-timing 前 7 字节以外的部分）置零后参与编码；每个分片为 `u16 BE 长度 + 原始 RTP 包`，补零到 `shard_size`。
- 修复包负载：

| 偏移 | 长度 | 字段 |
|------|------|------|
| 0 | 2 | `block_id`（u16 BE，随机起始、每块加 1） |
| 2 | 1 | `k`（源包数） |
| 3 | 1 | `m`（本块修复包数） |
| 4 | 1 | `repair_index`（`k + i`） |
| 5 | 4 | `protected_ssrc` |
| 9 | 2 | `shard_size` |
| 11 | 2 | `base_seq`（首个源包序号） |
| 13 | 2 / 6 / 14 | 源包掩码：跨度 ≤ 15 / ≤ 46 / ≤ 109；每段首位为“还有下一段”标志 |
| … | `shard_size` | CM256（GF(256) 系统 Reed–Solomon）修复分片 |

- 冗余：按独立丢包二项模型取使块恢复失败概率不超过目标的最小 `m`（普通块 1e-3；关键帧、RTT > 120 ms 或 `k ≥ 71` 时 1e-4），限制在块的约 5%–35%；RTT < 20 ms 的非关键帧只发下限。实际修复包数还受发送端保护预算（FEC 与重传共用）限制。
- 何时发送：只在拥塞控制测得的丢包率 ≥ 0.5 % 时发送，且只用富余带宽——媒体与重传低于本路带宽估计的 60 % 时，修复包补到 60 % 为止；编码器的码率不因 FEC 扣减。总发送量保持在估计的 60 % 以下，拥塞控制仍把发送端视为“没用满带宽”而继续探测（85 % 以下才算）。不发送时也不为修复包预留包长，媒体包与不使用 FEC 时相同。被控端 `FARWING_FEC=0` 关闭（只在被控端有效）。
- 修复流：被控 answer 以 `a=ssrc-group:FEC-FR-RS <媒体 SSRC> <修复 SSRC>` 声明。所有原生主控都在 offer 中提供 `x-farwing-rsfec` 并接收修复流；浏览器不提供这个编码，answer 中也就没有修复流。

### 6.5 RTCP 与拥塞反馈

- **NACK / RTX**：接收端每 20 ms 处理一次缺包，按 RTT 与后续序号重试（每包最多 17 次，退避底数 1.25，起始 50 ms；跟踪的包龄上限 10000 个序号、缺包上限 1000 个）；发送端以 RTX 重传（受保护预算限制）。
- **关键帧**：缺包超过 `max(2.5 × RTT, 60 ms)` 或 NACK 放弃时立即发 PLI，同一丢包事件的请求间隔 ≥ `max(100 ms, 1.5 × RTT)`；被控对 PLI/FIR 的节流相同。
- **拥塞控制**：接收端发送 transport-cc 反馈，间隔按“68 字节、发送端估计码率的 5%”计算并限制在 25–250 ms；被控以 GoogCC 计算目标码率、节奏与拥塞窗口。
- **RTT 与时钟**：接收报告（RR）；协商 `rrtr` 时接收端发 XR RRTR，发送端回 DLRR。
- **接收统计**：主控每秒推送 SESSION `ReceiverStats`，被控据此在解码跟不上时压低帧率上限（规则见 v2-design §6.3.1）。

### 6.6 长期参考恢复（`video.recovery`）

设计与字节示例见 [rfi-ltr-brief.md](rfi-ltr-brief.md) §6。

- 双方都声明 `video.recovery`、并且协商了 `frame-meta` 时启用；否则按 §6.5 用关键帧恢复。浏览器不声明，改用 Dependency Descriptor（见本节末）。
- 被控（组长）把关键帧标为长期参考槽 0；收到观看者的确认后周期地把一帧标进另一个槽（保留所有近期确认过的观看者都持有的最新参考）。间隔为 `max(2×RTT, 1.5×标记到全员确认的平滑时间)`，限制在 100 ms–1 s。帧的 `frame-meta` 按编码器的实际输出填写。
- 主控解码出带“标为长期参考”的帧后推送 `VideoFeedback{ssrc, epoch, decoded, ltr}`（确认）。
- 缺包超过 `max(1.5×RTT, 40 ms)`（或 §6.5 判定不可恢复）时，若主控持有本纪元的长期参考且解码器正常：推送 `VideoFeedback{…, recover: true, request}`，继续 NACK，`max(2×RTT, 150 ms) + 34 ms + 本地播放缓冲` 后仍没有可解码的帧时，以新的请求号再请求一次恢复帧（恢复帧本身可能丢了；关键帧会清空所有参考，而且更大、更容易再丢），同样时间后仍没有才发 PLI；之后到下一个关键帧之前只用关键帧。不持有时按 §6.5 发 PLI。被控按请求号对每位观看者去重。
- 被控收到请求后，下一帧只参考请求中列出的、自己仍保存的最新长期参考（不超过 5 s），并置“恢复帧 + 参考长期参考”；没有可用的就出关键帧。同一请求号只服务一次；恢复帧之后 1 s 内又收到关键帧请求计为一次失败，连续 3 次就对该编码器只出关键帧。
- 主控只在恢复帧参考的长期参考已解码、纪元相同、解码器不在“需要关键帧”状态时，才把它当作新链的起点放行；更早的搁置帧随之丢弃。
- 恢复帧与关键帧一样使用 1e-4 的 FEC 目标，RTX 不受速率限制。
- 共享编码组内，每位成员把自己观看者的确认与恢复请求交给组长；组长用所有请求者都持有的参考出恢复帧，整组收到同一帧。没有丢包的成员把恢复帧当作普通 P 帧解码。从不确认参考的观看者（浏览器）10 秒内 3 次以上请求关键帧时，被控让它单独编码 60 秒。被控拒绝重传时，按该观看者最近的确认主动发恢复帧。
- **浏览器观看者**（2026-10-18）：浏览器不声明 `video.recovery`，也看不到 `frame-meta`，改用 Dependency Descriptor（`https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension`）。只有验证过的浏览器请求它：Chromium 内核（Chrome、Edge，以 `navigator.userAgentData` 的品牌识别），经 `setHeaderExtensionsToNegotiate`；其他浏览器不请求，保持原来的行为。协商了它的视频轨：
  - 被控在每个包上写描述符：一个解码目标、一层；模板 0 为关键帧（首包带结构），模板 1 依赖上一帧；恢复帧在模板 1 上用自定义 fdiff 只依赖它的长期参考。描述符帧号按该轨实际发送的帧连续编号，与 `frame-meta` 纪元无关。
  - 浏览器每 100 ms 推送 `VideoFeedback{ssrc, rtp_timestamp}`（取 `getSynchronizationSources()` 中最近交付帧的 RTP 时间戳）；被控按该轨发送过的帧找到这一帧，沿依赖链（上一帧或恢复帧的参考，到关键帧为止）收集长期参考，当作这位观看者的确认。
  - 被控拒绝重传时，先发只含原始序号的 RTX 包（libwebrtc 当作该序号的填充，不再等待；它对 H.264/H.265 的组帧在缺包时会挡住之后的所有帧），再按这位观看者的确认主动发恢复帧。
  - H.265：Chrome 不解码恢复帧（实测，原因在 Chrome 内部），所以不使用确认；与没有协商描述符的浏览器一样，被控拒绝重传时立即出关键帧（每 500 ms 至多一次），不再等浏览器约 3 s 后自己请求。
- 支持长期参考的编码器：NVENC H.264 / HEVC / AV1（NVENC 需要 NVIDIA 驱动 570 或更新，即 API 13）、软件 H.264；AMF 与 QSV 已实现但默认关闭（`FARWING_AMF_LTR=1`、`FARWING_QSV_LTR=1`）。其他情况收到请求时出关键帧。

### 6.7 经服务器转发（多人）

设计见 [deployment-design.md](deployment-design.md) §5.4，服务器接口见 §3.14。

- 被控（服务器声明 `features.forwarding`、经服务器的主控——原生或网页——达到 `threshold` 人时）按主人选择的模式（被控设置“多人转发”，默认端到端加密；服务器不允许兼容模式时一律端到端加密）建一条到转发器的上行连接：只发送，[`TRACKS`](../../src/features/host/peer/uplink.rs) 条视频轨（`video_0`…，每个编码器一条），扩展与负载类型来自共享表。每条上行轨以只收帧、从不负责编码的成员身份加入一个编码组，经与参与者相同的打包、节奏控制、重传与 RS-FEC 发送；它的码率票据只限码率，不限帧率。
- 经转发的参与者（在房间的会话里）不再从直连发送视频，但每条轨要先经直连发出过至少一帧（主控在第一个直连包到达时建立这条轨，转发的同名轨才有处可去），后加入的参与者因此先看到直连画面、随后转到转发器；它们的编码组照常编码（组长也一样），且共用同一屏幕、同一格式的任一编码组（不再按各自的画质、分辨率与显卡分组）。
- 被控在 SESSION 上推送 `Forward`（tag 24）：`{generation, room, mode, tracks: [{index, track}], session, key, key_id, audio}`——主控的视频轨序号（`video_<index>`）来自房间的哪条轨；`audio`（tag 8）是房间的声音轨（`audio_0`），主控没有声音权限时为空；`mode` 为 `e2ee` 或 `compatible`；`key`（tag 6，20 字节）与 `key_id`（tag 7，1–255）是端到端加密时的帧密钥（§6.8），兼容模式为空与 0；`room` 为空表示回到直连。房间与轨道不变、只有帧密钥变化时，主控沿用已有的转发连接；同一房间换了轨道时（换屏、换到低码率一路或回到主流），主控用新编号的连接订阅，旧连接继续收，直到新连接的画面在其关键帧处接替，再退订旧连接（画面不断）。被控在控制通道换了路由时重发最近一次 `Forward`，主控忽略 `generation` 与已处理的相同的指令。主控订阅（§3.14，`tracks` 按 `Forward` 的顺序，有声音轨时排在最后；提议最多等 300 ms 收集候选，网页端 500 ms、只走中转时 3 秒），在第二条只收媒体的连接上收到这些轨，视频帧进入同一视频轨的接收与解码，声音在转发期间替代直连的声音（离开房间时换回）；此期间对该轨的关键帧请求也发往转发器。连接 10 秒内没有收到媒体即失败。
- 主控在控制通道推送 `ForwardStatus`（tag 25）：`{generation, receiving, error}`。被控只认最近一次 `Forward` 的 `generation`，较早的回答忽略；`receiving: false` 时被控对它恢复直连 5 分钟。主控在等待某个房间的媒体时收到 `generation` 更新的 `Forward`，即放弃这个房间。
- 切换不断画（先接后断）：被控把参与者放进房间后，上行即带上它的画面，但直连的画面与声音照常发送，直到该参与者回答 `receiving: true` 才停。主控的每条视频轨同时只把一条连接的帧送去解码：进房间时转发连接的帧在它的第一个关键帧处接替直连的帧（之前丢弃且不请求关键帧），各轨都接替后（最多再等 1 秒）才回答 `receiving: true`；连接建立 600 ms 后仍没有关键帧就再向转发器请求。10 秒内连接已通、有媒体但没等到关键帧时也回答 `receiving: true`（画面在下一个关键帧切换）。回到直连或换房间时，主控保留原转发连接，直到直连的帧在其关键帧处接回（最多 3 秒）才离开；被控关闭房间后上行再转发 3 秒。网页主控同样：转发的画面解出第一帧后才换上并回答，回到直连时等直连的画面重新解码后才换回。
- 网页主控只接收 `index` 为 0 的轨（网页只显示一路画面），订阅的 offer 去掉 AV1 的 `level-idx`（同直连）。端到端加密时用编码帧变换（`RTCRtpScriptTransform`，在 Worker 中解密，[`server/web/src/lib/forwardWorker.ts`](../../server/web/src/lib/forwardWorker.ts)），它必须在生成 offer 之前挂到接收端上（Chrome 不会把之后设置的变换接入 answer 中声明了 SSRC 的接收流）；浏览器不支持编码帧变换时回答 `receiving: false`。转发器为浏览器的下行改写负载类型、扩展 ID 与 MID 扩展的值（浏览器按 MID 分流）。
- 转发器在上行连接上向被控发 REMB（`ssrcs` 为 `[0]`，被控在 SSRC 0 上读取，与传输层反馈一起）：转发流的码率上限，即至少一半观看者能承受的码率（deployment-design §5.4）；没有上限时不发，被控的上限 1 秒后失效。
- 被控的上行连接带一条 Opus 声音轨（`audio_0`），用主人设置的声音设备与音质编码一次；在房间里的参与者不再从直连收声音。键鼠、剪贴板、文件、麦克风与其余消息仍走直连。人数低于阈值、转发器失败（30 秒后再试）或服务器关闭多人转发时，被控关闭房间并推送 `room` 为空的 `Forward`。
- 主控的参与者面板在经转发时显示“服务器转发 · 端到端加密”或“服务器转发 · 兼容模式”。

### 6.8 帧加密（端到端加密模式）

被控在编码后、分包前加密每一帧，转发器只转发密文，主控在组帧后、解码前解密（[`src/media/frame_crypto.rs`](../../src/media/frame_crypto.rs)）。

- 帧密钥 20 字节：前 16 字节为 AES-128-GCM 密钥，后 4 字节为 nonce 盐。每个房间一个，被控随机生成，只经被控与各参与者之间已认证的会话（`Forward.key`）下发。
- 加密单元：H.264 的 VCL NAL（类型 1–5）与 H.265 的 VCL NAL（类型 < 32），长度超过“NAL 头 + 8 字节”的；AV1 的帧与帧数据 OBU（类型 6、4）；Opus 帧（首字节 TOC 保持明文，作为 `clear`，接收端从它读取帧时长）。视频与声音共用帧密钥与 counter。参数集、SEI、AV1 序列头等保持明文（分包与接收端要读）；NAL 头之后的前 8 字节也保持明文（接收端在此读取 PPS ID 等）。
- 格式：`clear ‖ AES-128-GCM(rest) ‖ tag(16) ‖ counter(8) ‖ key_id(1) ‖ 0xFE`。`clear` 为 NAL 头 + 8 字节（H.264 / H.265）或 OBU 头（AV1），同时作为附加认证数据；nonce = 盐(4) ‖ counter(8，大端)。counter 由被控对所有轨共用、逐单元递增（起点随机）。H.264 / H.265 加密后的部分再做防竞争转义（`00 00 0x`，x ≤ 3 时插入 `03`），不产生起始码；AV1 改写 OBU 的长度字段。
- 解密失败（未知 `key_id`、认证失败、格式不符）的帧丢弃，主控向转发器请求关键帧。明文的参数集不受认证保护：转发器能让画面无法解码，但不能读出或伪造画面内容。
- 换钥：有参与者离开房间时，被控生成新密钥（`key_id` 加 1，跳过 0）并推送给留下的参与者，2 秒后上行改用新密钥；主控保留最新的两个密钥，因此切换前后的帧都能解密。新参与者加入时不换钥。

---

## 7. NAT 穿透

### 7.1 NAT 行为探测

- 服务端以 `[[turn.stun]]` 配置额外的 STUN 端点（内置 TURN 模式），应答 RFC 5780 行为探测（`OTHER-ADDRESS`、`CHANGE-REQUEST`），并经 `GET /server` 的 `features.nat_discovery` 告知客户端。
- 客户端用一个绑定默认路由 IPv4 地址的 UDP 套接字：先向主端点绑定，再做过滤测试（改 IP 与端口 / 只改端口），最后向其他端点做映射测试。映射与过滤各分 `eim`、`adm`、`apdm`，无法判定为 `unknown`（两类无法区分时取较严格的一类）。
- `port_delta`：映射与目的地相关时，取新映射端口的步长；至少两个步长一致、外部 IP 相同且 |步长| ≤ 64 时给出，否则为 0。
- 结果按网络指纹（默认路由的本地地址 + 网关）缓存 10 分钟（失败 1 分钟），网络变化（2 秒去抖）与到期时重新探测；会话最多等 300 ms，否则不带结果继续。

### 7.2 `a=x-farwing-nat`

offer 与 answer 都在会话级加入：

```
a=x-farwing-nat:mapping=<eim|adm|apdm|unknown>;filtering=<eim|adm|apdm|unknown>;delta=<i32>
```

无结果时发送 `unknown/unknown/0`；关闭“激进打洞”时 `delta=0` 并追加 `;punch=off`（对端因此既不预测本端端口，也不向本端做探测打洞）。映射不是 `adm`/`apdm` 时接收方把 `delta` 视为 0；不认识的字段忽略。ICE 重启时随新描述更新。SDP 对服务端不透明。

### 7.3 端口预测

对端映射为 `adm`/`apdm` 且 `delta ≠ 0`，且本端不是“`eim` 映射 + `eim`/`adm` 过滤”时，对端每个 UDP/IPv4 srflx 候选（含远端 SDP 内的）加入之后，再经 `add_ice_candidate` 加入 `port + delta·k`（k = 1..8，端口 ≥ 1024）的预测候选：沿用原候选的 foundation、ufrag 等字段，优先级为 `priority − k·256`。EIM/EIM 永不预测。

### 7.4 网关端口映射

ICE 代理在默认路由上发布私网 UDP/IPv4 主机候选后，为该端口向网关申请映射（每会话最多 2 个），依次尝试 PCP（MAP，端口 5351）、NAT-PMP、UPnP-IGD（`WANIPConnection:2/1` 或 `WANPPPConnection:1` 的 `AddPortMapping`）。外部地址为公网地址时，以 srflx 候选（类型偏好 100、沿用主机候选的本地偏好、`raddr/rport` 为主机地址）经正常候选通道发给对端；与已发布的 srflx 相同或为私网/共享地址时不发布。租约 2 小时，半程续期，会话关闭时删除（最多等 2 秒）。

### 7.5 探测打洞

两端都开启“激进打洞”（对端属性无 `punch=off`）时，ICE 代理在收集开始 5 秒后若仍未选中直连路径，按双方的映射规划探测：

- 映射：`eim` 为锥形；`adm`/`apdm` 且 `delta ≠ 0` 为线性（步长 `delta`）；`adm`/`apdm` 且 `delta = 0` 为随机；`unknown` 时按该端的 srflx 候选推断（同一本地端口在至少两个 STUN 服务器上映射相同为锥形，多个映射端口等差为线性，否则为随机，只有一个 STUN 服务器时不推断）。
- 规划按双方类型选择：向对端最近的映射端口及其线性后继发送、从多个临时本地端口发送，或（对端为随机时）向随机端口发送。每轮 20 秒，之后暂停 5 秒（两端都是随机时 120 秒），最多 6 轮；选中直连后停止；ICE 重启后重新规划。
- 探测报文为 12 字节 UDP：请求 `06 46 57 71`（`\x06FWq`）、应答 `06 46 57 72`（`\x06FWr`），后接 8 字节标记。标记为 `SHA-256("farwing punch probe\n" ‖ A ‖ "\n" ‖ B)` 的第 0–7 字节（请求）与第 8–15 字节（应答），A、B 为双方的 `ufrag:pwd`，按字节序较小者在前，因此两端算出相同的报文；ICE 重启后随凭据变化。首字节不在 RFC 7983 的任何分流范围内。
- 收到本会话的探测请求的端口回复应答；收到请求或应答的临时端口成为本端的一个主机候选，与发出方地址组成候选对，之后按普通 ICE 连通性检查（带消息完整性）建立。标记不符的报文按普通非 STUN 流量处理（丢弃）。

### 7.6 其他

- 中继候选对保持同意检查作为备份。控制方把路径分为三类——直连、UDP 中转、TCP / TLS 中转（按本端到中转服务器一段的协议）——按“代价 = RTT + 25 ms ×（丢包 % − 1）”选择：当前路径的丢包按收到的媒体计，其他两类各取 RTT 最低的一条，按每 250 ms 一次的连通检查计；另一条中转的代价低出 max(30 ms, 25%) 时切过去，直连代价不高于当前中转时切回直连，几条都满足时取代价最低的，判断连续 3 秒成立才切换。因媒体丢包离开的路径 60 秒起（反复时翻倍，最长 10 分钟）内不再尝试；切到直连（或重新尝试这样的 UDP 中转）之前先压测（§7.7），切过去后有 30 秒试用期，不行立即退回（v2-design §11）。
- 只用中转（观看窗口“使用中转”，`network = 1`）时 ICE 只收集中转候选，TLS 中转排在最前，TCP、UDP 其次；之后同样按实测代价在 UDP 与 TCP / TLS 中转之间选择。
- 主机候选优先级中 IPv6 网络偏好高于 IPv4，IPv6 主机候选优先于 IPv4 主机与 srflx 候选。
- 设置：每个 profile 的 `network.json` 中 `aggressive_punching` 与 `port_mapping`（默认开启），主控与被控在会话开始时读取。

### 7.7 路径压测

控制方在把会话移到另一条候选对之前，可以请对端沿那条候选对发一段填充数据，按实收判断它在负载下的丢包。两种 UDP 报文都直接在该候选对上收发，首字节 `0x06` 不在 RFC 7983 的分流范围内；`tag` 为 §7.5 探测报文的 8 字节标记。

- 请求（控制方 → 对端，26 字节，同一请求连发 3 次）：`06 46 57 4C`（`\x06FWL`）、`tag`、`id`（u32）、`rate_kbps`（u32）、`duration_ms`（u32）、`size`（u16），整数均为大端。
- 数据（对端 → 控制方，每个 `size` 字节）：`06 46 57 64`（`\x06FWd`）、`tag`、`id`（u32）、`seq`（u32，从 0 起），其余填充。共 `rate_kbps × 1000 / 8 × duration_ms / 1000 / size` 个，在 `duration_ms` 内均匀发出。
- 对端只在 `tag` 正确、请求来自 30 秒内与本端互通过连通检查的候选对、`rate_kbps ≤ 20000`、`duration_ms ≤ 5000`、`20 ≤ size ≤ 1200`、且没有正在进行的压测时发送；重复的 `id` 忽略。数据只发往请求的来源地址。
- Farwing 主控的取值：码率为当前视频码率的一半（1000–8000 kbps）、4000 ms、1100 字节；发出请求 5.5 秒后按收到的个数计算丢包。

---

## 8. 直连与局域网

不经服务端也能建立会话：被控内置直连端点，主控经局域网发现或输入地址连接；授权与经服务端时是同一张访问名单，由被控判定：本账号设备（缓存）、好友（缓存的好友授权）、受信任设备、访客（访问码或本机确认）。会话建立后的媒体与数据通道与经服务端的会话相同。线上格式在 `crates/protocol/src/direct.rs`，实现在 `src/transport/direct/`；本地记录（`trusted_devices.json`、`known_hosts.json`、`own_devices.json`、`friend_devices.json`）见 v2-design §12.6。

### 8.1 直连端点

- 被控在“允许直连”（`network.json` `direct`，默认开启）时监听 TCP `direct_port`（默认 28890，被占用时依次尝试 +1..+9）的所有接口；只在“允许连接本机”时接受会话。
- 传输为 TLS 1.3，ALPN `farwing-direct/2`。被控出示进程启动时生成的自签证书，主控不校验证书链；身份由握手证明绑定 TLS 导出密钥 `ekm`（§2.2）。
- 帧：`u32` 大端长度 + 实时面 `Envelope` JSON（`v: 2`），单帧 ≤ 1 MiB。会话建立后双方每 15 秒发 `ping`，45 秒无数据断开。
- 未完成握手的连接最多 16 个；握手每一步 10 秒（等待被控用户确认时最长 65 秒）。错误以实时面 `error` 信封返回（`ErrorBody`，码见 §3.3），随后断开。

### 8.2 握手消息

| type | 方向 | data |
|------|------|------|
| `direct.hello` | 双向，各自的第一条消息 | `{device_id, public_key, name, platform, version, proof}`；被控另带 `methods`（此刻接受的方式，见下）与 `udp_port` |
| `direct.auth` | C→H | `{method: "own"\|"friend"\|"trusted"}`（被控不论取值，依次检查全部） |
| `direct.pake` | 双向 | 主控：`{kind: "assist", messages}`；被控：`{messages, confirm: [每槽一条], confirmation}` |
| `direct.confirm` | C→H | `{confirm}` |
| `direct.authorized` | H→C | `{relation: "own"\|"friend"\|"trusted"\|"assist", grants}` |

`methods`（`direct::method`）：`own`（被控登录了账号）、`friend`（登录了账号且好友授权缓存非空）、`trusted`（总是声明）、`assist`（访问码已开启）。

### 8.3 授权

1. 双方交换 `direct.hello`，校验对方 `device_id` 由其 `public_key` 推导（§1.3，否则断开）以及 `proof`。被控未开启“允许连接本机”时回 `error{host_disabled}`。
2. 主控选择一种方式：
   - **设备密钥**（`direct.auth`；被控声明 `own` 时主控发 `own`，否则 `trusted`）。被控依次检查：
     - `own` —— 被控缓存的本账号设备列表（`GET /devices` 中的 `public_key`，保存在 `own_devices.json`）包含主控公钥：关系 `own`，全部能力。
     - `friend` —— 被控缓存的好友授权（`GET /devices/self/friend-access`，§3.12，保存在 `friend_devices.json`，7 天内有效）包含主控公钥：关系 `friend`，能力为该好友在被控上的授权（总含 `view`）。
     - 主控不在本账号设备缓存中时，已登录且连着服务端的被控先刷新这两份缓存（最多每 10 秒一次）再判定，因此服务端可达时好友授权的撤销立即生效；不可达时按缓存。
     - `trusted` —— 被控受信任设备（`trusted_devices.json`）中有该公钥：关系 `trusted`，能力为为该设备记录的能力（总含 `view`）。
     - 都不是时 `error{forbidden}`。
   - **访问码**（`direct.pake{kind: "assist"}`）：身份串 `farwing-assist-v2`，两个槽位同 §2.3；被控回 `{messages, confirm: [HMAC(K_i, "FWPAIR2\nhost\n"+hex(ekm))], confirmation}`，主控核对后发 `direct.confirm{HMAC(K, "FWPAIR2\ncontroller\n"+hex(ekm))}`。被控为仅确认模式时 `messages` 为空、`confirmation: true`，需要时由被控界面确认（被控用户可选“允许并信任”，把该设备加入受信任设备）。关系 `assist`，能力为访客默认权限。错误码：`assist_unavailable`（未开启访问码）、`assist_code_required`、`assist_code_invalid`、`assist_rejected`。
3. 被控回 `direct.authorized`；主控把被控记入 `known_hosts.json`，关系不是 `assist` 时记为“对方信任本机”，以后按设备密钥连接。
4. 失败计数与经服务端的访问码共用（每分钟 10 次失败后 5 分钟内拒绝，`error{rate_limited}`）；发出 PAKE 消息后 10 秒内未给出正确确认值计为失败。

### 8.4 直连会话

- 主控发送 `session.start{attempt_id, options, capability, purpose}`（带 `id`）。被控生成 `session_id` 与 `participant_id`，以 `ack: SessionStarted`（`ice_servers` 为空、`host` 为被控身份、`relation`、`grants`）回复，并把它作为一条 `session.incoming`（`connect_type`：访客为 `assist`，`own`、`friend`、`trusted` 为 `account`）交给与服务端会话共用的被控会话管理；被控照常按目的检查所需能力。
- `rtc.offer`/`rtc.answer`/`rtc.candidate` 与经服务端时相同；SDP 照常带 `a=x-farwing-identity`，使用访问码的会话另带 `a=x-farwing-pake`（K 为本次 PAKE 的密钥）。
- 被控结束参与者时向主控发 `session.ended{reason}`；主控断开或发 `session.end` 时被控以 `controller_left` 结束该参与者；被控停止直连时主控收到 `session.ended{host_closed}` 或连接断开。

### 8.5 直连 ICE

- 不使用 TURN 与 STUN。被控的所有直连会话共用 UDP `port`（与端点 TCP 端口同号，每个本机地址一个套接字）收集主机候选，不收集 ICE-TCP 候选；主控照常收集。共用端口上的数据报按会话分发：STUN 响应按事务 ID，对端的首个 Binding 请求按 USERNAME 中本端的 ufrag，其余按已交换过的对端地址；不属于任何会话的数据报丢弃。
- 主控对被控的每个 UDP 主机候选另加一个 srflx 候选：实际连上的被控地址 + 该候选的端口（优先级低于主机候选；地址相同或为回环地址时不加），用于经路由器端口转发的公网直连。
- §7 的 NAT 属性、端口预测与网关映射照常（端点的 TCP 端口不自动映射）。

### 8.6 局域网发现

- 被控在 `lan_discovery`（默认开启）时绑定 UDP `0.0.0.0:28889`（`SO_REUSEADDR`）并加入组播 `239.255.70.87`；“允许连接本机”时每 30 秒向组播地址发送通告，并单播回应查询。
- 主控向组播地址与 `255.255.255.255:28889` 各发一次查询，等待时间过半时再发一次，默认收集 2 秒。
- 查询：`{"t":"farwing.lan.query","v":2,"nonce":"…"}`。
- 通告：`{"t":"farwing.lan.announce","v":2,"device_id","name","platform","version","public_key","port","udp_port","methods":[…],"ts","nonce","sig"}`。回应查询时 `nonce` 与查询相同，主控只接受本次查询的应答；周期通告的 `nonce` 为空。`device_id` 不是由 `public_key` 推导的通告被丢弃。`sig` 只证明 ID、名称、端口与公钥属于同一设备；能否按设备密钥连接由主控按设备 ID 匹配本账号设备、好友共享的设备或 `known_hosts.json` 决定。

---

## 9. 安全要点

- 服务端通信使用 TLS 1.2+（rustls）；自签名部署时客户端固定服务器证书 SHA-256 指纹或导入自定义 CA。
- 设备认证为每请求签名，时间窗口 ±300 秒，600 秒内拒绝重放；账号令牌绑定设备，单独窃取令牌无法使用。服务端只保存令牌的 SHA-256 与设备公钥；密码为 Argon2id（m=19 MiB, t=2, p=1）。
- 会话的目的、类型、关系、能力与主控身份由服务端（直连时由被控）给出；对端自报的同名信息不作授权依据。被控本机开关始终是能力上限。
- 设备 ID 由公钥推导，冒用 ID 在协议层不可能；DTLS 指纹由设备密钥签名，对端公钥须能推导出对端设备 ID；使用访问码的会话另以 PAKE 密钥确认，服务端看不到访问码。
- 谁可以连接由被控判定：受信任设备记录与访问码只在被控本机，服务端只知道 `policy.assist`（是否接受访客或受信任设备经服务端连接）。
- TURN 凭据按会话签发，默认 12 小时过期，共享密钥只在服务端。
- 服务端对登录、设备登记、`assist/join`（按账号，无账号时按设备）、好友申请与实时连接限速；所有中转消息校验会话归属与方向。
