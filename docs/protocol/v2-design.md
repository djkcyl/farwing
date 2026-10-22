# Farwing Protocol v2 设计约定

状态：已实现（对应 `docs/improvement-proposals.md` §7–§8 的决策与阶段）。规范以 [`farwing-protocol.md`](farwing-protocol.md) 与 [`peer-messages.md`](peer-messages.md) 为准；本文件保留为 v2 的设计与决策记录，并附各节的实现状态说明，与规范不一致之处以规范（及代码）为准。v2 不兼容 v1，客户端与服务端同时升级；产品版本 0.1.0。

身份、授权与连接的统一模型（设备 ID 由公钥推导、被控端的一张访问名单、访问码、受信任设备）概览见 [`docs/access-model.md`](../access-model.md)；本文 §1、§2、§3、§12 按该模型描述。

## 0. 总则

- **版本**：HTTPS 头 `X-Farwing-Protocol: 2`；WSS 信封 `v: 2`；对端 HELLO `protocol_version = 2`。任何一方版本不符直接拒绝（HTTP 426 / 关闭码 4009 / HELLO 失败）。
- **平台**：统一为字符串枚举 `windows | macos | linux | android | ios`，替代账号平台号 1–5 与 `client_type` 1–4。
- **能力**：会话前只看服务端设备信息里的 `capabilities`（能做什么）与 `policy`（主人允许什么）；会话内只看双方 HELLO 声明的特性（名称 + 版本），且只声明运行时真实具备的。
- **权威字段**：会话的目的、类型、关系、主控身份、授予的能力由服务端在 `session.incoming` 给出（受信任设备与访客的能力由被控按本机记录决定，直连时全部由被控给出）；对端自报的同名信息一律不作授权依据。

## 1. 设备身份与请求认证（P2）

### 1.1 设备密钥

- 每个安装（每个 profile）生成 Ed25519 密钥对。私钥与设备身份一起保存在系统凭据存储；安装服务后保存在 DPAPI 保险库。
- **设备 ID 由公钥推导：** 公钥字节 SHA-256 的小写 base32（字母表 `a-z2-7`）前 16 个字符（80 位，`crypto::local_device_id`）。服务端不再分配 ID，有无服务器、换哪台服务器，同一台设备的 ID 都相同。
- **设备 ID 就是指纹：** 界面只显示设备 ID（四位一组，如 `abcd efgh ijkl mnop`），不再另列“指纹”。服务器证书指纹是另一回事，保留。
- `POST /api/v2/devices/init` 请求体携带 `public_key`（base64url，32 字节）、`timestamp` 与持有证明 `proof = sign(sk, "FWINIT2\n" + public_key + "\n" + timestamp)`。新公钥以推导 ID 登记；同一公钥再次登记即更新同一设备。登记不需要账号。
- 同一安装重新登记时沿用原密钥；密钥丢失即视为新设备。
- **设备令牌（device_token）取消。** 设备认证全部由请求签名完成。
- **服务端数据库：** 没有迁移。服务端只打开当前结构版本（4）的数据库，其他版本拒绝启动（提示把文件移走以得到空数据库）；新数据库直接按当前结构创建。设备 ID 只接受由公钥推导的格式。
- **客户端记录：** 本地以设备 ID 为键的记录（受信任设备、连接过的设备、本账号设备与好友授权缓存）中，设备 ID 不是由其公钥推导的条目读取时丢弃；保存 ID 与公钥不符的受信任设备报错。
- **短号**（`connect_id`，9 位数字）是服务端分配的拨号别名，只用于口头告诉别人、经服务端寻址，不是身份。被控在允许连接且有服务端时获取（`POST /api/v2/assist/identity`）。

### 1.2 请求签名

每个需要设备认证的 HTTPS 请求带三个头：

```
X-Farwing-Device-Id: <device_id>
X-Farwing-Timestamp: <unix 毫秒>
X-Farwing-Signature: base64url(ed25519(sk, "FWREQ2\n" + METHOD + "\n" + path_and_query + "\n" + timestamp + "\n" + hex(sha256(body))))
```

- 服务端用登记的公钥验签，允许 ±300 s 时钟偏差。
- 600 s 内拒绝重复签名（重放缓存，仅内存）。客户端保证同一进程内签名时间戳严格递增；被拒的请求未执行，客户端校准时钟后重签一次。
- 失败返回 `401 device_unauthorized`。

### 1.3 账号令牌

- `access_token` 仍为 bearer，但绑定设备：服务端要求同一请求的设备签名必须属于令牌绑定的设备。单独窃取令牌无法使用。
- 有效期 30 天滑动续期，可撤销。
- 每台设备只保留一个登录：新登录撤销该设备此前的令牌。
- 新增 `GET /api/v2/auth/sessions`（列出本账号各设备的登录）与 `DELETE /api/v2/auth/sessions/{device_id}`（撤销，效果同远程退出）。

### 1.4 DTLS 指纹绑定与身份校验

- **签名：** offer 与 answer 的会话级 SDP 各加一行：

  ```
  a=x-farwing-identity:<base64url(ed25519(sk, "FWDTLS2\n" + session_id + "\n" + attempt_id + "\n" + role + "\n" + 小写、排序、去重后的 a=fingerprint 值逗号连接))>
  ```

  `role` 取 `controller` 或 `host`。
- **公钥来源：** 对端公钥由服务端下发：`session.incoming.controller.public_key`、`session.started.host.public_key`。
- **ID 与公钥一致：** 双方先检查服务端下发的对端公钥能推导出对端的设备 ID（`src/transport/signal/identity.rs` `check_key`），不符即拒绝（“对端出示的密钥与设备 ID 不符”）。设备 ID 由公钥推导，冒用 ID 在协议层不可能，因此不需要首次信任记录。
- **校验失败**一律终止会话：被控发送 `session.end{reason: "identity_mismatch"}`；主控收到未通过校验的 answer 直接结束本次连接。
- **使用访问码的会话**另外用 PAKE 密钥确认指纹（见 §2），不依赖服务端下发的公钥。

## 2. 经服务端连接：访客与受信任设备（P2）

被控自己判定谁可以连接本机（`docs/access-model.md` §2.2）。经服务端时，本账号设备与好友走 `POST /api/v2/sessions`（§3.4）；其他设备——受信任设备与访客——走下面的流程：受信任设备按设备密钥放行；访客证明知道访问码（SPAKE2，服务端只中转，看不到访问码），或由被控用户确认。

- **访问码：** 类型为临时访问码（随机生成，可刷新）、固定访问码（自己设定）或两者都接受；验证方式为访问码、访问码 + 本机确认、仅本机确认；访客默认权限为能力模板（§3.2）。
- **主控：** 有无账号均可：`assist/join`、`…/accept`、`…/cancel` 与 `POST /api/v2/sessions/{id}/ticket` 接受账号令牌或游客令牌（服务端开启游客时）。
- **寻址：** 按短号（`connect_id`）或设备 ID（`device_id`），二者恰好给一个。
- **算法：** SPAKE2（`spake2` crate，Ed25519 群，对称模式），身份串 `"farwing-assist-v2"`，口令为访问码的 UTF-8 字节。槽 0 对应临时访问码、槽 1 对应固定访问码；被控两种都接受时并行运行两个实例。
- **流程**（全部经服务端中转）：
  1. 主控 `POST /api/v2/assist/join {connect_id | device_id, pake: [msg_0, msg_1]}`：两个 SPAKE2 实例都由用户输入的访问码生成（主控无法区分对方用哪一种）；未输入访问码时 `pake` 为空。
  2. 服务端推送被控 `assist.join {request_id, controller{device_id, alias, user, platform, public_key}, pake}`；无账号的主控 `user` 为空。
  3. 被控 `POST /api/v2/assist/requests/{id}/pake {allowed, mode, trusted, pake: [msgB_i], key_confirm: [HMAC(K_i, "FWAK2\nhost\n"+request_id)]}`：
     - 请求未带 `pake`、主控公钥在本机受信任设备中且本机允许连接：`{allowed: true, trusted: true}`，不跑 PAKE，也不要求确认。
     - 否则按访问码应答：未配置的槽用随机口令应答，不暴露配置；仅确认模式时 `pake` 为空。未开启访问码或暂时锁定时 `allowed: false`。
  4. 服务端把应答作为 `AssistPending{request_id, mode, trusted, pake, key_confirm, host}` 返回主控。非 `trusted` 时，需要访问码而请求未带 `pake` 返回 `409 assist_code_required`；`allowed: false` 返回 `409 assist_unavailable`。
  5. 主控：`trusted` 时直接 `accept`（确认值为空）。否则逐个计算 `K_i` 并比对 `key_confirm`；都不匹配就是访问码错误：本地报错并撤回请求（`…/cancel`），访问码本身从未发出。匹配则 `POST /api/v2/assist/requests/{id}/accept {key_confirm: HMAC(K, "FWAK2\ncontroller\n"+request_id)}`（长轮询）。
  6. 服务端推送被控 `assist.accept`。受信任的请求：被控再次核对仍允许连接、该设备仍受信任，然后 `POST …/decision {allowed: true, trusted: true}`。访客：校验确认值，需要时弹出本机确认，然后 `POST …/decision {allowed, refusal}`（`code_invalid | code_required | rejected | unavailable`）。允许后服务端建立会话（`purpose = desktop`、`connect_type = assist`、`relation = trusted | assist`、`grants` 为空），把 `AssistJoin{session ticket, host}` 返回主控。
  7. 使用访问码的会话，双方 SDP 增加 `a=x-farwing-pake:<base64url(HMAC(K, "FWDTLS2\n"+session_id+"\n\n"+role+"\n"+fingerprints))>`，互相校验；被控按会话保存 K，重连同一会话时继续使用。
- **能力：** 由被控在 `session.incoming` 时决定：`trusted` 重新核对本机受信任设备记录（设备 ID 与公钥都须匹配，否则拒绝），套用为该设备记录的能力；`assist` 套用访客默认权限。
- **允许并信任：** 被控用户回答本机确认时可选“允许并信任”（`assist::Action::Answer{trust: true}`）：本次仍是访客会话，同时把该设备加入受信任设备（能力取访客默认权限，§12.6），下次按第 3 步的受信任路径连接。
- **预检：** 主控未输入且未保存访问码时，先用空 `pake` 发起请求：信任本机或仅确认模式的被控直接返回待处理的请求，主控继续第 5 步；需要访问码的被控返回 `assist_code_required`，界面再要求输入。
- **被控策略：** `DevicePolicy.assist` 表示“接受访客或受信任设备经服务端连接”，即访问码已开启或本机有受信任设备；为假时服务端直接返回 `assist_unavailable`。游客被控只接受 `connect_type = assist` 的会话（访客与受信任设备）。
- **失败计数：**
  - 被控本地：每分钟最多 10 次失败，超过后 5 分钟内拒绝访问码（经服务端与直连共用），并在界面提示。失败包括确认值错误，以及带访问码却在 120 s 内未完成确认、或被撤回的请求（主控只凭 PAKE 应答就能判断猜测是否正确，必须在被控侧计数）。
  - 服务端：每个主控（账号；无账号时为设备 `device:<id>`）对每个目标每分钟 `assist_failures_per_minute` 次，每个目标跨主控每小时 50 次；最终建立会话才返还。
- **最近列表：** 只为有账号的主控、且被控有短号时记录。
- **仅确认模式与受信任设备：** 不跑 PAKE，指纹只靠 §1.4 的设备签名（服务端下发的公钥须能推导出对端设备 ID）。

## 3. 好友与授权（P2）

### 3.1 数据模型（服务端）

```
friendships(user_a, user_b, created_at)            -- user_a < user_b
friend_requests(id, from_user, to_user, created_at)
grants(owner_user, grantee_user, device_id NULL, caps TEXT, updated_at)
       -- device_id 为 NULL 表示“所有设备”默认值；具体设备的行覆盖默认值
```

### 3.2 能力名

