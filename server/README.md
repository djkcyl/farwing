# Farwing Server

Farwing Server 是 Farwing（自托管远程桌面）的服务端：账号、设备身份与设备管理、好友与授权、HTTPS 控制面（`/api/v2/...`）、WSS 实时面（`/api/v2/ws`，设备在线、会话协调、SDP/ICE 中转、推送）、远程协助（PAKE 中转）、电源与网络唤醒、TURN 临时凭据、可选的内置 STUN/TURN，以及同一端口上的**网页端**（用户中心、管理后台、浏览器里的远程桌面，见下文“网页端”）。服务端实现 **Farwing Protocol v2**：规范见 `docs/protocol/farwing-protocol.md` 与 `docs/protocol/peer-messages.md`（设计与决策记录见 `docs/protocol/v2-design.md`），线上类型来自 `crates/protocol`（crate `farwing-protocol`，服务端启用 `crypto` 特性）。不提供与旧版本（包括 v1）的兼容，客户端与服务端须使用同一版本（见下文“升级”）。

服务端只做信令与凭据，不接触、不转码音视频；媒体经 WebRTC（DTLS-SRTP）端到端加密，TURN 只转发密文。远程协助的验证码也不经过服务端：双方通过服务端中转 SPAKE2 消息，服务端看不到协助码。

> **未验证事项**：本服务端在 Windows 上开发和测试（`cargo test` 全部通过，测试在进程内启动服务并通过真实 HTTPS/WSS 与 TURN 连接验证）。开发环境中**无法运行 Linux、Docker 和 coturn**，因此以下内容**均未实际验证**：在 Linux 上构建与运行、`Dockerfile`、`docker-compose.yml`、`deploy/farwing-server.service`（systemd）、coturn 配置及其与本服务的互通、真实进程中的 SIGTERM/Ctrl-C 信号处理（优雅退出逻辑本身由测试覆盖）。部署前请自行验证。自动证书（ACME）用 Let's Encrypt 的测试 CA Pebble 在本机验证过 TLS-ALPN-01、HTTP-01、重启复用与失败重试，没有对真实的 Let's Encrypt 与公网域名验证。网页端只在本机 Chrome 中验证过（直连、经内置 TURN 中继、经带路径前缀的反向代理观看本机被控，访问码与“请对方确认”的访客观看；网页控制的键鼠注入、锁定键同步、剪贴板、文件上传下载与权限上限）；Firefox / Safari、手机实机触屏、指针锁定与全屏键盘锁定的真实操作、装有虚拟声卡时的麦克风、跨公网与真实证书、`build-web.sh` 与 Dockerfile 中的前端构建阶段未实际验证。

## 构建

需要 Rust 1.88 或更新版本（开发时使用 1.95）。服务端是独立的 Cargo 工作区，有自己的 `Cargo.lock` 和 `target/`：

```sh
cd server
cargo build --release
# 产物：server/target/release/farwing-server
cargo test          # 集成测试：进程内启动服务，HTTPS/WSS/TURN 全流程
```

网页端是 `server/web/` 中的 React + TypeScript 工程（需要 Node.js 20 或更新版本），`npm run build` 输出到 `server/web/dist/`，编译服务端时由 `build.rs` 嵌入二进制，部署仍只有一个文件。`cargo build` 不调用 npm：没有 `dist/` 时服务端照常构建，网页上显示“网页端没有构建”，客户端接口不受影响。一次完成两步：

```sh
pwsh server/build-web.ps1     # Windows
sh server/build-web.sh        # Linux / macOS
```

脚本依次执行 `npm ci`、`npm run check`（类型检查、oxlint、Prettier、Vitest 单元测试）、`npm run build`（同时生成 Brotli 与 gzip 压缩副本，服务端按浏览器的 `Accept-Encoding` 返回；约 430 KiB 的前端传输约 130 KiB）与 `cargo build --release --locked`。前端依赖全部装在 `server/web/node_modules/`，页面不引用任何 CDN 或外部资源。

SQLite 使用 `rusqlite` 的 `bundled` 特性静态编译，不依赖系统 SQLite；TLS 使用 rustls（ring），不依赖 OpenSSL。构建需要 C 编译器（SQLite、ring）。

Docker 镜像必须**在仓库根目录**构建（构建上下文需要 `crates/protocol`）：

```sh
docker build -f server/Dockerfile -t farwing-server .
```

## 快速开始

```sh
# 1. 生成带注释的配置文件（同时生成随机的 TURN 共享密钥）
farwing-server init --config /etc/farwing-server/farwing-server.toml \
    --public-url https://farwing.example.com:8443 \
    --data-dir /var/lib/farwing-server

# 2. 按需编辑 [tls] 与 [turn]（见下文）

# 3. 创建第一个管理员（之后可在网页端创建账号或生成邀请码）
farwing-server --config /etc/farwing-server/farwing-server.toml user create admin --admin
farwing-server --config /etc/farwing-server/farwing-server.toml user create alice --nickname 爱丽丝

# 4. 运行
farwing-server serve --config /etc/farwing-server/farwing-server.toml
```

`--config` 也可以用环境变量 `FARWING_CONFIG` 指定，默认值为当前目录下的 `farwing-server.toml`。配置文件中的相对路径（`data_dir`、证书路径）相对于配置文件所在目录解析。

运行后用浏览器打开 `public_url`（如 `https://farwing.example.com:8443/`）即为网页端，用管理员账号登录。

## 命令行

