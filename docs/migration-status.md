# Farwing 第一轮迁移状态与验证结果

> 本文件记录第一轮（Farwing Protocol v1）迁移；第二轮（Farwing Protocol v2）见 [protocol/v2-design.md](protocol/v2-design.md)。

日期：2026-10-14。范围：OpenUUYC v1.0.0-rc.3（`main` @506df56）全部现有功能的协议与服务迁移；随后已跟进上游 v1.0.0（9d436cb），取舍见 [upstream-sync.md](upstream-sync.md)，跟进后的复测见第 8 节。结论：所有功能已改用 Farwing Protocol 与自建 Farwing Server，客户端不再包含 UU 业务协议、官方服务地址、官方凭据或兼容组件。核心链路已经用真实客户端和自建服务端到端验证；需要第二台机器、驱动、管理员授权或会改动本机桌面的项目，下面逐项说明实现状态和待验证条件。

## 1. 验证环境

- Windows 11 家庭中文版，Intel 核显 + NVIDIA GeForce RTX 5060 Laptop GPU，单机。
- Rust 1.95.0（MSVC），客户端 `cargo build --release`，服务端 `server/` 独立构建。
- Farwing Server 本机运行：`127.0.0.1:18443`（自签名证书，客户端固定指纹），内置 STUN/TURN（UDP/TCP），SQLite。
- 客户端：同机四个 profile，各自独立的设备身份、凭据、配置和单实例锁：
  - `a`：主控，账号 alice；
  - `b`：无窗口被控 `host --allow`，账号 alice；
  - `c`：其他账号 bob；
  - `d`：未登录、未配置服务器。
- 账号由管理员 CLI 创建：`farwing-server user create alice/bob --password-stdin`。

自动化测试：

| 套件 | 结果 |
| --- | --- |
| 服务端 `cargo test` | 47 项全部通过，进程内真实 HTTPS/WSS/TURN；本轮新增“跨账号不能对他人设备建会话”断言 |
| 协议 crate `crates/protocol` | 3 项通过 |
| 客户端 `cargo test --lib` | 13 项通过，含新增的对端帧分发与往返测试（每种帧类型只由其消费者认领；FCT 记录与 FPP 帧不会混淆） |

## 2. 端到端验证结果（真实客户端 + 自建服务）