| 能力 | 含义 |
| --- | --- |
| `view` | 观看画面 |
| `control` | 键鼠控制 |
| `clipboard` | 剪贴板同步 |
| `files` | 文件传输与远程文件管理 |
| `ports` | 端口转发 |
| `power` | 关机 / 重启 / 网络唤醒 |
| `audio` | 接收桌面声音 |
| `microphone` | 向被控发送麦克风 |
| `display` | 修改分辨率 / DPI / 虚拟屏 / 独占屏 |
| `diagnostics` | 获取诊断包 |
| `annotation` | 批注 |

- 自己账号的设备：全部能力。
- 好友：按 grants；建立好友关系时，双方各自自动为对方生成“所有设备 = view, annotation”。某台设备的有效能力为空（单独设为空，或默认为空且未单独设置）时，该设备对好友完全不可见：不出现在 `GET /devices` 的 `shared` 中，详情、壁纸、会话等接口按 `device_not_found` 处理，也不推送它的状态。界面中“默认都不共享”即默认能力为空。
- 主控界面中好友共享的设备按好友分组列出（“好友”页与“设备 → 好友共享”），不出现在“我的设备”。所有设备列表只显示设备 ID（§1.1，即设备指纹），同一台设备在各种连接方式与任何服务器下都相同。
- 好友在直连时同样按这里的授权：被控缓存 `GET /api/v2/devices/self/friend-access` 的结果，按 `friend` 方法授权（§12.2）。
- 访客：被控本机的访客默认权限（能力模板）。默认 `view, control, clipboard, audio, annotation`。
- 受信任设备：被控为该设备记录的能力（§12.6），加入时取访客默认权限，可修改。

### 3.3 HTTPS 接口

| 接口 | 说明 |
| --- | --- |
| `GET /api/v2/friends` | 好友列表 `{user_id, username, nickname, since}` |
| `POST /api/v2/friends/requests {username}` | 发送申请；对方已向我申请时直接成为好友 |
| `GET /api/v2/friends/requests` | `{incoming:[…], outgoing:[…]}` |
| `POST /api/v2/friends/requests/{id}/accept` · `…/reject` · `DELETE …/{id}` | 接受 / 拒绝 / 撤回 |
| `DELETE /api/v2/friends/{user_id}` | 解除好友：删除双向 grants，结束相关会话 |
| `GET /api/v2/grants/{user_id}` | 我给该好友的授权：`{default: caps, devices: {device_id: caps}}` |
| `PUT /api/v2/grants/{user_id}` | 同上结构，整体替换 |
| `GET /api/v2/devices` | 返回 `own:[…]` 与 `shared:[{owner{user_id,nickname}, granted:[caps], device…}]` |
| `GET /api/v2/devices/self/friend-access` | 可以控制本设备的好友设备：`{devices: [{device_id, public_key, alias, user_id, nickname, grants}]}`，即在本设备上持有非空授权的每位好友的全部设备；被控缓存它，直连时按 `friend` 授权（§12.2） |

### 3.4 生效

- `POST /api/v2/sessions` 校验：自己的设备，或好友设备且具备该目的所需能力（desktop/audio 需要 `view`；files 需要 `files`；ports 需要 `ports`）。
- `session.incoming` 携带 `relation: own|friend|assist|trusted` 与 `grants:[caps]`；`assist` 与 `trusted` 的 `grants` 为空，由被控按本机记录决定（§2）。
- 授权变更时，服务端向相关被控推送 `session.permissions {sid, grants}`；被控取本机上限交集后立即生效，并用 FPP `PERMISSIONS` 帧通知主控。
- 被控本机开关（允许连接本机、文件、端口、远程电源、剪贴板设置）始终是上限，对所有关系（本账号、好友、受信任设备、访客）生效。

## 4. 多人会话（P3）

### 4.1 服务端

- 一台被控可同时有多个会话（参与者），默认上限 8；被控主人可在本机设置中改为 1–16，随设备策略 `policy.max_participants` 上报，服务端与被控都按它拒绝新参与者（已连接的不受影响）。
- 不再有 busy/force 顶替。
  - 同一主控设备对同一被控只保留一个会话，重连时替换自己的旧会话。
  - 超出上限返回 `409 busy`。
- 设备信息 `participants` 列出参与者：`{participant_id, user, device, relation, since}`。

### 4.2 被控

- 每个参与者一个 RTCPeerConnection。
- **共享编码（`src/features/host/peer/encode_group.rs`）：** 同一屏幕、同一参数的参与者共用一路编码；参数不同时另开，每个被控进程最多 4 路编码。
  - **分组键：** 输出设备、编码候选（适配器、后端、编码格式 / 色度 / 位深）、请求的 HDR、编码尺寸上限（主控的缩放限制）、帧率档（弱网策略与解码帧率上限之前）、光标合成、观影模式、画质档（自动画质取本人已定档位；自定义画质另含码率）。
  - **角色：** 每位参与者仍有自己的采集线程、传输、分包、FEC、发送节奏与 RTP 时钟。组内最早加入的成员（组长）采集并编码，其余成员（组员）接收已编码帧，按采集时刻换算到自己的 RTP 时间戳后发送；采集、编码器与 GPU 资源只有组长持有。组长离开时由最早加入的组员接替（重新打开采集与编码器，以关键帧开始）。
  - **码率与帧率：** 每位成员每次迭代上报自己的可用码率（`min(传输可用码率, 档位上限)`）、拥塞窗口收缩比与帧率（弱网策略与解码帧率上限之后）；组长按在线成员的最小码率、最大收缩比与最小帧率编码。可用码率低于编码下限（传输暂停）的成员不拉低整组，暂不接收帧，恢复后从关键帧开始。
  - **关键帧：** 新成员加入、任一成员的 PLI/FIR（各自的 RTT 门限之后）、组员丢帧或重新协商，都会请求整组的下一帧为关键帧。组长的观看者声明了 `video.recovery` 时，它自己的丢包改由恢复帧解决（farwing-protocol.md §6.6，2026-10-18）；组员的恢复请求仍转成整组关键帧。组员的接收队列为 3 帧，跟不上时漏掉的帧由序号发现，之后丢弃非关键帧直到关键帧到达（1 s 内未到则再次请求）。
  - **自动画质：** 每位成员按自己的带宽估计定档；定档结果不同即离开原组，加入或新开相应的组。
  - **编码路数上限：** 第 5 种参数出现时，加入同一屏幕、同一编码格式的最接近的组：优先同一编码器，其次不大于自己档位的最大画面，再次尺寸最接近；画面按该组的参数发送。没有可共用的组（其他屏幕或编码格式）时仍新开一路并记录警告。
  - **诊断：** 组员的编码器信息、`SenderInfo`、画质推送（`Quality`）与发布状态取自组长上报的当前输出；光标形状由组长采集后共享。
  - 硬件编码会话不足时，该组的组长回落到下一个编码候选（其分组键随之变化）。
  - 新成员在收到自己的首个 CaptureSetting 之前（最多 2 s），起始参数只作暂代：有同一屏幕、同一编码格式的组时先跟随最接近的组，不为马上要变的参数新开编码器；设置到达后按自己的分组键加入或新开组（与所跟随组的键相同则留在原组）。没有可跟随的组时照常新开。
  - 实现状态：已实现。本机验证：两位主控同屏同参数时只有一路编码，组员按 144 fps 转发；一方改画质即另开编码器；组长断开后约 0.12 s 内组员接替并继续出帧；任一方的 PLI 都让整组下一帧为关键帧并被双方收到。编码路数上限与最接近组的选择只有单元测试。不同网络条件下的多人共享、3 人以上、编码路数上限需双机测试。
- **音频：** 同一播放设备的系统声音只采集一路（`platform/windows/loopback/shared.rs`，2026-10-18），分发给每位参与者；各参与者仍按自己的音质设置编码。
- **控制权（`src/features/host/participants.rs`）：** 同一时刻只有一位参与者持有控制权，被控只注入持有者的键鼠，其他人的 INPUT 被丢弃。
  - **先到先得：** 主控开启键鼠控制时发送 SESSION `ControlTurn{take: true}`。无人持有且该参与者拥有 `control` 能力时获得控制权；否则被控回 `ControlTurnResult{held: false, message}`（“<持有者> 正在控制，对方释放控制权后才能开启键鼠控制”或“你没有这台设备的键鼠控制权限”），主控随即关闭键鼠控制并提示。他人持有时，主控工具栏的键鼠按钮不可用，悬停显示持有者。
  - **只由持有者释放：** 持有者关闭键鼠控制（`ControlTurn{take: false}`）、离开会话或失去 `control` 能力（服务端收回授权、主人“禁止控制”）时释放；没有抢占，持有者长时间不操作也不释放。被控在输入循环中每 200 ms、以及参与者列表变化时核对持有者的能力。持有者变化时被控释放原持有者按住的键与按钮。
  - **广播：** 持有者由 `PARTICIPANTS.controlling` 广播。主控发现自己不再是持有者（且没有尚未回复的 `ControlTurn`）时关闭键鼠控制并提示“被控端收回了本机的键鼠控制权”。获取控制权的 `ControlTurn` 发不出去或被控回错误时，主控关闭键鼠控制并提示“未能向被控端请求键鼠控制权”（自动恢复时不提示）。
  - **自动恢复：** 主控重连后按上次的状态自动开启键鼠控制时，发送 `ControlTurn{take: true, automatic: true}`。被控只在该参与者已经持有控制权、或会话中只有它一人时批准；多人时必须由用户手动开启。自动恢复被拒绝时主控静默回到仅观看，不弹提示。主控已经知道有其他参与者时，不发起自动恢复。
  - **禁止控制：** 被控主人可以在参与者列表中对某人“禁止控制”：本次会话内该参与者的有效能力去掉 `control`（`PERMISSIONS` 随之更新，其持有的控制权随之释放，剪贴板也不再自动同步），“允许控制”恢复。服务模式下经常驻进程的 `BlockControl` 请求执行。
  - 实现状态：已实现。本机验证（同机多个 profile）：先到者获得控制权，后到者被拒绝并看到持有者；持有者释放后另一人可以获得；会话中收回 `control` 授权约 2 ms 内释放，主控随即提示；没有 `control` 能力时请求被拒绝；持有者离开即释放；上次开着控制的参与者在他人已连接时加入不会自动获取控制权，独自连接时照常自动恢复。两台以上真实主控同时操作需双机测试。
- **采集：** 同一进程内同一输出只有一路 DXGI 桌面复制，所有参与者共享；每个编码组（见上）各自缩放、HDR 转换与光标合成后编码一次。
- **剪贴板：** 只有一位参与者时按其 `clipboard` 能力自动双向同步；多人时只有持有控制权的参与者自动同步，其他参与者既不接收也不覆盖被控剪贴板。拥有 `clipboard` 的其他参与者可以手动获取被控当前剪贴板（CLIPBOARD v2 `Pull`，§8.2；主控菜单“获取被控剪贴板”）。
- **麦克风：** 被控只有一个虚拟麦克风所有者（混音线程），各参与者的 NetEq 输出逐 10 ms 相加并软限幅后写入虚拟麦克风。每人的开关行为保持原实现：关闭只是静音自己，端点在仍有连接保留时不拆除。开启麦克风只需要 `microphone` 能力，不要求键鼠控制或控制权。
- **发送端信息：** `SenderInfo` 按参与者发送各自的编码器与采集后端。
- **显示设置：** 需要 `display` 能力；分辨率、DPI、虚拟屏、超级屏变更成功后，被控向其他参与者（不含操作者）发送 `HOST_EVENT{DISPLAY_CHANGING, 2000, "<昵称> 修改了被控端的显示设置"}`，主控显示为提示。

### 4.3 参与者光标

- 主控在 control 通道发送 `POINTER` 帧：`{seq, screen_id, x, y（屏内归一化）, visible, click?{button, phase}}`。观看者也发送，用作“指示光标”，不注入输入。
- 被控把各参与者光标：
  1. 绘制到被控物理屏的覆盖层（UIAccess 常驻进程，与批注覆盖层同源）；
  2. 以 `CURSOR.Participants` 转发给其他参与者，由他们本地绘制。
- **样式：**
  - 颜色按加入顺序取调色板，用户可在 HELLO 的 `profile.color` 中指定。
  - 标签显示昵称；参与者面板中悬停显示设备名。
  - 空闲 3 s 淡出。
  - 点击反馈：左键蓝色、右键橙色圆环。
  - 当前控制者的光标即被控真实光标，覆盖层为其加上身份标签。