| 命令 | 说明 |
|------|------|
| `init --config <path> [--public-url URL] [--data-dir DIR] [--force]` | 写入带注释的示例配置，TURN 共享密钥随机生成；文件已存在时需 `--force` |
| `serve --config <path>` | 运行服务；Ctrl-C / SIGTERM 时优雅退出（通知会话 `server_shutdown`，关闭连接） |
| `user create <username> [--nickname N] [--password-stdin] [--admin]` | 创建账号；不带 `--password-stdin` 时交互输入两次密码；密码至少 8 个字符；`--admin` 设为网页端管理员 |
| `user list` | 列出账号（含角色） |
| `user promote <username>` / `user demote <username>` | 设为 / 取消网页端管理员；最后一个管理员不能取消 |
| `user reset-totp <username>` | 关闭账号的二次验证（丢失身份验证器时） |
| `user passwd <username> [--password-stdin]` | 修改密码，并吊销该账号所有登录 |
| `user disable <username>` / `user enable <username>` | 禁用（吊销所有登录）/ 启用账号 |
| `user delete <username>` | 删除账号：解绑其设备、吊销登录、删除协助收藏/最近记录、好友关系、好友申请与授权 |
| `device list [--user <username>]` | 列出设备（平台、版本、协助号、主人允许的策略 `policy` 与参与者上限、最近在线） |
| `device remove <device_id>` | 把设备从账号解绑并吊销其登录（同时删除针对该设备的授权） |
| `session list` | 列出活动会话（关系 `own`/`friend`/`assist`、目的、状态、被控、主控、账号） |
| `friend list <username>` | 列出账号的好友，以及该账号给每位好友的授权（默认授权与按设备的覆盖） |

管理命令直接操作配置中 `data_dir/farwing.db`（SQLite WAL 模式），可以在服务运行时执行。吊销类操作会写入事件表，运行中的服务每秒读取一次，并以关闭码 `4003` 断开受影响的实时连接、结束相关会话。请以服务运行的同一系统用户执行管理命令（数据库文件权限）。

`--password-stdin` 从标准输入读取第一行作为密码，例如：`printf '%s\n' "$PW" | farwing-server user create bob --password-stdin`。

## 配置

完整示例见 `farwing-server.example.toml`（与 `init` 生成的内容相同，仅密钥为占位符）。

| 键 | 说明 |
|----|------|
| `listen` | HTTPS/WSS 监听地址，如 `0.0.0.0:8443` |
| `public_url` | 客户端访问的 `https://` 地址；实时端点由它生成：`wss://<host>[:port][/path]/api/v2/ws`。反向代理模式下也必须是 https。带路径前缀（如 `https://example.com/farwing`）且代理剥掉前缀时，对带前缀路径的请求签名同样被接受 |
| `data_dir` | 数据目录：`farwing.db`、`wallpapers/`、`tls/` |
| `instance_name` | `GET /api/v2/server` 与网页端显示的实例名（可在网页端修改，见下） |
| `trusted_proxies` | 受信任的反向代理 CIDR 列表；仅当直连对端在列表内时才采用 `X-Forwarded-For`（取最右侧非受信地址） |
| `bitrate_limit_mbps` | 可选，下发给客户端的会话码率上限（可在网页端修改） |
| `log` | 日志过滤（`tracing` 语法，如 `info`、`farwing_server=debug`）；环境变量 `RUST_LOG` 优先 |
| `[tls]` | `mode = "tls"`（默认）或 `"proxy"`；`cert`/`key`（PEM）；或 `self_signed = true` 与 `hostnames = [...]` |
| `[guest] enabled` | 是否允许游客（未登录）设备作为协助被控与网页访客观看，默认 `true`（可在网页端修改）。设备登记本身不受此项限制（新设备只能登录，见下文） |
| `[turn]` | `mode = "external" | "builtin" | "none"`、`urls`、`shared_secret`、`credential_ttl`（默认 `"12h"`，可写秒数或 `30m`/`12h`/`1d`）、`force_relay`；内置模式另有 `listen_udp`、`listen_tcp`、`public_ip`、`realm`、`relay_bind`、`relay_ip`、`relay_port_min`、`relay_port_max`，以及用于 NAT 类型探测的额外 STUN 端点 `[[turn.stun]]`（见下文） |
| `[network]` | 会话自动切换中继的阈值（`auto_switch`、`possible_loss`、`possible_latency`、`possible_min_latency`、`force_loss`、`force_latency`），默认值与 `farwing-protocol` 一致；取值不一致时启动日志会告警（被控会回退为 30% / 600 ms） |
| `[web]` | 网页端：`enabled`（默认 `true`）、`admin_allow`（管理后台的 IP 白名单 CIDR，默认空 = 不限）、`session_idle_minutes`（网页登录空闲过期，默认 30）、`session_max_hours`（网页登录最长有效期，默认 12）、`audit_days`（审计日志保留天数，默认 180） |
| `[limits]` | 可选覆盖：`heartbeat_ms`、`idle_timeout_ms`、`resume_ttl_s`、`ticket_ttl_s`、`max_frame_bytes`、`max_sdp_bytes`、`max_candidates`、`max_candidate_bytes`、`ws_rate_per_s`、`ws_rate_burst`、`device_init_per_ip_hour`（新密钥登记）、`login_failures_per_ip`、`login_failures_per_user`（均为 15 分钟窗口）、`assist_failures_per_minute`（每账号每协助号未建立会话的尝试数）、`assist_host_reply_ms`（等待被控应答 PAKE）、`assist_decision_wait_ms`（`accept` 长轮询）、`guest_token_ttl_s` |