| 项目 | 结果 | 说明 |
| --- | --- | --- |
| 服务器探测与证书固定 | 通过 | `server probe` 显示 SHA-256 与服务器日志一致；`server set --pin` 后全部 HTTPS/WSS 走固定指纹 |
| 设备登记 | 通过 | `POST /devices/init`；同机多 profile 得到不同设备 ID |
| 登录 / 退出 | 通过 | 用户名密码（管理员创建）；退出后本地凭据清除、服务器解绑设备 |
| 账号设备列表、分组、详情、壁纸上传下载、允许被控 | 通过 | 设备中心与 `devices` 命令 |
| 在线（WSS presence） | 通过 | 主控看到被控“在线 / 可控 是”。服务器重启后，被控的 resume 被拒（401，令牌不跨重启），随后用新票据自动重新上线 |
| 会话建立 | 通过 | `POST /sessions` → `session.start` / `session.incoming` → `rtc.offer/answer/candidate` → 对端 HELLO 特性协商 → FCT 控制通道与 control/session/bulk 数据通道 |
| 音视频完整生产链 | 通过 | 被控 DXGI 采集 → NVENC AV1 → WebRTC → 主控 DXVA AV1 硬解 → 渲染；Opus 桌面声音 → NetEq 播放 |
| 直连（P2P） | 通过 | host↔host（IPv6），接收、解码、渲染约 144 fps，丢包 0%，端到端约 3.3 ms |
| 中继（TURN） | 通过 | `--transport relay`，经内置 TURN（UDP 中继），约 133 fps，丢包 0% |
| 仅音频连接 | 通过 | 会话 purpose=audio，NetEq 正常出声 |
| 端口转发 TCP | 通过 | 本地 `127.0.0.1:28090` → 被控 `127.0.0.1:18090`，HTTP 200，内容一致 |
| 端口转发 UDP（Farwing 扩展） | 通过 | 5/5 回显 |
| 文件传输 | 通过 | 浏览远端目录；下载 3 MiB、上传文本，SHA-256 均一致 |
| 远端诊断包 | 通过 | 经 DIAG 通道获取脱敏 ZIP（14 项，0 警告） |
| 远程协助 | 通过 | 被控开启密码协助后得到 9 位设备号。错误验证码：被控验签失败，服务器返回 403，不建会话。正确验证码：其他账号（bob）建立 assist 会话，约 144 fps |
| 远程电源请求链路 | 通过（未执行） | 主控 → 服务器 → `power.request` 推送 → 被控因未允许远程电源而拒绝 → 结果回报服务器；本机没有执行关机或重启 |
| 软件更新通知 | 通过 | 服务器 `[[updates]]` 发布 1.0.1 → 设备中心弹出“发现新版本”（该配置段后来已删除，更新公告改在网页端“客户端更新”中发布） |
| 正常关闭 | 通过 | 关闭观看窗口：退出码 0，服务器会话结束，被控释放采集，保持在线 |
| 连接中取消 | 通过 | 建立过程中关闭：被控关闭半建立的连接，服务器无残留会话 |
| 被控异常退出 | 通过 | 强制结束被控进程约 12 s 后主控 ICE 失败，按序回收媒体、通道和音频；重连请求得到 `device_offline` 后停止（见 §4.3） |
| 撤权：移除设备 | 通过 | 会话中执行 `device remove <主控>`：服务器立即结束会话，观看窗口退出；该 profile 下次请求得到 `session_revoked`，本地登录随即作废 |
| 撤权：禁用账号 | 通过 | `user disable alice` 吊销 2 个登录，被控下线退出，主控凭据作废；`user enable` 后可重新登录 |
| 跨账号隔离 | 通过 | bob 的设备列表看不到 alice 的设备，客户端不发起会话；服务端对他人设备返回 404 `device_not_found`（有测试） |
| 多实例隔离 | 通过 | profile a/b/c/d 的凭据分别位于 `farwing.<profile>.*`，数据位于 `%LOCALAPPDATA%\Farwing\profiles\<profile>` |
| 图形界面 | 通过 | 新登录页（服务器地址、证书指纹、账号密码、游客协助入口）和登录后设备中心正常渲染（窗口截图核对） |