- 删除批注中的“鼠标指示”工具（`Tool::Pointer`），激光笔保留。

### 4.4 多人观看流（`src/features/host/shared_view.rs`）

- 有两位及以上参与者时，除控制权持有者外的所有参与者（观看者）看同一路“多人观看流”：1080p 档（画质档 2，按屏幕比例缩放，例如 16:10 屏为 1728×1080）、60 fps、观影模式、合成光标、SDR。
  - **编码格式：** 取所有观看者都能解码的最优 4:2:0 格式，顺序为 AV1 10 bit、AV1 8 bit、HEVC 10 bit、HEVC 8 bit、H.264；10 bit 只在该会话允许 SDR 10 bit 时计入。被控先找完全匹配该格式的编码候选，找不到才退回其他候选。
  - **共享编码：** 观看者的分组键因此相同，按 §4.2 合为一组，同一屏幕只编码一次。
  - **码率：** 由画质档 2 的档位上限决定，并随组内最弱成员的可用码率下降。
- 持有者使用自己的画质设置（画质、帧率、编码偏好、HDR、观影模式开关），可以使用低延迟播放。
- **只换本次参数：** 被控只在采集循环中替换本次使用的参数，不改动参与者保存的设置。
  - 获得控制权或只剩一人时立即恢复其设置。
  - 失去控制权时切回观看流：新参数在下一次迭代生效，通常加入已有的组并从关键帧开始。
- **主控：** 处于观看流时（参与者多于一人且本人不是持有者），画质菜单显示“多人观看：统一 1080p · 60 FPS · 观影模式”。
  - 编码偏好、网络不佳时、观影模式、画质、帧率、色度与 HDR 不可修改。
  - 播放强制使用观影模式缓冲。
- **目的：** 减少被控的编码路数与上行带宽，所有观看者的体验一致。
- **实现状态：** 已实现。本机验证（两位主控）：
  - 一人控制时，另一人为 1728×1080 · 60 fps，帧缓冲约 118 ms（观影缓冲 120 ms）；控制者按自己的设置（30 fps，或自动档 144 fps），帧缓冲约 5 ms。
  - 控制者释放后约 3 ms 内加入观看流的组，并收到转发的共享关键帧；控制权移交后，原控制者成为观看流的组长。
  - 三位以上观看者、解码能力不同时的格式协商、多显示器需双机测试。

## 5. 对端协议 FPP v2

### 5.1 帧头

- 仍为 10 字节：`"FW" | ver=2 | flags | type u16 | request_id u32`。
- `flags`：bit0 RESPONSE，bit1 ERROR，bit2 MORE（分片未完）。
- 请求的 `request_id` 非 0，由发送方递增；响应带同一 id 并置 RESPONSE。失败时置 ERROR，帧体为 `PeerError`。
- 推送（无需回复）的 `request_id` 为 0。

```
PeerError { code: enum, message: string, detail: string }
code: UNSUPPORTED | BAD_REQUEST | FORBIDDEN | NOT_FOUND | BUSY | UNAVAILABLE | INTERNAL | TIMEOUT
```

- 收到未知帧类型：带 request_id 的回 `ERROR{UNSUPPORTED}`，推送直接忽略。

### 5.2 帧类型

| 类型 | 值 | 通道 | 说明 |
| --- | --- | --- | --- |
| HELLO | 0x0001 | session | 版本与特性协商（§5.4） |
| INPUT | 0x0101 | control | 键鼠（§7） |
| POINTER_POLICY | 0x0102 | control | 指针模式（原 MOUSE_POLICY） |
| CURSOR | 0x0103 | control | 被控光标形状 / 状态、参与者光标 |
| TOUCH | 0x0104 | control | 触摸触点（2026-10-17 实现，见 peer-messages.md） |
| PEN | 0x0105 | control | 笔样本（2026-10-17 实现） |
| POINTER | 0x0106 | control | 参与者指针（§4.3） |
| SESSION | 0x0201 | session | 媒体与显示控制（§6） |
| AUDIO | 0x0202 | session | 音质与麦克风控制（原 AUDIO_QUALITY） |
| HOST_EVENT | 0x0203 | session | 被控生命周期事件 |
| CLIPBOARD | 0x0301 | session | 剪贴板控制面（§8.2，数据走 TRANSFER） |
| FILE | 0x0401 | session | 文件管理与传输控制面 |
| TRANSFER | 0x0402 | bulk | 统一传输数据块（§8） |
| DRAG | 0x0501 | session | 拖放控制面（§8.3，数据走 TRANSFER） |
| ANNOTATION | 0x0601 | session | 批注 |
| PORT | 0x0701 | tunnel | 端口转发控制与 TCP 数据 |
| PORT_DGRAM | 0x0702 | dgram | UDP 数据报 |
| DIAGNOSTICS | 0x0801 | session | 诊断包控制面（§8.4，数据走 TRANSFER） |
| PERMISSIONS | 0x0901 | session | 被控 → 主控：本会话生效能力 |
| PARTICIPANTS | 0x0902 | session | 参与者列表与控制权持有者 |
| FLOW | 0x0A01 | 各数据通道 | 信用窗口更新（§8） |

### 5.3 数据通道（全部预协商，`negotiated: true`）

| 标签 | id | 可靠性 | 用途 |
| --- | --- | --- | --- |
| `farwing.control.v2` | 0 | 有序可靠；协商 FCT 后经 FCT | INPUT / POINTER / CURSOR / POINTER_POLICY |
| `farwing.session.v2` | 2 | 有序可靠 | HELLO / SESSION / AUDIO / HOST_EVENT / CLIPBOARD / FILE / DRAG / ANNOTATION / DIAGNOSTICS / PERMISSIONS / PARTICIPANTS |
| `farwing.bulk.v2` | 4 | 有序可靠 | TRANSFER、FLOW（文件、剪贴板、拖放、诊断包共用） |
| `farwing.tunnel.v2` | 6 | 有序可靠 | PORT、FLOW |
| `farwing.dgram.v2` | 8 | 无序，maxRetransmits=0 | PORT_DGRAM |

- 单个数据帧不超过 64 KiB；更大的消息按 §5.1 用 MORE 分片。
- 每个连接只有一个写出调度器，优先级 control > session > tunnel > bulk。bulk/tunnel 在 SCTP 中排队的字节限制在 256 KiB 内。
- bulk 通道的所有发送方（FILE、CLIPBOARD、DRAG、DIAGNOSTICS）共用一个写出闸门（`src/transport/bulk.rs`）：同一个 `on_buffered_amount_low` 回调（低水位 128 KiB）唤醒所有等待者，检查空间与入队串行进行，合计排队不超过 256 KiB。
- v1 的 `farwing.diag` 诊断通道与 `DRAG_DATA`（0x0502）帧类型已删除。
- `on_message` 只按帧类型分发，不在接收回调里等待业务处理。

### 5.4 HELLO

```
Hello {
  protocol_version: 2,
  role: controller|host,
  client_version: string,
  platform: string,
  features: map<string, uint32>,     // 特性名 → 版本，只列运行时可用的
  profile: { nickname, color? },     // 参与者显示信息（主控）
}
```

- 主控在 session 通道打开后发送 HELLO 请求，被控以 HELLO 响应（RESPONSE）。只交换一次，5 s 超时视为失败。
- 帧头版本字节为 2，数据通道标签见 §5.3；版本不符的帧直接丢弃。
- 两端同为 v2 构建，特性集合相同；门控只看特性名，不再有数值“等级”。连接建立之前（HELLO 到达前）被控按完整 v2 特性集合准备会话。
- **特性名（v2 初始集合）：**
  - 画面与显示：`video.av1`、`video.hevc`、`video.h264`、`video.444`、`video.hdr`、`video.10bit`、`display.config`、`display.virtual`、`display.exclusive`、`display.geometry`
  - 音频：`audio.quality`、`audio.microphone`
  - 输入与光标：`input.keyboard_usage`、`input.pointer`、`cursor.shape`、`cursor.participants`
  - 数据特性：`clipboard`、`files`、`drag`、`annotation`、`ports.tcp`、`ports.udp`、`diagnostics`
  - 传输：`fct`
  - `touch`、`pen`：被控运行时具备合成指针才声明（2026-10-17）

## 6. SESSION v2：媒体与显示控制

帧体为 `SessionMessage { oneof body }`（protobuf，`src/features/stream_control/wire.rs`）。请求带非 0 的 FPP request_id，被控用同一 id 并置 RESPONSE 回复；推送的 request_id 为 0，不回复。被控收到不认识的请求时回 `RESPONSE|ERROR` + `PeerError{unsupported}`；主控把任何 ERROR 回复按“请求失败”处理。v1 的 `seq/timestamp` 信封、`RpcRequest/RpcResponse` 包装与重新编号全部取消。

### 6.1 推送

| tag | 消息 | 方向 | 内容 | 说明 |
| --- | --- | --- | --- | --- |
| 1 | `Screens` | H→C | `PbScreenSources{screens[], current_screen_id}` | 屏幕列表或任一屏幕变化时发送（字节相同不重发）。`PbScreen` 去掉恒定的 `dpr` 与 `builtin_screen_type`；`display_geometry`（tag 100）为驱动首选模式 |
| 2 | `Quality` | H→C | `{encoder_type, capture_type, probe_bps, video_quality}` | `video_quality` 为生效档位 1..4（自动已解析）或 6=自定义；主控仅在自动画质下用它更新自动档位。去掉只用于日志的四档参考码率 |
| 3 | `Microphone` | H→C | `{state: 1 ACTIVE / 2 INACTIVE / 3 RESTART}` | 被控虚拟麦克风状态；RESTART 表示被控音频链路出错，主控重启本地采集 |
| 4 | `ClipboardFiles` | H→C | `{enabled}` | 被控是否接收剪贴板文件 |
| 5 | `Capture` | C→H | `{screen_id, start}` | 开始/停止采集某块屏幕；`-1` 表示本会话全部屏幕（停止保留槽位，`start(-1)` 恢复）。无回复，状态变化以随后的 `Screens` 为准 |

### 6.2 请求与响应

| 请求 tag | 请求 | 响应 | 说明 |
| --- | --- | --- | --- |
| 10 | `CaptureSetting` | 20 `CaptureSettingResult{errors[]}` | 见 6.3 |
| 11 | `CreateVirtualDisplay{local_resolution[1], virtual_display_count=1}` | 21 `DisplayResult` | 0 成功；-2 没有空闲且已注册的视频轨道；-1 其他失败。需要 `display` 能力 |
| 12 | `RemoveVirtualDisplay{screen_id[1]}` | 21 | 只能删除 Farwing 创建的虚拟屏；0 / -1 |
| 13 | `EnterExclusive{reason 1 手动 / 2 提高帧率, resolution, dpi_scale, enter_fps}` | 21 | 0；502 参数无效；501 策略不允许或系统失败 |
| 14 | `QuitExclusive{}` | 21 | 0 / -1 |
| 15 | `VideoTracks{video_track_index[]}` | 21 | 主控已协商可接收的视频轨道（0..4）；采集新屏幕之前必须先确认 |
| 16 | `MicrophonePolicy{policy: bit0 开启}` | 22 `MicrophoneResult{error_code}` | 0；`0x0e003001` 组件不可用；`0x0e003006` 不允许；`0x0e003012` 初始化失败。需要 `microphone` 能力 |

### 6.3 CaptureSetting

字段沿用 v1 编号（`fps` 档位 1..4、`frame_quality` 2..6、`cursor_capture`、`screen_id`、`resolution_*`、`chroma_format`、`max_custom_bitrate`（bit/s）、`dpi_scale`、`resolution_type`、`enable_hdr`、`auto_frame_quality`、`codec_type`、`max_scale_*`、`fps_count`）。