配置中出现未知的段或键时启动报错（包括已删除的 `[input]` 与 `[[updates]]` 段）。版本公告不在配置文件中：管理员在网页端“客户端更新”（或管理接口）中发布与编辑，保存在数据库；`beta` 频道同时包含 stable 条目，取 `published_at` 最新者。

**在线设置的优先级**：实例名称、游客功能、邀请码注册（默认关闭）、码率上限与网页端可以使用的功能可以在网页端“服务器设置”中修改，立即生效。修改过的项保存在数据库（`settings` 表），覆盖配置文件中的值；没有在网页上改过的项仍取配置文件。其余配置（监听、TLS、TURN、网络策略、限流、`[web]`）只读显示，修改需编辑配置文件并重启。

## 网页端

服务端在同一端口上提供网页端（`/` 为页面，`/api/v2/...` 为接口），设计见 `docs/web-console.md`：

- **用户中心**（所有账号）：概览、设备（本账号与好友共享的设备，带壁纸缩略图与在线状态）、设备详情（改名、移除）、好友（申请、按设备的权限，与客户端相同）、收藏的短号、账号与安全（昵称、密码、我的登录、管理员的二次验证）、连接到本服务器（地址、证书指纹、客户端下载）。
- **管理后台**（管理员）：服务器概览（每 5 秒刷新）、账号（创建、重置密码、禁用、删除、设为管理员、查看设备 / 登录 / 好友与授权）、邀请码、全部设备（解绑、断开）、会话（每 3 秒刷新，可结束）、客户端更新、服务器设置、审计日志。
- **网页远程桌面**：在浏览器中连接本账号或好友共享的在线设备，或在 `/visit` 用短号 / 设备 ID 加访问码（或请对方确认）以访客身份连接，无需账号。观看：适应 / 填满 / 1:1 缩放、全屏、画中画、切换显示器、AV1 / H.265 / H.264、声音与音量、其他参与者的指针、截图、连接统计、参与者列表、被控事件提示与 5 分钟内自动重连。按授权控制（与客户端相同）：键鼠（全屏时锁定键盘、特殊键菜单、相对鼠标、触屏手势）、文本剪贴板、文件上传与下载、麦克风。

账号与安全：

- 第一个管理员用命令行创建（`user create <名> --admin` 或 `user promote <名>`）。网页上不能取消或禁用最后一个管理员，也不能降级、禁用或删除自己。
- 浏览器在本地生成 Ed25519 设备密钥（WebCrypto 不可导出，存 IndexedDB），作为平台为 `web` 的设备登录，所有请求与客户端一样签名；登录令牌离开这个浏览器的密钥没有用处。网页登录空闲 `session_idle_minutes`、最长 `session_max_hours` 后过期，出现在“我的登录”中，可单独注销。
- 网页主控的权限与客户端相同，再受“服务器设置 → 网页端可以使用的功能”限制（默认：观看、键鼠、剪贴板、文件、声音、麦克风、批注；只勾观看与声音即只看不控）。服务端在浏览器主控的每个会话信息中都把这个上限发给被控，被控对账号会话、访客与受信任设备都按它裁剪。浏览器不会被好友看到或连接。
- 管理员可开启 TOTP 二次验证（RFC 6238，扫码，10 个一次性恢复码）；开启后网页登录需要第二步，管理接口只对完成验证的登录开放。客户端登录不受影响。丢失手机时由其他管理员在后台重置，或用 `user reset-totp`。
- 邀请码：管理员生成（次数、有效期、备注、是否自动加为好友），只保存哈希，可作废、可查看使用记录；需要在“服务器设置”中开启“邀请码注册”（默认关闭）。注册页为 `/register?code=…`。
- 管理操作、网页与客户端登录（含失败）、邀请码注册写入审计日志，保留 `audit_days` 天。
- 响应头：严格的 CSP（只允许本站脚本，`media-src`/`img-src` 只额外允许 `blob:`/`data:`）、`X-Frame-Options: DENY`、`nosniff`、`no-referrer`；权限策略只允许本站使用麦克风与剪贴板。

部署要点：

- 浏览器只信任它认可的证书：自签证书（`self_signed = true`）需要用户在浏览器中手动接受，正式部署请使用受信任的证书或反向代理。客户端固定证书指纹的方式对浏览器不适用。
- 反向代理按“TLS 选项”配置即可；带路径前缀（`public_url = "https://example.com/farwing"`）时，服务端把前缀写入页面的 `<base href>`，页面、接口与实时连接都在前缀下工作。代理须支持 WebSocket 升级，浏览器的实时连接把凭据放在子协议 `farwing.v2, ticket.<入场券>` 中（不能设置请求头）。
- 浏览器要求：Chrome / Edge 120+、Firefox 128+、Safari 17+（需要 WebRTC、数据通道与 H.264 解码；浏览器能接收 AV1 或 H.265 时自动使用，也可在观看页“画质”菜单中指定）。全屏键盘锁定与大文件边收边写只有 Chromium 支持；麦克风与剪贴板需要 HTTPS。
- 网页控制需要服务端与被控都是本版本（被控要能识别服务端发来的权限上限）。
- 不需要网页端时设置 `[web] enabled = false`：页面返回 404，接口不受影响。

## 协议 v2 要点

### 设备身份与请求签名