`dev` 子命令是本轮新增的隐藏验收工具，调用与设备中心相同的功能服务，不打开窗口：`dev ports`、`dev files`、`dev diagnostics`、`dev power`，用法见 [client-usage.md](client-usage.md#9-开发验证命令隐藏)。

## 3. 各功能迁移情况

“已验证”指第 2 节的真实端到端验证；“未端到端验证”的项目，原因与所需条件见第 5 节。

| 功能 | 原 UU 依赖 | Farwing 实现 | 状态 |
| --- | --- | --- | --- |
| 账号登录 | 签名 REST、短信 / 扫码登录 | HTTPS `/auth/login`，管理员 CLI 建号，Argon2id；短信 / 扫码随 UU 账号体系移除 | 已验证 |
| 设备管理 | UU 设备接口 | `/devices*`：列表、分组、详情、改名、移除、允许被控、壁纸 | 已验证（改名仅服务端测试） |
| 在线状态 | Socket.IO room / bmsg | WSS presence，单次票据 + resume、心跳、关闭码 | 已验证 |
| 会话协调 | Socket.IO control / soac | `POST /sessions` + WSS `session.*`、`rtc.*`，带代次、请求 ID、错误和超时 | 已验证 |
| 接管 | UU force_join | `force=true`，busy 冲突提示 | 仅服务端测试 |
| 远程协助 | UU assist 接口 | `/assist/*`：密码、确认、密码加确认三种模式，随机盐验签，游客被控 | 密码模式已验证；确认模式和游客仅服务端测试 |
| 最近与收藏 | UU saved 接口 | `/assist/saved/*` | 仅服务端测试 |
| 视频 | UU RTP 扩展、rs-fec 表 | 标准 WebRTC；Farwing RTP 扩展 URI；`video/x-farwing-rsfec`；FEC 冗余改用二项分布模型 | 已验证（2026-10-19 起 FEC 才真正发出：内置 WebRTC 库此前仍按旧名称建立 FEC 流，见 [selftest-results.md](selftest-results.md) §20） |
| 音频 | — | Opus / NetEq 不变 | 已验证 |
| 控制传输 | 混合 KCP（`a=x-uuremote-mix-kcp`） | FCT（`a=x-farwing-fct:1`，magic `FCT`+版本） | 已验证 |
| 对端协议 | PbControlMessage + ECHO 内容识别 | FPP 帧（`FW` 头 + 类型 + 请求 ID），HELLO 特性等级，按类型分发 | 已验证 |
| 远程输入 | UU 输入 protobuf | `INPUT` / `MOUSE_POLICY` 帧 | 帧分发有单元测试；未端到端注入 |
| 剪贴板 | UU 剪贴板消息 | `CLIPBOARD` 帧 | 协商与往返测试通过；未端到端同步 |
| 文件传输 | UU 文件协议 | `FILE` / `FILE_BLOCK` 帧（bulk 通道） | 已验证 |
| 拖放 | UU 拖放 | `DRAG` / `DRAG_DATA` 帧 | 已协商；未端到端 |
| 批注 | UU 批注 | `ANNOTATION` 帧 | 已协商；未端到端 |
| 端口转发 | UU 端口映射 | `PORT` 帧；TCP，以及 Farwing 扩展 UDP | 已验证 |
| 诊断 | UU 诊断通道 | `DIAGNOSTICS` 帧（diag 通道） | 已验证 |
| 显示管理 / 虚拟屏 | UU RPC | `SESSION` 帧（CaptureSetting 等） | 初始采集设置与 QoS 已验证；切屏和分辨率修改未端到端 |
| 音质调整 | UU 音质消息 | `AUDIO_QUALITY` 帧 | 往返测试通过；未端到端 |
| 麦克风 | — | 同一 WebRTC 会话的音频发送 | 已协商；需虚拟声卡 |
| 电源与唤醒 | UU 电源接口、UU 路由器唤醒 | `/devices/{id}/power` + `power.request`；同网设备代发 WoL | 请求和拒绝链路已验证；未实际执行；WoL 仅服务端测试 |
| 网络策略 / 自动中继 | UU 下发阈值，被控要求主控切中继 | 已删除；ICE 按实测路径质量在直连与中继之间切换（v2-design §11） | 未人为制造丢包 |
| 后台服务 / 无人值守 | OpenUUYC 服务身份 | `FarwingHostService`，DPAPI 保险库，仅限默认 profile | 未验证（需管理员授权） |
| 安装 / 更新 / 卸载 | UU 与 OpenUUYC 路径、发布源 | `%ProgramFiles%\Farwing`，卸载项 `Farwing`；更新源 `/updates/windows` | 更新通知已验证；安装和卸载未验证 |
| 驱动 | OpenUUYC 预编译驱动 | Farwing 硬件 ID / GUID，源码保留；嵌入可选，仓库不带二进制 | 未构建（无 WDK 和签名证书） |
| 插件 | `oumeta` ABI | `farwing_*` 符号，`.fwmeta` 段 | 编译通过；未加载实测 |
| 通知 | OpenUUYC AUMID / CLSID | Farwing AUMID / CLSID，按 profile 区分 | 未专项验证 |
| 多实例 | — | `--profile` / `FARWING_PROFILE` | 已验证 |

## 4. 与原行为的差异和需要确认的决定

1. **许可前提。** OpenUUYC 原创部分按其声明保留所有权利，未采用开源许可（[licenses/OpenUUYC-NOTICE.txt](../licenses/OpenUUYC-NOTICE.txt)）。Farwing 的新增和改写部分标为 AGPL-3.0-only。以 AGPL 分发包含 OpenUUYC 原创部分的整体程序，需要原权利人授权。
2. **不再与 UU 互通。** 官方 UU 客户端和 OpenUUYC 都不能连接 Farwing；原来“与官方端连接时”的兼容分支不会再被触发。
3. **`device_offline` 不自动重试。** 原实现除少数 UU 错误码外都会重试 5 次（每次间隔 3 s），那几个 UU 错误码的含义无法从参考代码确定。现在 `device_offline`、`host_disabled` 等明确的拒绝不重试，只重试服务端标为可重试的错误和 `conflict`。如需在被控短暂掉线时自动等待它回来，可在验收后调整。
4. **FEC 冗余量。** 原实现使用 UU SDK 的私有冗余表，现在改为按二项分布丢包模型计算：控制通道目标 1.5%，视频 1e-3，严格 1e-4。冗余量与原表不完全相同。
5. **码率表来源待确认。** `parameters.rs` 中的码率表沿用原值，来源需要你确认（已在 THIRD_PARTY_NOTICES 标注）。
6. **少量标识变化。**
   - 注入输入标记改为 `0x46575549`。
   - 插件段名由 `.oumeta` 改为 `.fwmeta`，现有插件需要重新编译。
   - 不再支持“UU 路由器代发唤醒”，`router_wol` 恒为 false。
7. **服务端对规范空白处的解释**（共 24 条，见服务端实现报告）。较重要的有：
   - 协助加入成功会替换被控当前会话；
   - 被控结束会话时，主控统一收到 `host_closed`；
   - 设备令牌轮换只在规范中出现，未定义触发条件，因此未实现。（第二轮起设备令牌已取消：设备以每请求签名认证，账号令牌绑定签名设备并按 30 天不用失效，单独窃取令牌无法使用，轮换不再需要，见 farwing-protocol.md §1.3、§3.2。）

## 5. 未端到端验证的项目与所需条件

| 项目 | 原因 | 验证条件 |
| --- | --- | --- |
| 键鼠注入、触摸、鼠标策略、光标、快捷键 | 单机测试时会向正在使用的桌面注入输入 | 两台电脑或一台虚拟机，在观看窗口开启控制 |
| 剪贴板同步、拖放 | 同机共享剪贴板，会覆盖你当前的剪贴板 | 两台电脑或虚拟机 |
| 批注、切屏、分辨率 / DPI 修改、超级屏 | 需要 GUI 操作，并会改动本机显示配置 | 两台电脑；虚拟屏和超级屏还需要显示驱动 |
| 麦克风、虚拟声卡 | 被控需要虚拟音频驱动（测试签名） | 构建并签名驱动，启用测试签名后验证 |
| 电源实际执行 | 会关闭或重启测试机 | 被控开启“远程电源”的独立机器 |
| 网络唤醒 | 需要第二台支持 WoL 的机器、BIOS 设置和同网协助设备 | 两台机器 |
| 后台服务、无人值守、锁屏接入、安装 / 更新 / 卸载 | 需要管理员授权，并写入 Program Files、服务、注册表和证书库 | 你授权后，用默认 profile 在测试机或虚拟机上执行“安装服务” |
| 驱动（显示 / 输入 / 音频） | 本机没有 WDK 和代码签名证书 | 按 [drivers/README.md](../drivers/README.md) 构建签名，嵌入后安装验证 |
| 游客协助、确认模式协助、收藏 / 最近 | 需要 GUI 操作（服务端测试已覆盖） | 设备中心操作 |
| 服务端 Linux、Docker、systemd、coturn 外部 TURN、TURNS | 开发环境没有 Linux、Docker 和 coturn | Linux 主机或容器环境 |
| 网络劣化时自动切换中继 | 没有人为制造丢包和延迟 | 使用 clumsy 等工具注入丢包和延迟 |

## 6. 本轮验证中发现并修复的问题

1. **FCT 记录被当成 SCTP 包。** 内嵌 WebRTC 的 SCTP 分流规则只排除 UU 的 `PCK` 记录，导致主控的 FCT 记录被交给 SCTP，出现“checksum mismatch”，HELLO 无法完成。已改为排除 `FCT`，并在 SCTP 入口丢弃残留记录。
2. **无窗口被控首连超时。** `host` 命令没有预检编码能力，首个会话要先花约 11.5 s 探测编码器，超过主控 12 s 的连接时限。现在与设备中心一致，先预检后上线。
3. **新 profile 首次启动报错。** 未设置服务器的新 profile 显示“无法打开账号会话”，没有进入登录页。已修复。
4. **非默认 profile 可以安装全机组件。** 非默认 profile 会弹出“安装服务”，可能把全机服务和驱动装到开发实例上。现在界面和安装入口都限定为默认 profile。
5. **更新源平台名不一致。** 客户端请求 `windows-x64`，服务端只认 `windows`，更新通知永远不会出现。已统一为 `windows`，并写入协议规范。
6. **自建 TURN 的中继切换。** 主控切换到中继时只选 TURNS；服务器只提供 TURN 时无法切换。已增加回退（第一轮开发期间修复）。

## 7. 复现端到端验证

```powershell
# 服务端（Windows 下同样可用）
farwing-server init --config e2e.toml --public-url https://localhost:18443 --data-dir ./data
# 编辑 e2e.toml：listen = "127.0.0.1:18443"；[turn] mode = "builtin"，listen_udp/listen_tcp/public_ip/relay_bind 用本机局域网 IP
farwing-server --config e2e.toml user create alice --password-stdin < alice.pw
farwing-server --config e2e.toml serve        # 启动日志打印证书指纹

# 客户端（两个 profile）
Farwing.exe --profile a server set localhost:18443 --pin <指纹>
Farwing.exe --profile b server set localhost:18443 --pin <指纹>
Farwing.exe --profile a login --username alice --password-stdin < alice.pw
Farwing.exe --profile b login --username alice --password-stdin < alice.pw
Farwing.exe --profile b host --allow --allow-ports --access-code <访问码>
Farwing.exe --profile a devices
Farwing.exe --profile a connect --device-id <b 的设备 ID> --mute
Farwing.exe --profile a connect --device-id <b 的设备 ID> --transport relay --mute
Farwing.exe --profile a dev ports --device-id <ID> --local 127.0.0.1:28090 --remote 127.0.0.1:<端口>
Farwing.exe --profile a dev files --device-id <ID> --list <目录> --download <文件> --to <目录>
```

注意：

- 同机测试时，内置 TURN 不能只监听 127.0.0.1：WebRTC 套接字绑定在网卡地址上，访问回环地址会失败（os error 10049）。
- 首次让 `farwing-server` 监听局域网地址时，Windows 防火墙可能弹出提示。本机内部流量不受影响。

## 8. 跟进上游 v1.0.0 后的复测（2026-10-14）

取舍详见 [upstream-sync.md](upstream-sync.md)。复测结果：

| 项目 | 结果 |
| --- | --- |
| 构建与测试 | 客户端 `cargo check` 无警告。单元测试：客户端 13 项、协议 crate 4 项（新增扩展往返测试）、服务端 47 项，全部通过。`cargo dist` 通过清单和启动检查（含新的 PerMonitorV2 DPI 清单） |
| 直连会话 | 约 144 fps，丢包 0%；被控日志显示新的 GPU 调度（Realtime 优先级） |
| `display_geometry` 扩展 | 主控收到被控首选分辨率 `preferred=(2560, 1600)`。当时更早的服务器会丢弃该扩展，服务端须随客户端一起升级（Farwing 不提供与旧版本的兼容，客户端与服务端始终使用同一版本）；之后新增的扩展会被原样转发 |
| 中继会话 | relay↔host，约 143 fps，丢包 0% |
| 端口转发、文件传输 | TCP 正常；下载和上传的 SHA-256 均一致 |
| 跨账号远程协助 | 约 144 fps，关闭后会话清理正常 |
| 设备中心 | 正常启动渲染，无错误日志 |

以下需要人工在界面中验证：

- 全屏顶部操作栏、本地比例拉伸菜单、被控设置中的“GPU 调度”选项。
- 批注改由 UIAccess 常驻进程绘制：需要安装服务，并按第 5 节的条件验证。