- `screen_id = -2`：本会话所有正在采集或暂停的轨道，不能带分辨率或 DPI；`-1`：单轨道会话的唯一来源；其他值为具体屏幕。
- 带分辨率、DPI 或 `resolution_type` 2..4 的请求是显示变更，需要 `display` 能力，否则返回 -1。
- 结果码：`0` 成功；`-1` 无效或不支持；`-2` 正在更新或系统执行失败；`-3` 请求帧率超过屏幕刷新率（提示，不算失败）；`-5 + {"error_code":0|501}` 分辨率转为独占屏的结果；`-6 + {"error_code":0|1|2|5}` 全彩（4:4:4）开关结果（0 已开启，1 编码器不支持，2 支持但未生效，5 已关闭）。
- 每个屏幕一条结果；`-2` 时按屏幕拼接，全部为空时补一条 `0`。
- v2 新增：`19 weak_network`（0 优先降码率，1 优先降帧率）、`20 cinema`（观影模式）。会话起始参数 `CaptureParams` 带同名字段，主控按设备记住并在首个 CaptureSetting 中发送。

### 6.3.1 码率与画面策略（P5）

- **自动档位只在开始时决定：** 有画面后约 8 s 内按带宽估计选定档位（输出尺寸），之后固定。换屏幕或切换到“自动”时重新决定；网络路径变化不会改尺寸。
- **弱网策略：** 档位（尺寸与码率上限）不变。
  - 优先降码率：码率随拥塞控制下降，降到档位上限的 1/4 后开始按比例降帧率（最低 30 fps）。
  - 优先降帧率：按“档位码率 / 帧率”保持每帧码率，帧率最低 30 fps，之后码率继续下降。
  - 帧率每次至少降 5 fps（间隔 ≥ 0.5 s），回升每次最多 10 fps（间隔 ≥ 3 s）。
- **观影模式：**
  - 被控：码控窗口约 1 秒（NVENC/AMF/QSV 的 VBV），每帧分散在约 0.9 个帧间隔内发出。
  - 主控：视频播放缓冲 120 ms（最多 500 ms，忽略发送端的 playout-delay 提示），NetEq 最小延迟 100 ms，音画同步；交换链仍按刷新率出帧。
  - 延迟告警：每帧在缓冲中实际等待的时间（帧组装完成到放行解码）计入“估算帧延迟”和“本地单帧”的显示值，但判断告警等级时扣除，因为这是用户选择的延迟。超出缓冲的部分照常告警。统计面板注明“含观影缓冲 N ms”，并单列“其中观影缓冲”（`src/diagnostics/performance/health.rs`）。
- **起始码率：** 被控记住每个主控设备上次会话（≥ 15 s）收敛到的带宽估计，7 天内的下次会话从其 80% 起步（不超过档位上限）。
- **发送节奏：** 每帧在 α × 帧间隔内发出（4–30 ms），α 初始 0.4，丢包 ≥ 2% 时增大（上限 0.9），5 s 无丢包后逐步回到 0.3。
- **快速关键帧：** 主控在缺包超过 max(2.5 × RTT, 60 ms) 或 NACK 放弃时立即发 PLI（间隔 ≥ max(100 ms, 1.5 × RTT)），不再等待约 1 s 的解码超时；被控对 PLI/FIR 的节流同为 max(100 ms, 1.5 × RTT)。
- **RTP 头扩展：** 音频只协商 transport-cc 与 mid；视频协商 transport-cc、mid、video-timing、`urn:farwing:rtp-hdrext:sending-delay`、color-space、`urn:farwing:rtp-hdrext:capture-index`、`urn:farwing:rtp-hdrext:new-frame`。共 7 个扩展（音频与视频共用编号），始终使用一字节扩展头。删除 abs-send-time、toffset、playout-delay（主控本地决定播放缓冲）、CVO、video-content-type、rid。
- **FEC：** 每帧一个 RSFEC 块，修复包紧跟该帧发出；开销受 FEC 预算（目标码率 × 丢包率，5%–35%）限制。
- **接收端统计：** 主控每秒推送 SESSION `7 ReceiverStats{receive_fps, decode_fps, render_fps, decode_delay_ms, dropped_frames, display_hz}`。被控在连续 3 次“解码跟不上”（解码帧率 < 接收帧率 × 0.85，或解码耗时 > 1.5 × 帧间隔）后把帧率上限压到解码帧率（取 5 的倍数，最低 30）；连续 10 次健康且解码耗时 < 0.7 × 帧间隔后取消上限。

### 6.4 其他帧

- **光标形状**：`CURSOR` 帧（control 通道，FCT 时经 FCT），帧体 `CursorMessage{shape}`；`shape.cursor_type = -1` 表示隐藏。
- **被控事件**：`HOST_EVENT`（§10）。被控本机安装更新时发送 `UPDATING`，主控显示“被控端正在更新”并在其重新上线后自动重连。
- **能力**：`PERMISSIONS{grants[]}`，握手完成后与能力变化时由被控发送；主控据此启停文件传输等功能。

### 6.5 删除

v1 的 `SimpleAction`（锁屏、显示桌面、任务视图、任务管理器、锁定键、延后更新、quitCapture、刷新状态）、`CaptureChange`、`ReportError`、`SystemMetrics`、`QuerySystemState`、`InputEvent`（SESSION 内触摸）、`MouseSwitch`、官方批注 `Draw` RPC（批注只走 ANNOTATION 帧）、`SystemStateChange` 的安全桌面 / 权限 / 私密屏 / UI 就绪状态全部删除。系统命令（锁屏、任务管理器等）在 P5 作为 INPUT `SystemCommand` 重新提供。

## 7. 输入 v2（P5）

帧体为 `InputMessage { oneof kind }`（protobuf，`src/protocol/input.rs`），tag 如下：

```
1 Key { usage: uint32 /* (HID page << 16) | id，键盘页 0x07、消费者页 0x0C */, down: bool, repeat: bool }
2 LockState { caps: bool, num: bool, scroll: bool, seq: uint32 }
3 MouseAbsolute { screen_id: int32?, x: double, y: double }   // 屏内归一化 0..1
4 MouseRelative { dx: sint32, dy: sint32 }                    // 被控像素
5 MouseButton { button: uint32 /* 1 左 2 右 16 中 32 X1 64 X2 */, down: bool }
6 MouseWheel { dx: sint32, dy: sint32 }                       // 一条消息同时携带两个轴，单位 1/120 格
7 SystemCommand: enum { UNSPECIFIED=0, SAS=1, LOCK=2, TASK_MANAGER=3, SHOW_DESKTOP=4, TASK_VIEW=5 }
8 Heartbeat {}
```

- **按位置传键：** 主控按物理键的 set-1 扫描码（E0 扩展键记为 `0xE0xx`）查表得到 usage；媒体键与 Pause / NumLock 按其固定 VK 识别。被控有 HID 驱动时直接上报 usage（键盘页 0x04–0xE7、消费者页 7 个媒体键），否则查反表以 `KEYEVENTF_SCANCODE`（E0 键加 `KEYEVENTF_EXTENDEDKEY`）注入；Pause、NumLock、Hanja、Hangul 无法用扫描码表达，以固定 VK 注入。两张表在 `src/platform/windows/input/keyboard.rs`。不认识的 usage 被忽略。
- **主控 IME：** 观看窗口获得焦点并处于控制模式时解除窗口的输入法上下文（`ImmAssociateContextEx(hwnd, NULL, 0)`），失去焦点或退出控制时恢复；键按原样发送，由被控的输入法组字。
- **重复：** `repeat` 表示已按下键的自动重复。被控对 SendInput 路径再注入一次按下，HID 路径由被控系统自行重复。
- **锁定键：** 按普通键发送。激活控制时、以及主控本地锁状态并非因已发送的锁定键而变化时（例如在其他窗口切换了 CapsLock），主控发送 `LockState`。`seq` 是此前已提交给传输的锁定键按下次数（不含重复）。被控按参与者统计收到的锁定键按下次数：若多于 `seq`，说明有按键与该状态交叉，不纠正；之后计数对齐到 `seq`。被控在本参与者最近的锁定键注入稳定（50 ms）且无锁定键按住后，只对不一致的位注入一次锁定键；`LockState` 之后的按键在纠正前同步翻转目标位。交接控制或断开时丢弃未执行的纠正。
- **系统命令：** 只由主控菜单显式发送，被控先释放该参与者按住的键和按钮再执行。`SAS` 经已安装的被控服务执行（否则报“Ctrl+Alt+Del需要安装并启用被控服务”），`LOCK` 调用 `LockWorkStation`，其余以与布局无关的 VK 注入 Win+D、Win+Tab、Ctrl+Shift+Esc。
- **保活：** 有按键按下时，主控在 50 ms 内没有发出任何 INPUT 帧就发一次 Heartbeat；任何 INPUT 帧都算保活。被控超时取 `max(150 ms, 3×RTT + 50 ms)`（RTT 上限按 3 s 计），取自该参与者的 RTCP RTT（被控传输层 `round_trip()`），不再读服务器配置。
- **删除：** VK 键码、UU 移动端动作（点击、比例相对移动、文本、组合键、SESSION 内触摸）、经键流识别 Ctrl+Alt+Del / Win+L、`Key.interruptible` 与 `Key.lock_on`。TOUCH / PEN 帧类型保留未实现。

## 8. 统一传输（P4）

```
TRANSFER { stream: uint32, kind: FILE=1|CLIPBOARD=2|DRAG=3|DIAGNOSTICS=4, offset: uint64, data: bytes(≤64 KiB), last: bool, entry: uint32 }
FLOW { channel: BULK=1|TUNNEL=2, stream: uint32, credit: uint64, kind }   // 累计允许发送的字节总数
```

- 帧格式定义在 `src/transport/transfer.rs`（`Chunk`、`Flow`、`Credit`、`Window`）。
- **流的创建：** 由各特性的控制面消息（FILE / CLIPBOARD / DRAG / DIAGNOSTICS）发起，携带清单（文件、大小、属性、纳秒 mtime）。`stream` 由发起请求的一方在本连接内分配，同一 `kind` 内唯一；`(kind, stream)` 决定接收方。CLIPBOARD、DRAG 与 DIAGNOSTICS 的请求方即数据接收方：它先登记流再发请求，因此数据块可以先于（session 通道上的）响应到达。收到 `TRANSFER` 的一方总是该流的接收方，收到 `FLOW` 的一方总是发送方，双方各自分配的流号不会混淆。
- **条目：** 一个流按清单顺序依次发送各条目（文件）。`entry` 是清单下标，`offset` 是该条目内的位置，条目的最后一块置 `last`（空文件只发一个空块）。
- **信用：** 按流累计计算（所有条目合计）。接收方初始授予 4 MiB，数据真正落盘或交付后才归还；空出四分之一窗口时发送 `FLOW`。窗口在交付速度跟得上时倍增（上限 64 MiB）。发送方不得超出信用，超出视为协议错误并终止该流。
- **续传：** 接收方一次性回复所有条目的续传偏移，并附上已写入前缀末尾（最多 1 MiB）的 SHA-256；发送方核对自己文件的同一段，不一致的条目从 0 开始。
- bulk 通道在 SCTP 中排队不超过 256 KiB，控制与会话消息不会被大文件阻塞。

### 8.1 FILE v2

帧类型 `FILE`（session 通道），帧体 `FileMessage { oneof }`（`src/features/file_transfer/wire.rs`），用 FPP request_id 关联请求与响应（主控发请求，被控以 RESPONSE 回复；失败回复 `Failed`，带 ERROR 标志的 `PeerError` 回复按 `Failed` 处理）。帧体超过 64 KiB 时按 §5.1 拆成同一 request_id 的多帧，除最后一帧外置 MORE；推送不分片。请求 60 s 未获响应视为失败。数据走 `TRANSFER{kind: FILE}` 与 `FLOW{channel: BULK, kind: FILE}`（bulk 通道）。