- 每个安装生成 Ed25519 设备密钥。`POST /api/v2/devices/init` 携带 `public_key` 与持有证明 `proof`（签名 `FWINIT2\n<public_key>\n<timestamp>`），时间偏差超过 ±300 秒、证明无效或重放均返回 `401 device_unauthorized`。同一公钥即同一设备（更新名称、平台、系统、版本、能力 `capabilities`、主人策略 `policy`、可选硬件信息）；新公钥即新设备（按来源 IP 限速）。设备令牌（`device_token`）已取消。
- 新登记的设备不属于任何账号，只能登录（或在开启游客时申请游客令牌）；登录把设备绑定到账号。
- 除 `devices/init`、`server`、`updates`、`healthz` 外，每个请求都带 `X-Farwing-Device-Id`、`X-Farwing-Timestamp`（Unix 毫秒）与 `X-Farwing-Signature`：设备密钥对 `FWREQ2\n<METHOD>\n<path_and_query>\n<timestamp>\n<hex(sha256(body))>` 的签名。服务端按收到的原始请求体计算摘要，允许 ±300 秒时钟偏差，600 秒内拒绝重复的签名（重试必须重新签名；同一设备在同一毫秒内发出两个完全相同的请求会被当作重放）。任何失败都返回 `401 device_unauthorized`。
- 账号请求另带 `Authorization: Bearer <access_token>`。令牌绑定登录时签名的设备：其他设备签名的请求返回 `401 device_mismatch`，单独窃取令牌无法使用。令牌 30 天未使用即失效（每次使用滑动续期，`last_used_at` 至多每分钟写一次），失效、吊销或账号被禁用时返回 `401 session_revoked`。设备再次登录会替换该设备以前的所有登录。
- `GET /api/v2/auth/sessions` 列出本账号各设备的登录（每设备一条，`current` 标出发起请求的设备）；`DELETE /api/v2/auth/sessions/{device_id}` 吊销该设备的登录，效果同该设备注销：令牌吊销、设备解绑、实时连接以 `4003` 断开、其他设备收到 `device.changed{removed}`。
- 游客被控仍用设备签名 + 游客令牌（`fwg_`）。实时面入场券（`POST /presence/ticket`，空请求体；`POST /sessions`）经签名的 HTTPS 获取，WSS 仍凭入场券或恢复令牌接入。

### 设备、好友与授权

- `GET /api/v2/devices` 返回 `{current_device, own, shared}`：`own` 为本账号其他设备（`granted` 为全部能力），`shared` 为好友的、本账号至少持有一项授权的设备（带 `owner` 与 `granted`）。好友只看得到自己在其设备上的参与者。`GET /devices/{id}/detail` 返回结构化详情；好友设备带 `owner`，不含协助号与硬件信息。改名与删除只限自己的设备（好友设备返回 `403`）。
- `PUT /api/v2/devices/self/policy`（游客也可）设置主人策略 `DevicePolicy{control, files, ports, power, wol, assist, max_participants}`，返回规范化后的策略；取代 v1 的 `self/controllable` 与 `self/wol/enabled`。
- 好友：`GET /friends`、`POST /friends/requests {username}`（对方已向我申请时直接成为好友，返回 `accepted`；否则 `sent`；未知用户 `404 user_not_found`，已是好友 `409 already_friends`，自己 `400`；每账号每小时 20 次）、`GET /friends/requests`、`POST /friends/requests/{id}/accept`（返回 `Friend`）· `/reject`、`DELETE /friends/requests/{id}`（撤回自己发出的）、`DELETE /friends/{user_id}`（删除双向授权并以 `permission_revoked` 结束相关会话）。成为好友时双方各自自动授予对方“所有设备 = `view`, `annotation`”。
- 授权：`GET`/`PUT /api/v2/grants/{user_id}`，`{default, devices: {device_id: caps}}`，只对好友有效（否则 `404`），设备键必须是自己的设备，能力名按 `caps::normalize` 规范化。某好友在某设备上的能力 = 该设备的条目，否则默认条目。
- 推送：`device.changed` 同时发给主人的其他设备（`shared: false`）与持有授权的好友（`shared: true`，`granted` 按该好友计算），覆盖上下线、改名、策略、删除，以及授权导致的可见性变化；`friend.changed` 覆盖申请收到/接受/拒绝/撤回、解除好友与授权变化（`grants_changed`）。授权变化时，该好友在相关设备上的活动会话收到 `session.permissions{grants}`（被控与主控都收到）；不再具备会话目的所需能力（desktop/audio 需 `view`，files 需 `files`，ports 需 `ports`）时会话以 `permission_revoked` 结束。

### 会话与多人参与

- `POST /api/v2/sessions {target_device_id, purpose}`：自己的设备，或具备 `purpose` 所需能力的好友设备（能看到设备但缺能力返回 `403 forbidden`）；被控须在线、具备 `host` 能力，且主人策略允许（desktop/audio 需 `policy.control`，files 需 `policy.files`，ports 需 `policy.ports`），否则 `409 host_disabled`。`force` 已删除。
- 一台被控可同时有多个会话，上限为 `policy.max_participants`（限制在 1–16）。同一主控设备对同一被控只保留一个会话，新会话以 `replaced` 替换自己的旧会话；超出上限返回 `409 busy`，从不顶替他人。协助会话遵循同样规则。
- `POST /sessions` 带 `"presence": true` 时不签发入场券（`ticket` 为空）：主控在本设备、本账号的在线连接上发送带 `sid` 的 `session.start`，此后该会话的信令走这条连接，离开时发送 `session.leave{sid}`；原以入场券建立的主控连接以 `4001` 关闭。协助会话总是签发入场券。
- `session.start` 只携带 `attempt_id`、`options`、`capability`（以及只供直连端点使用、服务端忽略的可选 `purpose`）；目的、类型、关系、授予能力与主控身份全部来自服务端。`session.incoming` 带 `relation`、`grants`（自己的设备为全部能力，好友为生效授权，协助为空、由被控套用本机模板）与 `controller{…, public_key}`；`session.start` 的 `ack` 带 `host{device_id, public_key}`、`relation`、`grants`；`SessionTicket.target` 带 `relation`、`granted`、`public_key`。
- 被控 `session.end` 可说明原因 `revoked`、`kicked`、`identity_mismatch`、`media_failed`，其他原因按 `host_closed` 转告主控。