```
// 请求（主控 → 被控）与其响应
List { path }                         → Listing { path, entries: [Entry{ name, kind: FILE=1|DIR=2|DRIVE=3|SHORTCUT=4, size, mtime_ns, hidden, path, icon }] }
                                        // path 为空：驱动器与常用位置，Entry.path 为其绝对路径、icon 为位置图标；
                                        // 其余目录 Listing.path 为被控解析后的目录，条目路径 = Listing.path + name
MakeDir { parent, name }              → Done { path }          // 名称已存在时失败（EXISTS）
Rename { path, name }                 → Done { path }          // name 为同目录下的新名称
Remove { path, recursive }            → Done { path }
Exists { dir, names: [string] }       → Existing { present: [string], writing: [string] }
Upload { transfer, dir, folder, items: [Item{ path, size, mtime_ns, dir }], conflict: OVERWRITE=1|RENAME=2|SKIP=3, key }
                                      → Plan { transfer, items: [ItemPlan{ offset, tail_sha256, skip }] }
UploadStart { transfer, offsets }     → Done                   // 主控核对 tail 后确定的起点（计划值或 0）；随后主控在 bulk 上发送 TRANSFER
Download { transfer, paths, key }     → Manifest { transfer, folder, items: [Item] }
DownloadStart { transfer, items: [ItemPlan{ offset, tail_sha256, skip }] }
                                      → Accepted { offsets }   // 被控核对 tail 后的最终起点（计划值或 0）；随后被控发送 TRANSFER
Cancel { transfer, key, discard }     → Done                   // 停止传输后回复；默认双方保留部分文件以便续传，
                                                               // discard 时被控删除 key 的部分文件与记录（取消上传任务；无传输时 transfer 为 0）
// 推送（被控 → 主控，request_id 0）
ItemDone { transfer, entry, error_code, message }              // 上传：条目已落盘（error_code 0）或失败；下载：被控读取失败时报告；
                                                               // entry 为 0xFFFFFFFF 表示整个传输失败（如权限被撤销）
// 失败响应
Failed { code: PROTOCOL=1|DENIED=2|NOT_FOUND=3|EXISTS=4|NO_SPACE=5|IO=6|BUSY=7|CHANGED=8|CANCELLED=9, message }
```

- 路径用 `/` 分隔的相对路径（`Item.path`），绝对路径只出现在 List/MakeDir/Rename/Remove/Exists/Upload/Download 的远端路径中。被控拒绝 `..`、设备名、链接与重解析点。
- **清单：** 包含文件与空目录（`Item.dir`，`size` 为 0）；非空目录由其下的条目隐含。上传或下载单个文件夹时 `folder` 为其名称、条目相对该文件夹；单个文件时 `folder` 为空。`Download.paths` 有多项时须位于同一目录，条目相对该目录。
- **条目数据：** 按清单顺序发送未 `skip` 的条目，`TRANSFER.entry` 为清单下标、`stream` 为 `transfer`；空文件与空目录各发一个空的 `last` 块。信用只计数据字节，接收方写入磁盘后才通过 FLOW 归还。
- **续传：** 被控按 `key` 保存上传记录与部分文件；`Plan`/`DownloadStart` 中 `offset` 为接收方已有的字节数，`skip` 表示已保存或按 SKIP 策略跳过。部分文件已不存在但目标文件大小与 mtime 与条目一致时视为已保存。
- 部分文件写在目标目录的隐藏临时文件 `.{名称}.{key}.downloading` 里，完成后按冲突策略改名为最终名称（RENAME 取空闲的“名称 (n)”，SKIP 在计划时与完成时遇到同名均跳过），并设置 mtime（100 ns 精度）、去掉隐藏属性。上传文件夹时 RENAME 对顶层文件夹同样取空闲名称，OVERWRITE/SKIP 并入已有文件夹。
- 被控的磁盘操作始终在被控用户的普通权限进程中执行（安装服务模式下为用户子进程），不在 SYSTEM 中执行。

### 8.2 CLIPBOARD v2

帧类型 `CLIPBOARD`（session 通道），帧体 `ClipboardMessage { oneof }`（`src/features/clipboard/wire.rs`）。两端都可发请求：用 FPP request_id 关联，响应置 RESPONSE，失败回复 `RESPONSE|ERROR` + `PeerError`；推送的 request_id 为 0。超过 64 KiB 的消息（文件清单）按 §5.1 分片。内容按需渲染：先发格式清单，本机应用粘贴时才取内容。

```
// 推送
Offer { offer, formats: [Format{ id, name }] }   // 新的本机剪贴板；offer 为发送方递增的编号；formats 为空表示清空
Release { offer }                                // 接收方不再读取 offer 的文件，发送方释放其文件清单
Abort { stream }                                 // 请求方放弃 stream，发送方停止发送
// 请求 → 响应
Fetch { offer, format, stream }       → Data { size, inline }      // 某个格式的内容
ListFiles { offer }                   → Files { items: [FileItem{ path, attributes, mtime, size }] }
ReadFile { offer, index, offset, length(≤1 MiB), stream } → Data { size, inline }
Pull {}                               → Offer { offer, formats }   // 主控 → 被控：立即提供被控当前剪贴板
```

- **格式：** `id` 为 Windows 剪贴板格式号，`name` 为注册格式的名称（接收方按名称在本机注册）；文件以 `FileGroupDescriptorW` + `FileContents` 表示。`FileItem.path` 为 `\` 分隔的相对路径（含顶层名称），`attributes` 为 Windows 文件属性（0x10 目录），`mtime` 为 FILETIME（100 ns，1601 起）。接收方校验名称安全（拒绝 `..`、设备名、非法字符）。
- **内容：** `Data.size` ≤ 32 KiB 时内容在 `inline` 中；更大时 `inline` 为空，内容以 `TRANSFER{kind: CLIPBOARD, stream: 请求中的 stream, entry: 0}` 在 bulk 通道发送（最后一块置 `last`），按请求方的 `FLOW{channel: BULK, kind: CLIPBOARD}` 信用发送（初始 4 MiB）。单个格式最大 128 MiB。请求 30 s 无任何进展即放弃并发送 `Abort`。
- **文件：** 首个 `ListFiles` 时发送方固定该 offer 的文件清单，之后 `ReadFile` 按清单下标读取（每次最多 1 MiB，由接收方 IStream 读取驱动）；接收方结束粘贴或拖放后发送 `Release`。
- **失败码：** `not_found`（offer 已被替换、格式未提供、内容为空）、`forbidden`（文件剪贴板关闭、来源标记为不同步、主控收到 `Pull`）、`unavailable`（同步暂停或未登录桌面）、`bad_request`、`internal`。
- **自动同步与手动获取：**
  - 自动同步：本机复制时向对端发 `Offer`，并应用对端的 `Offer`。被控只对“自动同步”参与者这样做：单人会话中拥有 `clipboard` 的参与者；多人会话中同时拥有 `clipboard` 与 `control` 的参与者（§4.2）。主控只在控制模式（非“仅观看”）且开启“剪贴板同步”时自动同步。
  - 手动获取（`Pull`）：任何拥有 `clipboard` 能力的参与者都可以发，不要求 `control`，也不受主控“剪贴板同步”开关影响；被控本机的剪贴板开关仍是上限（关闭则 `unavailable`），文件仍受“文件复制”开关约束。被控取当时的系统剪贴板，按与自动同步相同的规则过滤（来源标记 `ExcludeClipboardContentFromMonitorProcessing`、`Clipboard Viewer Ignore`、`CanUploadToCloudClipboard=0`、`CanIncludeInClipboardHistory=0` 时回复 `forbidden`），登记为该参与者的新 offer 并在响应中返回；主控把它装入本机剪贴板（按需渲染），之后的 `Fetch`/`ListFiles`/`ReadFile` 与自动同步相同。只有 `view` 没有 `clipboard` 的参与者不能获取，因为剪贴板内容可能包含密码等敏感信息。
  - 主控界面：观看窗口菜单“获取被控剪贴板”，在剪贴板通道就绪时始终可用（自动同步时也可用，等同于重新取一次）。
- 实现状态：已实现并替换 v1 的 UU 形消息（`FormatList`/`FormatDataAsk`/`DataBlock`/`FileContentsRequest` 等、`err 1/2` 约定、文本改写请求、`DragDropAutoSave`/`OleDrop` 拖放动作）。被控执行仍在用户会话的剪贴板进程中（安装服务时为用户子进程，经管道转发帧）。单机验证见 `Farwing dev clipboard`（主控为内存剪贴板）；两台机器间的真实剪贴板同步须双机测试。

### 8.3 DRAG v2

帧类型 `DRAG`（session 通道），帧体 `Packet { token, drag, sequence, capabilities, oneof payload }`（`src/protocol/drag_drop.rs`）：`Hello`/`HelloAck`/`Available`、`Begin`/`Position`/`Probe`/`Commit`/`Cancel`、`Feedback`/`Finished`/`Progress`、`Image`（≤ 120×120 BGRA）、`SourceReleased`/`Resume`/`Resumed`/`HandoffDone`（拖回主控），以及：

```
Files(bytes)    // 本次拖放的文件交换：一个完整的 CLIPBOARD 帧（§8.2 的 Offer/ListFiles/Files/ReadFile/Data/Release/Abort）
```

- 每次拖放有独立的文件交换（不读写系统剪贴板）：拖出方发 `Offer`（`FileGroupDescriptorW` + `FileContents`），落点方在 OLE 目标读取时发 `ListFiles`/`ReadFile`。只接受文件格式，`Fetch` 与 `Pull` 回复 `forbidden`。
- 文件内容为 `TRANSFER{kind: DRAG}` / `FLOW{kind: DRAG}`，直接在 bulk 通道发送（不包在 DRAG 帧里）；流号在进程内唯一，接收方据此找到对应的拖放。
- 单个 DRAG 帧不超过 64 KiB。v1 的 `Data` 负载（tag 10）与 `DRAG_DATA` 帧类型删除。
- 实现状态：已实现。被控端“发送到主控”窗口（UU 主控的 `DragDropAutoSave` 回退路径）在 v2 中不可达，已随 `FileDrop` 发布删除；拖放两个方向都只走原生拖放。原生拖放需要真实鼠标拖动，单机只验证了协议层（文件交换的回环测试与帧编码）；端到端拖放须双机测试。

### 8.4 DIAGNOSTICS v2

帧类型 `DIAGNOSTICS`（session 通道），帧体 `DiagnosticsMessage { oneof }`（`src/diagnostics/remote/mod.rs`）。被控只发送自己的脱敏诊断包，不接受路径或任意文件请求。

```
Status { allowed }                          // 推送 被控 → 主控：本参与者能否获取（session 通道打开时与变化时）
Request { stream }                          // 请求 主控 → 被控 → Ready，或 RESPONSE|ERROR（forbidden / unsupported / busy / internal）
Progress { stream, stage, completed, total } // 推送 被控 → 主控：生成进度（约 200 ms 一次）
Ready { size, sha256, logs, warnings }      // Request 的响应；随后包内容以 TRANSFER{kind: DIAGNOSTICS, stream} 发送
Cancel { stream }                           // 推送 主控 → 被控：停止
Failed { stream, message }                  // 推送 被控 → 主控：Ready 之后发送中断（读取失败、权限撤销）
```

- 允许条件：主控声明 `diagnostics` 特性，且本参与者拥有 `diagnostics` 能力（访客默认权限不含该能力）。
- 主控先建立 `.partial` 文件再发请求，按 `FLOW{kind: DIAGNOSTICS}` 归还信用（写入磁盘后），收到最后一块后核对大小与 SHA-256，成功才改名为最终文件；任何失败都删除部分文件。
- 实现状态：已实现，替换 v1 的独立 `farwing.diag` 通道、JSON 控制消息与逐块 ACK（停等）。`Farwing dev diagnostics` 在单机经真实会话验证。

## 9. 端口转发 v2（P4）

```
PortMessage { oneof {
  Open { stream: uint32, proto: TCP|UDP, host: string, port: uint32 }    // 主控分配 stream
  OpenResult { stream, code: OK|REFUSED|UNREACHABLE|FORBIDDEN|TIMEOUT|DNS_FAILED, message }
  Data { stream, data: bytes(≤64 KiB) }                                  // TCP，tunnel 通道
  ShutdownWrite { stream }                                               // 半关闭
  Reset { stream, code }
}}
PortDatagram { stream: uint32, data: bytes }                             // PORT_DGRAM，dgram 通道
```

- 目标可以是任意地址，主机名在被控侧解析。
- TCP 两个方向各自结束；信用按 stream 计算。
- UDP 走无序不重传的 dgram 通道；超出预算只丢弃单个数据报。
- 规则（本地监听 → 目标）只存在于主控，不上线。
- 探测默认只做 TCP 建连；HTTP 探测由用户手动触发。
- **实现（`src/features/port_mapping/`）：**
  - 通道：`farwing.tunnel.v2` 有序可靠，承载 PORT 与 `FLOW{channel: TUNNEL}`；`farwing.dgram.v2` 无序且 `maxRetransmits = 0`，承载 PORT_DGRAM。
  - 每个 TCP stream 初始信用 4 MiB，接收方写入本地套接字后归还。tunnel 在 SCTP 中排队不超过 256 KiB，dgram 超过 1 MiB 时丢弃新数据报。
  - `OpenResult.code`：`OK=0 REFUSED=1 UNREACHABLE=2 FORBIDDEN=3 TIMEOUT=4 DNS_FAILED=5 LIMIT=6 PROTOCOL=7 CLOSED=8`；`Reset` 复用同一组代码，`CLOSED` 表示正常结束。
  - 被控只连接单播地址（拒绝未指定、组播、广播地址），名称解析超时 5 s，TCP 连接超时 10 s，UDP 关联空闲 60 s 结束。

## 10. 被控生命周期事件

```
HostEvent { kind: UPDATING|RESTARTING|SLEEPING|SHUTTING_DOWN|SESSION_SWITCH|DISPLAY_CHANGING, expected_downtime_ms, message }
```

- 主控收到后进入“等待设备”状态（最长 5 分钟，可取消）。被控重新上线后，由在线推送触发自动重建会话，并恢复窗口布局与画面设置。
- 远端触发更新已取消，只保留被控本机更新时发送 `UPDATING`。
- 实现状态：
  - 被控执行远程重启 / 关机前，经参与者发布任务向所有参与者发送 `RESTARTING`（预计 90 s）/ `SHUTTING_DOWN`，约 400 ms 后以 `host_closed` 结束会话；主控把 `host_closed` 视为“等待设备”（§11 第 2 级）。
  - 有参与者时，被控在系统即将睡眠 / 休眠时发送 `SLEEPING`（挂起通知保留约 300 ms 让该帧发出），在活动 Windows 会话变化（切换用户、远程桌面接管控制台）时发送 `SESSION_SWITCH`（预计 5 s）。`DISPLAY_CHANGING` 在参与者修改显示设置后发给其他参与者（§4.2）。
  - 等待期间由在线推送触发检查：本进程任一在线连接收到设备推送（设备上线时服务器推送 `device.changed`）就立即查询该设备状态；另以 5 s 的慢速轮询兜底（没有在线连接的进程）。

## 11. 连接（P6）

- **信令复用（已实现）：** 主控进程已有本设备、本账号的在线连接（presence）且处于连接状态时，会话信令复用它（`sid` 区分），不再为该会话新开 WSS；否则沿用按会话签发票据、单独建立主控连接的方式（独立主控进程尚未上线、服务模式下界面进程没有自己的在线连接、在线连接正在重连等）。
  - `POST /api/v2/sessions` 请求带 `"presence": true` 时服务端不签发票据，响应中 `ticket` 为空字符串，其余字段（`session_id`、`target`、`bitrate_limit_mbps` 等）不变。会话仍须在票据有效期内开始，否则按过期结束。
  - 主控在在线连接上发送带 `sid` 的 `session.start`；服务端只接受会话主控设备自己的在线连接、且该连接的账号与创建会话的账号相同（其他设备、被控设备本身发送均回 `forbidden`）。此后该会话的 `rtc.offer`/`rtc.candidate`（主控 → 被控）从这条连接接受，`rtc.answer`/`rtc.candidate`/`session.setting`/`session.permissions`/`session.ended`（→ 主控）发往这条连接。一个以票据建立了主控连接的会话改由在线连接开始时，原主控连接以 4001 关闭。
  - 主控离开会话发送 `session.leave{sid}`（代替主控连接上的 `bye`）；会话结束时只发 `session.ended`，不关闭在线连接。在线连接 `bye`、被新的在线连接取代、恢复窗口过期或凭据被撤销时，它承载的主控会话以 `controller_left` 结束；连接只是暂时断开（可恢复）时会话保留，与主控连接相同。
  - 客户端：在线连接的套接字工作线程按 `sid` 与请求 ID 把这些会话的消息交给对应的主控会话，其余仍交给被控会话；主控会话的请求 ID 带各自前缀，避免同一连接上互相冲突。在线连接中断且无法恢复时，承载的主控会话按“实时连接中断”进入第 2 级恢复，重建时若在线连接不可用则自动改用单独连接。ICE 重启、身份校验（§1.4）与恢复逻辑两种方式相同；直连会话（§12）不受影响。
  - 诊断：环境变量 `FARWING_SIGNAL_PRESENCE=off` 强制单独连接；`=wait` 让独立主控进程在创建首个会话前最多等 10 s 自己的在线连接。
- **首连：** 被控收到 `session.incoming` 后立即应答 SDP（媒体轨道预先协商）；媒体准备与 ICE 并行。主控的连接时限分两段：等被控应答最多 40 s（被控需要重新检查编码器时约 10–15 s 才应答），收到应答后 12 s 内连通（网页端 15 s）。编码能力探测结果按（适配器、驱动版本、GPU 调度设置）缓存到磁盘。
- **NAT 探测（已实现，`src/transport/nat/discovery.rs`）：**
  - 服务端以 `[[turn.stun]]` 配置多个 STUN 端点（内置模式），在这些地址上应答 RFC 5780 行为探测（OTHER-ADDRESS、CHANGE-REQUEST），并经 `GET /api/v2/server` 的 `features.nat_discovery`（`stun:ip:port`）告知客户端。
  - 客户端用一个本地 UDP 套接字（绑定默认路由的 IPv4 地址）探测：先向主端点做绑定，再做过滤测试（CHANGE-REQUEST 改 IP 与端口 / 只改端口），最后向其他端点做映射测试（RFC 5780 顺序：另一 IP 同端口、另一 IP 另一端口、同 IP 另一端口）。过滤测试必须在映射测试之前，否则映射测试会先为其他服务端地址打开过滤。
  - 映射与过滤各分三类：`eim`（与端点无关）、`adm`（与地址相关）、`apdm`（与地址和端口相关），无法判定为 `unknown`。服务端只有一个 IP 时：同 IP 两端口能判出 `apdm`，否则映射为 `unknown`；改 IP 不可用（420）时，过滤只能区分 `adm` 与 `apdm`。两类无法区分时报告较严格的一类（预测对两者处理相同）。
  - `port_delta`：映射与目的地相关时，按创建顺序取各个新映射端口的步长；至少两个步长一致、外部 IP 相同且 |步长| ≤ 64 时才给出，否则为 0。服务端只有两个 IP 时，`adm` 只有两个映射，不给步长。
  - 结果按网络指纹（默认路由的本地地址 + 网关）缓存 10 分钟（失败缓存 1 分钟）。实时连接建立时（被控上线即发生）启动后台任务，在网络变化（接口、地址、路由通知，2 s 去抖）与缓存到期时重新探测。会话只读缓存：有正在进行的探测时最多等 300 ms，否则不带结果继续。
- **NAT 信息交换（SDP 属性）：** offer 与 answer 都在会话级加入 `a=x-farwing-nat:mapping=<eim|adm|apdm|unknown>;filtering=<eim|adm|apdm|unknown>;delta=<i32>`。SDP 对服务端不透明，服务端无需改动；身份签名只覆盖 DTLS 指纹，不受该属性影响。无结果时发送 `unknown/unknown/0`；关闭激进打洞时发送 `delta=0;punch=off`，对端因此既不预测本端端口，也不向本端探测。ICE 重启时属性随新 offer/answer 更新。
- **端口预测（激进打洞）：** 对端映射为 `adm`/`apdm` 且 `delta≠0`，且本端不是“`eim` 映射 + `eim`/`adm` 过滤”（这种情况下对端的新端口会作为 prflx 候选被接受，无需预测）时，对端每个 UDP/IPv4 srflx 候选在加入之后，再经正常的 `add_ice_candidate` 加入 `port + delta·k`（k = 1..8，端口 ≥ 1024）的预测候选：沿用原候选的 foundation、ufrag 等字段，优先级每步降低一个本地偏好单位（`priority − k·256`）。远端 SDP 内已带的 srflx 候选同样处理。EIM/EIM 永不预测。
- **端口映射（`src/transport/nat/portmap.rs`）：** ICE 代理每个网络一个 UDP 套接字、端口随机；不固定本地端口，而是在代理发布默认路由上的私网 UDP/IPv4 主机候选后，为该端口向网关申请映射（每会话最多 2 个）。依次尝试 PCP（MAP，5351）、NAT-PMP（无应答则跳过）、UPnP-IGD（SSDP 搜索 IGD:1/2，`WANIPConnection:2/1` 或 `WANPPPConnection:1` 的 `AddPortMapping`，冲突 718 时换随机高端口，不接受永久租约）。外部地址为公网地址时，以 srflx 候选（类型偏好 100，沿用主机候选的本地偏好，`raddr/rport` 为主机地址）经本端正常候选通道发给对端；与代理已发布的 srflx 地址相同则不重复发布；外部地址为私网/共享地址（多层 NAT）时不发布。租约 2 小时，半程续期；会话关闭时删除（最多等 2 s，超时则随租约过期）。网关支持的协议按网关缓存 10 分钟。
- **设置：** 每个用户配置文件的 `network.json`：`aggressive_punching`（“激进打洞（对称型 NAT）”）与 `port_mapping`（“允许 UPnP / NAT-PMP / PCP 端口映射”），默认均开启，位于“设置 → 网络与直连”；主控与被控均适用，会话开始时读取。
- **IPv6 优先**：内置 ICE 的候选优先级为 `类型偏好<<24 | (网络偏好<<8 | IP 优先级)<<8 | 255`。网络按适配器类型、再按 IP 优先级（RFC 6724：全局 IPv6 40 > IPv4 30；ULA 50；6to4、Teredo 低于 IPv4）排名，同一适配器上 IPv6 网络的网络偏好更高，因此 IPv6 主机候选优先于 IPv4 主机与 srflx 候选。UDP6/TCP6 默认启用（未设置 `set_network_types` 即四种都收集），无需改动。
- **未经验证（需双机测试）：** 真实网关上的 PCP / NAT-PMP / UPnP 映射、续期与删除；真实 NAT 上的行为分类与步长；预测候选对对称型 NAT 的实际打通率；IPv6 直连优先的实际选路。以上目前只有报文编解码、分类逻辑（模拟响应）与本地回环探测的单元测试。
- **直连与中继的自动切换（已实现，`vendor/webrtc-ice/src/candidate/path_quality.rs`、`agent_selector.rs` `choose_path`）：** 国内运营商常对 P2P 的 UDP 流做 QoS：直连表现为持续丢包（像 Wi‑Fi 不稳），但中继不受影响。只看连通检查发现不了（检查包小而少，常能通过），所以按实际媒体丢包判断，并在丢包造成的卡顿与中继绕路的延迟之间权衡。
  - 中继候选对作为备份保持同意检查（剪枝时每种中转——UDP、TCP / TLS——各保留第一条活动的候选对）。控制方（主控）每秒为各候选对记录检查发送 / 应答计数与平滑 RTT；当前选中的路径另记主控收到的视频包（应收 / 实收，所有画面合计，重传与 FEC 不算）。路径分三类：直连、UDP 中转、TCP / TLS 中转（按本端到中转服务器那一段的协议；中转服务器到对端总是 UDP）。当前路径以外的两类各取 RTT 最低的一条，每 250 ms 探测一次。TCP / TLS 中转只在本端那一段丢包时因重传而卡顿，运营商限速 UDP 时它常不受影响，所以两种中转之间不按规则排先后，只按实测代价比较。
  - 每条路径 10 s 窗口（至少 5 s 才判断）：RTT 取窗口内平滑 RTT 的最低值；丢包率当前路径用媒体（窗口内至少 300 个包），否则用检查（至少 8 次）。20 s 内至少 6 次检查且丢失 ≥ 40% 时按检查丢包计（不论媒体）。
  - 代价 = RTT + 25 ms ×（丢包 % − 1），丢包 1% 以内不计（FEC 与重传能补）。例：直连 30 ms 丢 15% 代价 380 ms，中继 60 ms 不丢代价 60 ms。
  - 切到（另一条）中转：代价比当前低 max(30 ms, 25%)；切回直连：直连代价不高于当前中转（直连不占服务器带宽）。几条都满足时取代价最低的。判断连续 3 s 成立才切换；当前路径没有足够媒体时只比延迟。都丢包时由延迟决定。
  - “使用中转”（观看窗口）：只收集中转候选，使用服务器提供的全部中转方式，TLS 排在最前（还没有测量时先用它），之后同样按实测在 UDP 与 TCP / TLS 中转之间选择；服务器没有 TLS 中转时从 UDP / TCP 中转开始。
  - 探测包少，测不出按流量限速的 QoS，所以因媒体丢包离开的路径 60 s 内不再尝试，再次因丢包离开时翻倍，最长 10 min，10 min 内没再离开则从 60 s 重新计。
  - **切回前先压测：** 切到直连（以及重新尝试一条曾因丢包离开的 UDP 中转）之前，主控请对端沿那条路径发 4 s 填充数据（当前码率的一半，1–8 Mbps），按实收计算丢包；会话在此期间留在原路径。压测下的代价仍满足切换条件才切换；不满足时不切，该路径按“因丢包离开”计入冷却（翻倍）。TCP 路径不压测。
  - 切过去以后仍有 30 s 试用期：期间它的代价明显高于原来的路径，就立即退回原路径（不必等原路径的冷却，也不必连续 3 s）。
  - 选择中继后，常规的候选对排序不再把会话拉回直连，只由上面的规则决定；路径完全不通时仍按原有的可写 / 接收规则处理。
  - 日志：每次切换一行 `path switch: cost …`（注明两条路径的类别），每次压测一行 `path load test of …`（目标 `webrtc_ice::agent::path`，默认记录），注明 `(retry)`、`(trial failed)` 与冷却时间；`webrtc_ice::agent::path=debug` 时每秒记录双方的测量值。
  - 只在 Farwing 主控（控制方）生效；网页主控的 ICE 由浏览器决定（被控照常响应 Farwing 主控的压测请求）。被控端 `FARWING_DEV_DIRECT_LOSS=<百分比>[,<Unix 时间（秒）>]` 在直连上丢掉这么多媒体包与压测包（检查包不丢；给出时间时只丢到那一刻），主控端 `FARWING_DEV_PATH_LOSS=<类别>:<百分比>[,…][,until:<Unix 时间>]`（类别 `direct`、`udp-relay`、`stream-relay`）按主控所判的路径类别丢掉收到的媒体与压测包（检查包不丢），用于本机验证三类路径之间的选择（被控端的开关按被控自己对路径的判断，本机上中转地址是局域网地址时会把中转误判为直连）。
- **探测打洞（`vendor/webrtc-ice/src/agent/punch*`，协议见 farwing-protocol §7.5）：** 5 s 后仍无直连时，两端按 `a=x-farwing-nat` 中双方测得的映射（未测得时按 srflx 候选推断）规划线性 / 临时端口 / 随机探测，作为端口预测之外的补充；探测报文带由双方 ICE 凭据派生的标记，只有本会话的对端能让端口应答或提升临时端口；最多 6 轮（约 2–14 分钟）后停止。任一端关闭“激进打洞”时两端都不探测。原先由被控按丢包 / 延迟阈值要求主控切中继的机制（`NetworkPolicy`、`session.setting` `network_switch`）已删除，由上面的“中继切换”取代；原探测协调器中对两个公共 DNS 的 TCP 可达性检查也已删除。
- **三级恢复：**
  1. 会话内 ICE 重启：`Disconnected` 持续 2 s 以上、或收到本机网络变化 / 唤醒事件时触发。
  2. 重建会话：指数退避；`device_offline` 与本机网络错误视为“等待设备”。
  3. 界面保持布局与最后一帧，显示“正在重新连接”，提供“重试”。
- **实现状态（P6）：**
  - 第 1 级：主控在 `Disconnected` 持续 2 s 后，或本机网卡 / 地址 / 默认路由变化稳定 2 s 后，或系统从睡眠唤醒 2 s 后，发起 ICE 重启（同一会话最多 3 次，`Connected` 后清零），沿用当前线路策略。到期时信令连接正断开的，等信令恢复后立即重启；信令恢复时连接仍是 `Disconnected` 的也立即重启。
  - 第 2 级：会话丢失后在 5 分钟内持续恢复。
    - 被控掉线（`PublisherDisconnected`）或创建会话返回 `device_offline` 时，显示“等待设备上线”，每 3 s 查询一次设备状态，上线后立即重建。
    - 其他可恢复错误（实时连接中断、对端失败、`network` / `busy` / `internal`）按 1、2、4、8、15 s 退避重建。
    - 被踢出、权限撤销等不恢复。首次连接失败不等待，直接报告。
  - 第 3 级：会话丢失后，观看窗口在进度页背后保留最后一帧画面（渲染线程在下一次呈现前复制后台缓冲区，缩到不超过 1280 像素宽；HDR 输出按 sRGB 近似；无法截取时退回设备壁纸），显示等待原因与剩余时间，并提供“立即重试”：结束当前退避或“等待设备上线”的轮询，立即重建会话。直连会话同样适用。

## 12. 局域网与直连（P7）

不经服务端也能建立会话：被控内置一个最小信令端点，主控经局域网发现或输入地址直接连接；授权与经服务端时是同一张访问名单（`docs/access-model.md` §2.2），由被控判定：本账号设备（缓存）、好友（缓存的好友授权）、受信任设备、访客（访问码或本机确认）。会话建立后的媒体与数据通道与经服务端的会话完全相同。实现在 `src/transport/direct/`（端点、主控、发现、记录）、`src/transport/signal/router.rs`（与服务端参与者共用被控会话）与 `crates/protocol/src/direct.rs`（线上格式）。

### 12.1 直连端点（被控）

- “允许直连”开启（`network.json` `direct`，默认开启）时，被控在 TCP `direct_port`（默认 28890；被占用时依次尝试 +1..+9，例如同一台机器上的第二个 profile）上监听所有接口（IPv4，以及可用时的 IPv6）。端点随被控的在线任务启动，只在“允许连接本机”时接受会话；“允许直连”、直连端口与局域网发现改变后，端点立即按新设置重建（不必重启程序）。
- 直连会话的 ICE 只用 UDP，端口取自 `port .. port + 15`（16 个，按会话随机选取，每个网卡一个套接字）。内置 ICE 的分配器要求每个网卡独立的套接字，不支持单端口 UDP mux；从公网直连时在路由器上转发 TCP `port` 与 UDP `port..port+15`。直连会话不收集 ICE-TCP 候选，以免在网卡地址上占用端点端口。
- `port_mapping` 开启时，§11 的网关映射照常为会话的 UDP 端口申请映射；端点的 TCP 端口不自动映射。
- 传输为 TLS 1.3（rustls，ALPN `farwing-direct/2`）。被控出示进程启动时生成的自签证书（rcgen），主控不校验证书链；身份认证由下面的握手完成，并绑定 TLS 导出密钥 `ekm = export_keying_material(32, "EXPORTER-farwing-direct-v2")`，中间人无法转接。
- 帧为 `u32` 大端长度前缀 + 与 WSS 相同的 `Envelope` JSON（`v: 2`），单帧最大 1 MiB。会话建立后双方每 15 s 发 `ping`，45 s 无数据断开。
- 未完成握手的连接最多 16 个；握手每一步须在 10 s 内完成（等待被控用户确认时最长 65 s）。

### 12.2 握手与授权

1. 主控 → `direct.hello {device_id, public_key, name, platform, version, proof}`，`proof = ed25519(sk, "FWDIRECT2\ncontroller\n" + hex(ekm))`。
2. 被控 → `direct.hello {…, proof(role = host), udp_port, methods}`。双方都校验对方的 `device_id` 由其 `public_key` 推导（§1.1，不符即断开：“对端出示的密钥与设备 ID 不符”）以及 `proof`。`methods` 为被控此刻接受的方式：`own`（被控登录了账号）、`friend`（登录了账号且好友授权缓存非空）、`trusted`（总是声明）、`assist`（访问码已开启）。被控未开启“允许连接本机”时回 `error{host_disabled}`。
3. 授权（主控选择一种）：
   - 按设备密钥：主控发 `direct.auth {method}`（被控声明 `own` 时为 `own`，否则 `trusted`）。被控不论 `method`，依次检查：
     - `own`：被控缓存的本账号设备列表（`GET /api/v2/devices` 中的 `public_key`；客户端每次取设备列表时写入 profile 的 `own_devices.json`，按账号区分，退出登录时删除）包含主控公钥。关系 `own`，全部能力。
     - `friend`：被控缓存的好友授权（§12.6 的 `friend_devices.json`）包含主控公钥。关系 `friend`，能力为该好友在本机上的授权（总含 `view`）。
     - 主控不在本账号设备缓存中时，已登录且连着服务端的被控先刷新本账号设备与好友授权两份缓存（最多每 10 s 一次，5 s 超时）再判定：服务端可达时好友授权的撤销立即生效，不可达时按缓存。
     - `trusted`：被控 `trusted_devices.json` 中有该公钥（§12.6）。关系 `trusted`，能力为该设备记录的能力（总含 `view`），并更新其最近连接时间。
     - 都不是时回 `error{forbidden}`（“对方没有信任这台设备”），主控提示输入访问码或请对方确认。
   - `assist`：主控 `direct.pake {kind: "assist", messages: [msg_0, msg_1]}`，SPAKE2（身份串 `"farwing-assist-v2"`），两个槽位与 §2 相同（临时 / 固定访问码）。被控回 `direct.pake {messages, confirm: [每槽 HMAC(K_i, "FWPAIR2\nhost\n" + hex(ekm))], confirmation}`；主控核对后发 `direct.confirm {confirm: HMAC(K, "FWPAIR2\ncontroller\n" + hex(ekm))}`。主控发现确认值都不符即为访问码错误，直接断开。仅确认模式时 `messages` 为空、应答中 `confirmation: true`；需要本机确认时由被控界面弹出确认（无界面、会话锁定或已有待确认请求时拒绝），被控用户可选“允许并信任”（§12.6）。关系 `assist`，能力为访客默认权限。错误码：`assist_unavailable`（未开启访问码）、`assist_code_required`、`assist_code_invalid`、`assist_rejected`、`rate_limited`。
   - 失败计数与 §2 共用同一计数（经服务端与直连；每分钟 10 次，超出后 5 分钟拒绝访问码）。发出 PAKE 消息后未在 10 s 内给出正确确认值的尝试计为失败。日志不记录任何访问码。
4. 被控回 `direct.authorized {relation, grants}`。主控把被控记入 `known_hosts.json`（地址与公钥；关系不是 `assist` 时标记为“对方信任本机”，§12.6）。
5. 主控照常发送 `session.start`，并带上新增的可选字段 `purpose`（服务端不读取，仍以会话票据为准）。被控以 `ack SessionStarted` 回复：`session_id`、`participant_id` 由被控生成，`relation: own|friend|trusted|assist`，`grants`，`ice_servers` 为空，`network` 为默认。端点把它作为一条 `session.incoming` 交给被控的信令循环（`router.rs` 按会话 ID 转发两个方向的消息），与经服务端的会话共用同一套会话、参与者与显示布局管理，两类参与者可以同时在一个会话里。`connect_type` 为 `assist`（访客）或 `account`（`own` / `friend` / `trusted`）；被控照常按目的检查所需能力。
6. `rtc.offer / rtc.answer / rtc.candidate` 与经服务端时相同；SDP 照常带 `a=x-farwing-identity`，使用访问码的会话另带 `a=x-farwing-pake`（K 为本次 PAKE 的密钥）。被控结束参与者时（`session.end{reason}`），端点向主控发 `session.ended{reason}`；主控断开 TCP 或发 `session.end` 时，被控收到 `session.ended{controller_left}`；被控停止直连（退出、关闭被控）时主控收到 `session.ended{host_closed}` 或 TCP 断开，按“等待设备”恢复（每 3 s 重连，最长 5 分钟）。

### 12.3 直连会话的 ICE

- 不使用 TURN 与 STUN；被控的直连会话在 12.1 的 UDP 端口上收集主机候选，主控照常收集。
- 主控对被控的每个 UDP 主机候选，另加一个候选：它实际连上的被控地址 + 该候选的端口（类型 srflx，优先级低于主机候选；地址相同或为回环地址时不加），用于经路由器端口转发的公网直连（外部端口与内部端口相同）。
- NAT 属性与激进打洞照常（§11）。

### 12.4 局域网发现

- `network.json` `lan_discovery`（默认开启）时，被控在 UDP `0.0.0.0:28889` 上（`SO_REUSEADDR`，多个 profile 可同时监听）加入 IPv4 组播 `239.255.70.87`，“允许连接本机”时每 30 s 向组播地址发送一次通告，并单播回应查询。发现端口不在直连会话的 UDP 端口范围内。
- 主控查询时向组播地址与 `255.255.255.255:28889` 各发一次查询，1 s 后再发一次，收集 2 s（命令行可调）。
- 查询：`{"t":"farwing.lan.query","v":2,"nonce":"…"}`。
- 通告：`{"t":"farwing.lan.announce","v":2,"device_id","name","platform","version","public_key","port","udp_port","methods":[…],"ts","nonce","sig"}`，`sig = ed25519(sk, "FWLAN2\n" + 规范 JSON（除 sig 外按键排序）)`，单个报文不超过 1200 字节；回应查询时 `nonce` 与查询相同，主控只接受本次查询的应答。`device_id` 不是由 `public_key` 推导的通告被丢弃。签名只证明 ID、名称、端口与公钥属于同一设备；能否按设备密钥连接由主控按 ID 匹配本账号设备缓存、好友共享的设备或 `known_hosts.json` 决定。
- 主控界面在“设备 → 附近”中列出发现的设备，标记“本账号 / 某某 共享 / 对方信任本机 / 需要访问码（或未开启访问码）”：本账号设备与信任本机的设备按设备密钥直连（服务端不可用时也可以）；好友共享的设备在被控声明 `friend` 时直连，否则经服务端连接；其他设备可“请对方确认”或“输入访问码”。

### 12.5 地址直连

- “设备”页的连接栏（登录页“无需登录 · 连接其他设备”也可进入）：输入设备 ID、9 位短号或 `主机名或 IP[:端口]`（IPv6 加方括号，默认端口 28890）；可填访问码或选“请对方确认”，都不填时按设备密钥授权。
  - 地址：直连。
  - 设备 ID：本账号或好友共享的设备（未填访问码时）经账号连接；在附近发现的按其当前地址直连；有服务端时按设备 ID 经服务端（§2）；否则按 `known_hosts.json` 中上次的地址直连。
  - 短号：只能经服务端。
- 主控解析出的各个地址错开并行尝试，先连上的为准。
- 观看窗口与经服务端时相同；画面设置按设备记在不属于任何账号的存储中。会话丢失后按 §11 第 2 级恢复：被控断开或停止时每 3 s 重新直连，其他错误按 1、2、4、8、15 s 退避，最长 5 分钟。按设备密钥授权的会话照常重连；访客会话的重连再次使用同一访问码。

### 12.6 受信任设备与本地记录

受信任设备是被控记住的设备：无需访问码与确认，经服务端（§2）或直连（§12.2）都能连接，按为它记录的能力。

- **记录（被控）：** `trusted_devices.json`：`{device_id, public_key, name, grants, trusted_at, last_seen}`，`device_id` 须由 `public_key` 推导（不符的记录读取时丢弃）。`grants` 总含 `view`。
- **加入：**
  - 被控“本机”页“正在连接”中对访客点“信任”；
  - 回答连接请求（经服务端或直连）时选“允许并信任”（`assist::Action::Answer{trust: true}`）；
  - 命令行 `Farwing trusted add <设备ID> --key <公钥>`（对方用 `Farwing id` 查看），校验 ID 由公钥推导。
  - 能力默认取访客默认权限，可在“本机 → 受信任设备”或 `trusted grant` 修改。
- **生效：** 修改能力在对方下次连接时生效；移除时，由被控进程执行（界面，或后台服务模式下任何方式）会同时断开该设备当前的会话，未安装服务时命令行移除在对方下次连接时生效。
- **单向：** 只允许对方连接本机，不反向授权。
- **主控侧记录：** `known_hosts.json`：`{device_id, public_key, name, address, connect_id, trusted, last_seen}`，记录连接过的设备、上次的直连地址与短号（最多 200 条）；`trusted` 表示对方上次按设备密钥接受了本机（本账号、好友或信任本机），界面据此直接连接，不再要求访问码。
- **好友授权缓存（被控）：** `friend_devices.json`：`GET /api/v2/devices/self/friend-access` 的结果（好友中对本机持有非空授权者的全部设备及其授权），按账号与本机设备 ID 区分，7 天内有效，退出登录时删除。直连开启时，被控进程在每次在线连接建立时刷新它（连同本账号设备缓存），按设备密钥授权前也可能刷新（§12.2）。
- **旧记录：** 不迁移旧版本的配对记录（`paired*.json`）。

### 12.7 无服务器模式

- `Farwing host --serverless`：不登录、不连接服务端，只提供直连端点与局域网发现，接受受信任设备与访客（访问码 / 本机确认）。设备密钥沿用本 profile 的密钥，设备 ID 与有服务端时相同（§1.1）。这种被控使用与“无需登录 · 让别人连接本机”相同的本机设置存储。
- 设置“仅直连（不连接服务器）”：已登录或游客的被控也不再连接服务端（不发布设备策略、不取短号），只接受直连。
- 已登录的被控在服务端不可达时继续接受直连（本账号设备与好友按缓存授权），并在后台继续重连服务端；服务端连接中断不会结束直连参与者。
- 主控未登录时也可使用局域网发现、地址直连与访问码；有服务端时也可按短号或设备 ID 经服务端连接（游客令牌，§2）。

### 12.8 命令行

```
Farwing connect --direct <地址> [--code <访问码> | --ask]   # 都不给时按设备密钥授权
Farwing assist <短号或设备ID> [--code <访问码>]              # 经服务端连接，无需登录
Farwing id                                                  # 显示本机设备 ID 与设备公钥
Farwing lan [--seconds N]                                   # 发现局域网设备（默认 2 s）
Farwing trusted list                                        # 本机信任的设备，以及信任本机的设备
Farwing trusted add <设备ID> --key <公钥> [--name <名称>] [--grants <能力,…>]   # 默认访客权限
Farwing trusted remove <设备ID>
Farwing trusted grant <设备ID> <能力,…>
Farwing host [--serverless] [--allow] [--access-code <访问码>]
```

### 12.9 实现状态

- **已实现：** 12.1–12.8 全部内容；服务端设备列表的 `public_key`、`Relation::Trusted`、`GET /api/v2/devices/self/friend-access`；被控会话在服务端重连之间保留（直连参与者不受影响）。
- **自动测试：** 服务端测试覆盖无账号主控按设备 ID 经服务端以 `trusted` 连接（`server/tests/assist.rs`）、好友授权列表（`server/tests/friends.rs`）与拒绝其他结构版本的数据库（`server/tests/api.rs`）；客户端单元测试覆盖受信任设备按推导 ID 存取（`src/transport/direct/store.rs`）与服务模式动作的校验（`managed.rs`）。
- **本机已验证（同一台机器的多个 profile，回环与局域网地址；在统一访问模型之前完成，按设备密钥授权的路径当时称“配对”）：**
  - 经局域网地址 `192.168.1.217` 按设备密钥连接并出画面；直连会话只占用 `28890–28905` 的 UDP 端口，没有 ICE-TCP 监听。
  - 访问码：错误访问码被拒绝；正确访问码以 `assist` 关系连接（访客默认权限）并出画面；10 次失败后被以 `rate_limited` 拒绝。
  - 本账号设备（g → h）经 `connect --direct` 按设备缓存授权（`relation=Own`，全部能力）并出画面。
  - 同一会话：f 经直连与 g 经服务端同时观看被控 h（`participants=2`）。
  - 主控进程被强制结束后，被控收到 `controller_left` 并移除参与者。
  - 被控进程被强制结束并在约 8 s 后重新启动：直连观看窗口每 3 s 重试，被控恢复后按设备密钥自动重连并出画面（`direct session recovered elapsed_ms=11009`）。
  - 本机 Windows 防火墙只有“域”配置文件开启（专用 / 公用关闭）且没有 Farwing 规则，因此本机的局域网地址测试没有经过防火墙过滤。
- **后台服务模式（§12.10）：** 已实现；本机未安装服务，只验证了未安装服务时的同一套代码路径、请求校验与管道消息的序列化（单元测试），服务进程中的实际读写与界面联动需要安装服务后测试。
- **本机已验证（统一访问模型，CLI，2026-10-16）：** 测试服务器上的 schema 2 → 3 迁移（14 台设备，登录与好友授权保持有效；该迁移已于 2026-10-19 删除）；无服务器直连时未信任设备被拒、受信任设备按密钥接纳；直连访客的正确 / 错误访问码；无账号主控经服务端按短号（访问码）与按设备 ID（受信任，无需访问码）连接；`friend` 方法直连，以及撤销好友授权后立即被拒。结果见 `docs/access-model.md` §7.3。
- **需要在界面上验证：** “信任”与“允许并信任”。
- **需要双机测试：** 两台物理机器之间的局域网发现与直连；经路由器端口转发的公网直连（TCP + UDP 端口段）与派生候选；Windows 防火墙首次监听时的提示与放行；安装后台服务后的直连端点、受信任设备与开关（§12.10）；IPv6 直连；被控重启 / 断网后的直连恢复；“本机确认”模式下界面确认直连访客；“设置 → 网络与直连”中的直连开关（端口、局域网发现、仅直连）；界面中移除受信任设备时断开该设备当前会话。

### 12.10 后台服务模式

安装后台服务后（只用于默认 profile），被控运行在服务的常驻进程里（SYSTEM），界面与命令行是安装服务的 Windows 用户的普通进程。直连的记录按“谁使用”存放：

| 记录 | 未安装服务 / 其他 profile | 已安装服务 |
| --- | --- | --- |
| `trusted_devices.json`（受信任设备） | 本 profile 数据目录 | 服务的数据目录（SYSTEM 的 `%LOCALAPPDATA%\Farwing`，只有 SYSTEM 与管理员可写） |
| `network.json`（直连与 NAT 开关） | 同上 | 服务的数据目录为被控所用；用户数据目录保留一份相同内容，供本机作为主控的会话使用 |
| `own_devices.json`（本账号设备缓存） | 本进程取设备列表时写入 | 两份：服务进程每次连上服务端时刷新自己的一份（被控按它授权 `own`，离线时也可用），界面进程的一份用于识别局域网中的本账号设备 |
| `friend_devices.json`（好友授权缓存） | 被控进程写入 | 服务的数据目录（服务进程按它授权 `friend`） |
| `known_hosts.json`（连接过的设备） | 本 profile 数据目录 | 用户数据目录（主控会话在用户进程中运行） |

- 用户不能直接写服务的记录：界面和命令行经常驻进程管道（只接受安装用户与 SYSTEM）发送 `DirectState`（读取）与 `Direct{account, action}`（修改），`action` 为信任设备（`Trust`，ID 须由公钥推导）、修改能力（`SetGrants`）、移除（`Remove`）、设置网络开关（`Network`）之一；常驻进程核对账号代次并校验能力名、设备 ID 与端口范围后才写入。移除受信任设备时常驻进程同时断开该设备当前的会话；切换“仅直连”时常驻进程重建被控的在线任务。服务模式下“允许并信任”由常驻进程自己写入。
- 常驻进程快照（界面每 500 ms 读取）带上被控的直连状态（网络开关、受信任设备、端点端口），界面按它显示；界面进程把网络开关同步到自己的 `network.json`。
- 命令行 `trusted list | add | remove | grant` 在安装服务后同样经常驻进程操作服务的记录（`trusted list` 中“本机信任的设备”取自服务，“信任本机的设备”取自用户的 `known_hosts.json`）；`trusted remove` 同时删除用户一侧该设备的 `known_hosts.json` 记录。