### 远程协助（PAKE）

1. 主控 `POST /api/v2/assist/join {connect_id, pake}`（`pake` 为由输入的协助码生成的 SPAKE2 消息，每个槽一条；确认模式可为空）。服务端找到在线且 `policy.assist` 为真的被控（否则 `409 assist_unavailable`，不打扰被控），推送 `assist.join{request_id, controller, pake}`，等待被控 `POST /assist/requests/{id}/pake`。被控不允许时 `409 assist_unavailable`；需要密码而请求未带 `pake` 时 `409 assist_code_required`（`details.mode`），并推送 `assist.cancelled`；否则返回 `AssistPending{request_id, mode, pake, key_confirm, host}`。
2. 主控在本地核对被控的密钥确认；都不匹配即验证码错误，主控不再继续（可调用 `cancel`）。匹配则 `POST /assist/requests/{id}/accept {key_confirm}`（长轮询）：服务端推送 `assist.accept`，等待被控 `POST /assist/requests/{id}/decision`。允许则建立协助会话并返回 `AssistJoin{…会话入场券, host}`；拒绝时 `code_invalid`→`403 assist_code_invalid`，`code_required`→`409 assist_code_required`，`rejected`→`403 assist_rejected`，`unavailable`→`409 assist_unavailable`；超时 `408 timeout` 并推送 `assist.cancelled`。
3. `POST /assist/requests/{id}/cancel`（仅发起该请求的主控）撤销请求并推送 `assist.cancelled`。被控已应答但主控 120 秒内未 `accept` 的请求过期后也推送 `assist.cancelled`。
4. 限速：每次 `join` 预先计数（每账号 + 协助号每分钟 `assist_failures_per_minute` 次，每协助号跨账号每小时 50 次），只有最终建立会话才返还；在请求到达被控前失败（协助号无效、不在线、未开启协助）不计数。服务端无法区分验证码错误与放弃的请求，二者都计为失败。
5. v1 的 `assist/mode`、`assist/join/confirm`、`assist/cancel` 与 `assist.mode_query`/`assist.verify`/`assist.confirm` 推送已删除；`POST /assist/identity` 与收藏/最近记录接口不变。

### 电源与网络唤醒

- `POST /api/v2/devices/{id}/power`：自己的设备总是允许，好友需要 `power` 授权。关机/重启要求被控在线、具备 `power` 能力且 `policy.power` 为真（否则 `409 host_disabled`）；推送 `power.request{request_id, action, requester{user_id, username, nickname}, requester_device}`。结果 `POST /api/v2/power/{request_id}/result` 只能由该请求的目标设备提交（否则 `403`）。
- 唤醒要求 `policy.wol` 为真且已登记 WoL 信息（`PUT /devices/self/wol`）。请求设备与目标的公网地址相同、且它自己登记的内网地址与目标在同一子网（按目标子网掩码比较 `inner_ip & mask`）时，回执 `current_device_in_same_network = true` 并附 `target{mac, inner_ip, subnet_mask}`，由请求设备自己发送魔术包；另外仍向目标主人的、与目标同一公网地址的在线设备推送 `wol.wake`（`assist_count`）。`router_wol` 已删除。

### 已删除的 v1 接口

`/api/v1/*`、`GET /devices/groups`、`PUT /devices/self/controllable`、`PUT /devices/self/wol/enabled`、`POST /assist/mode`、`POST /assist/join/confirm`、`POST /assist/cancel`、`/assist/requests/{id}/mode|sign|confirmation`、设备令牌头 `X-Farwing-Device-Token`、`session.start` 中的 `connect_type`/`extensions`（`purpose` 不再作为授权依据，服务端忽略）、`POST /sessions` 的 `force`、`ClientConfig.input`。

## TLS 选项

1. **自签名（默认）**：`self_signed = true`。首次启动时在 `data_dir/tls/` 生成 `cert.pem`/`key.pem`（主机名取 `hostnames` 加上 `public_url` 的主机），之后复用。启动日志会打印证书 SHA-256 指纹（`sha256_fingerprint=AA:BB:...`），在客户端“信任证书”中固定该指纹。想重新生成证书时删除 `data_dir/tls/` 后重启（客户端需要重新固定指纹）。
2. **证书文件**：`self_signed = false`，`cert = "/etc/letsencrypt/live/<域名>/fullchain.pem"`、`key = ".../privkey.pem"`。证书只在启动时加载，续期后需重启服务。
3. **自动证书（ACME）**：`mode = "acme"`，服务自动向 Let's Encrypt（或其他 ACME CA）申请证书、到期前续期，并在运行中更换，不需要重启。浏览器与客户端都直接信任这张证书（客户端不再需要固定指纹；已固定的旧指纹不影响连接）。配置在 `[tls.acme]`：

   ```toml
   [tls]
   mode = "acme"

   [tls.acme]
   agree_tos = true                  # 必填：同意 CA 的服务条款
   contact = "admin@example.com"     # 可选：到期与策略通知
   # domains = ["farwing.example.com"]  # 默认取 public_url 的主机名
   # directory = "letsencrypt"       # 或 "letsencrypt-staging"，或其他 CA 的 directory URL
   # challenge = "auto"              # "tls-alpn-01" / "http-01"
   # http_listen = "0.0.0.0:80"      # http-01 验证，并把 http:// 跳转到 public_url
   # ca_root = "/path/to/root.pem"   # 私有 CA 或测试 CA（如 Pebble）的根证书
   ```

   CA 必须能通过域名访问到本服务以验证域名：`tls-alpn-01` 由 TLS 监听本身应答，需要外网 **443** 端口到达 `listen`（`public_url` 用 443 时 `auto` 选它）；`http-01` 由 `http_listen` 应答，需要外网 **80** 端口到达它（`public_url` 不在 443 时，`auto` 在设置了 `http_listen` 时选它）。两者都不可行时启动会报错并说明原因。账号与证书保存在 `data_dir/acme/`（账号按 CA 区分，`key.pem` 仅本用户可读）；证书在有效期过去三分之二时续期，失败后从 5 分钟到 6 小时退避重试，错误显示在日志与网页端“服务器概览”。首次申请成功前使用临时自签证书。通配符证书（需要 DNS-01）暂不支持。
4. **反向代理**：`mode = "proxy"`，服务在 `listen` 上提供明文 HTTP，由 nginx/Caddy 等终止 TLS。`public_url` 仍写对外的 `https://` 地址，并把代理地址加入 `trusted_proxies`。代理必须支持 WebSocket 升级，透传 `X-Farwing-*` 与 `Authorization` 请求头（默认会透传），且**不得改写请求体、方法、路径或查询串**（请求签名覆盖它们；剥掉 `public_url` 中的路径前缀是允许的）。nginx 示例：

```nginx
location / {
    proxy_pass http://127.0.0.1:8443;
    proxy_http_version 1.1;
    proxy_set_header Upgrade $http_upgrade;
    proxy_set_header Connection "upgrade";
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
    proxy_read_timeout 120s;
    client_max_body_size 5m;
}
```

只支持 TLS 1.2 与 1.3（rustls）。ALPN 协商 `http/1.1`；ACME 模式另外接受 `acme-tls/1`（只用于 TLS-ALPN-01 验证连接）。

## TURN 选项

客户端拿到的 TURN 凭据为 TURN REST 临时凭据：`username = "<过期 Unix 秒>:<session_id>"`，`credential = base64(HMAC-SHA1(shared_secret, username))`，与 coturn 的 `use-auth-secret` 兼容。凭据随 `session.start` 的 `ack` 与 `session.incoming` 分别下发给主控与被控。

- **`mode = "external"`（推荐）**：使用 coturn。coturn 配置要点：

  ```
  listening-port=3478
  fingerprint
  use-auth-secret
  static-auth-secret=<与 farwing-server.toml 中 shared_secret 相同>
  realm=farwing
  external-ip=<公网 IP>
  min-port=49160
  max-port=49200
  no-cli
  ```

  `urls` 写客户端可达的地址，例如 `stun:turn.example.com:3478`、`turn:turn.example.com:3478?transport=udp`、`turn:turn.example.com:3478?transport=tcp`（如配置了 coturn TLS，可加 `turns:turn.example.com:5349?transport=tcp`）。
- **`mode = "builtin"`**：内置 STUN/TURN（webrtc-rs `turn` crate），同样使用上述 REST 凭据校验（过期即拒绝）。需要 `listen_udp`（如 `0.0.0.0:3478`）、`public_ip`（写入中继候选的公网地址）；可选 `listen_tcp`（TURN over TCP）、`relay_bind`（中继套接字绑定地址，默认 `0.0.0.0`）、`relay_ip`（默认等于 `public_ip`）、`relay_port_min`/`relay_port_max`（不设则由系统分配端口）。`urls` 留空时自动生成 `stun:`/`turn:` 地址。内置实现不支持 TURNS（TLS）与 IPv6 中继，规模较大时建议使用 coturn。
- **`mode = "none"`**：只下发 `urls` 中的 `stun:` 地址，不签发中继凭据（NAT 打洞失败时无法中继）。

`force_relay = true` 时要求客户端只使用中继候选。

### NAT 类型探测（`[[turn.stun]]`）

为提高 P2P 打洞成功率，内置模式可以额外开启若干 STUN 端点，供客户端按 RFC 5780 判断自身 NAT 的映射行为与过滤行为。每个端点一个 `[[turn.stun]]` 条目，**放在 `[turn]` 段的最后**（TOML 中其后的键会归入该条目）：

```toml
[[turn.stun]]
listen = "0.0.0.0:3479"            # 本地绑定的 UDP 地址
# public = "203.0.113.10:3479"     # 客户端看到的地址，默认为 public_ip（未设置时为 relay_ip）+ listen 端口
```

- **行为**：端点只应答 STUN Binding 请求（RFC 5389，必须带 magic cookie），成功响应包含 `XOR-MAPPED-ADDRESS`、`RESPONSE-ORIGIN`（实际发出响应的端点的公网地址）与 `FINGERPRINT`；请求中的 `FINGERPRINT` 会被校验。无需认证。其他方法、指示、响应、无 cookie 的 RFC 3489 请求、格式错误或指纹错误的报文一律静默丢弃。
- **`OTHER-ADDRESS`**：只有当端点属于完整的一组时才携带——即第二个公网 IP 上有相同的两个端口（A:p、A:q、B:p、B:q），取 IP 与端口都不同的那个端点。只有一个公网 IP 时**不发送** `OTHER-ADDRESS`（RFC 5780 7.4 的要求；否则标准客户端会把同一地址当作“另一 IP”，误判为与端点无关的映射）。
- **`CHANGE-REQUEST`**（`0x0003`，change IP `0x04`、change port `0x02`）：从相应的另一个端点套接字发出响应（同 IP 另一端口 / 另一 IP 同端口 / 另一 IP 另一端口），优先与 `OTHER-ADDRESS` 一致的端点。没有这样的端点时回复 `420 Unknown Attribute`，`UNKNOWN-ATTRIBUTES` 中列出 `CHANGE-REQUEST`（RFC 5780 第 6 节的做法，不选择静默忽略，客户端可立即得到结论）。与 RFC 5780 略有不同：单 IP 双端口时也执行 change port（标准客户端在没有 `OTHER-ADDRESS` 时不会发送该请求，Farwing 客户端可借此检测端口相关的过滤）。
- **不支持** `RESPONSE-PORT` 与 `PADDING`（二者可把响应引向其他端口或放大响应），与其他不认识的必须理解属性一样回复 `420`。
- **限速**：按来源 IP（IPv6 按 /64）每秒 20 个响应、突发 40 个，超出的请求静默丢弃；跟踪的来源数有上限。响应只比请求略大（无 `SOFTWARE` 属性），反射放大有限。
- **主 STUN/TURN 端口**（`listen_udp`，由 `turn` crate 处理）照常应答 Binding，但不支持 `CHANGE-REQUEST`，也不参与 `OTHER-ADDRESS` 的配对；它算作第一个端点（公网地址不得与额外端点重复）。未配置 `[[turn.stun]]` 时不会额外开启任何端口，行为与之前完全相同。
- **下发给客户端**：每个端点的公网地址以 `stun:<public>` 追加到会话 ICE 服务器列表（`session.start` 的 `ack` 与 `session.incoming` 中的 `ice_servers`，无凭据的那一项；手工配置的 `urls` 中已有的不重复添加），TURN 地址不变。客户端可向每个 `stun:` 地址发送 Binding 请求，响应带 `RESPONSE-ORIGIN` 的即支持 NAT 探测，带 `OTHER-ADDRESS` 的支持完整的 RFC 5780 测试。注意每个 STUN 地址都会让客户端在 ICE 收集阶段为每个本地接口多发一次请求，不宜配置过多。
- **只在 `mode = "builtin"` 时生效**；其他模式下忽略，并在启动时告警。

推荐布局：

| 场景 | 配置 | 能做的测试 |
|------|------|------------|
| 一个公网 IP | 两个端口，如 `0.0.0.0:3479`、`0.0.0.0:3480` | 不同端口的映射比较、change port 过滤测试 |
| 两个公网 IP | 两个 IP 上相同的两个端口，共 4 个端点，`listen` 写各自的本地 IP（第二个 IP 的端点需写 `public`） | RFC 5780 全部映射与过滤测试（带 `OTHER-ADDRESS`） |

`farwing-server.example.toml` 中有两个公网 IP 的完整示例。启动时会校验：`listen` 互不冲突（含 `listen_udp`；同端口时通配地址 `0.0.0.0`/`[::]` 与任何地址冲突）、`public` 为可用地址（非 0 端口、非通配、非组播）且与 `listen` 同一地址族、公网地址互不重复；同一地址族中出现多个公网 IP 时，每个端点的 `listen` 必须是具体的本地 IP（通配地址无法保证响应从正确的 IP 发出）。缺少配对、无法发送 `OTHER-ADDRESS`，或 `listen` 是公网 IP 却未写 `public` 时，启动日志会告警。

部署在 1:1 NAT 之后（如云主机的弹性公网 IP）时，`listen` 写内网 IP、`public` 写对应的公网地址。`CHANGE-REQUEST` 的响应从另一个套接字发出，对 NAT 而言是新的出站流，因此需要静态 1:1 NAT（出站源地址固定映射到 `public`）；仅做端口转发时，响应的源地址可能被改写，`RESPONSE-ORIGIN` 将与实际不符。

## 需要开放的端口

| 端口 | 协议 | 用途 |
|------|------|------|
| `listen`（默认 8443） | TCP | HTTPS 控制面与 WSS 实时面（反向代理模式下由代理对外开放 443） |
| 3478 | UDP（+TCP） | STUN/TURN（coturn 或内置 TURN） |
| 中继端口范围（示例 49160–49200） | UDP | TURN 中继（coturn `min-port`/`max-port` 或内置 `relay_port_min`/`relay_port_max`） |
| `[[turn.stun]]` 的端口（示例 3479、3480） | UDP | 仅内置模式配置了 NAT 类型探测端点时 |
| 5349 | TCP | 仅当 coturn 启用 TURNS 时 |

## Docker 与 Compose

```sh
# 在仓库根目录构建镜像
docker build -f server/Dockerfile -t farwing-server .

# 在 server/ 目录生成配置（容器内数据目录固定为 /var/lib/farwing-server）
cd server
docker run --rm --user "$(id -u):$(id -g)" -v "$PWD:/out" farwing-server \
    init --config /out/farwing-server.toml \
    --public-url https://farwing.example.com:8443 --data-dir /var/lib/farwing-server
chmod 0644 farwing-server.toml        # 容器内以 uid 10001 读取

# .env：coturn 使用与配置相同的共享密钥
printf 'TURN_SECRET=%s\nTURN_EXTERNAL_IP=%s\n' \
    "$(sed -n 's/^shared_secret = "\(.*\)"/\1/p' farwing-server.toml)" 203.0.113.10 > .env

docker compose up -d
docker compose exec -it farwing-server farwing-server user create alice
docker compose logs farwing-server | grep sha256_fingerprint
```

镜像以非 root 用户（uid 10001）运行，数据卷为 `/var/lib/farwing-server`，配置路径由 `FARWING_CONFIG=/etc/farwing-server/farwing-server.toml` 指定。Compose 中 coturn 使用 host 网络，以便中继端口范围直接可达。

服务端依据来源 IP 做限速和网络唤醒的“同一网络”判断。若 Docker 的 userland-proxy 把来源地址改写成网桥地址，这两项会失真；此时可让 farwing-server 也使用 `network_mode: host`，或在前面放反向代理并配置 `trusted_proxies`。

## systemd

见 `deploy/farwing-server.service` 文件头部的安装步骤。要点：

```sh
sudo useradd --system --home-dir /var/lib/farwing-server --shell /usr/sbin/nologin farwing
sudo install -d -m 0750 -o farwing -g farwing /var/lib/farwing-server
sudo install -m 0755 target/release/farwing-server /usr/local/bin/
sudo systemctl enable --now farwing-server
sudo -u farwing farwing-server --config /etc/farwing-server/farwing-server.toml user create alice
```

## 备份

需要备份的内容都在 `data_dir`：`farwing.db`（账号、设备公钥与资料、令牌哈希、好友关系与授权、协助收藏）、`wallpapers/`、`tls/`（自签名证书；丢失后客户端需重新固定指纹），以及配置文件本身（含 TURN 共享密钥）。

- 在线备份（推荐）：`sqlite3 /var/lib/farwing-server/farwing.db ".backup '/backup/farwing.db'"`，再复制 `wallpapers/` 与 `tls/`。
- 离线备份：停止服务后复制整个 `data_dir`（WAL 模式下若服务未停止，必须同时复制 `farwing.db-wal`，因此更推荐上面的在线方式）。

服务端只保存所有令牌的 SHA-256 与设备公钥（设备私钥只在客户端），备份泄露不会直接泄露可用凭据；密码为 Argon2id（m=19 MiB, t=2, p=1）哈希。

## 升级

1. 备份（见上）。
2. 停止服务，替换二进制（或重新构建镜像），启动服务。
3. 数据库结构版本记录在 `meta` 表（当前为 4）。服务端只打开当前结构版本的数据库，**没有迁移**：其他版本的数据库（更旧或更新）一律拒绝启动并提示 `no migration: move the file aside to start with an empty database`，此时把旧文件移走后重新启动即得到空数据库（需重新创建账号，客户端重新登录）；新数据库直接按当前结构创建。回退版本时请使用升级前的备份。
4. 会话与实时连接状态只在内存中：重启会结束所有会话（`server_shutdown`），客户端按协议重连（恢复令牌在重启后失效，客户端重新获取入场券）；账号登录与设备身份不受影响。

**旧版本**：Farwing 尚未发布，不提供与旧版本的兼容（以后需要时另行设计）。客户端与服务端须使用同一版本；旧数据库按上面第 3 条处理；旧配置中已删除的段与键（如 `[input]`、`[[updates]]`、`[limits] assist_confirm_wait_ms`，后者现为 `assist_decision_wait_ms`）会使启动报错，删掉或改名即可，也可以直接用 `init` 重新生成。

## 日志与隐私

日志使用 `tracing`，记录请求方法、路径、状态码、耗时、客户端 IP 与请求 ID，以及会话/设备/好友/授权生命周期事件。服务端不记录密码、各类令牌、请求签名、入场券、PAKE 消息与密钥确认、SDP 内容或 ICE 候选；协助码本身从不经过服务端。

## 实现说明

- 设备 ID 为 16 位小写 base32（`[a-z2-7]`），由设备公钥推导（`crypto::local_device_id`），与公钥一一对应；其他格式的 ID 不被接受。`connect_id` 为首位非零的 9 位随机数字，首次调用 `POST /assist/identity` 时分配，此后不变。服务端从不按硬件信息去重，同机多实例（不同密钥）是不同设备。
- 每台设备同时只有一条在线连接（新连接以 `4001` 取代旧连接，并递增 `epoch`，旧 `epoch` 的会话以 `host_offline` 结束）；被控断线后 300 秒内可恢复，超时后其会话以 `host_offline` 结束；主控断线未 `bye` 时会话保留 300 秒，超时以 `controller_left` 结束。设备的 `last_seen_at` 在上下线时更新。
- 速率限制：新设备登记按来源 IP、登录失败按来源 IP 与用户名、协助尝试按（账号, connect_id）与 connect_id、好友申请按账号（每小时 20 次），实时连接按 200 条/秒（突发 400），NAT 探测端点按来源 IP 每秒 20 个响应（突发 40）。签名重放缓存只在内存中，重启后清空（时间偏差窗口仍限制旧签名）。
