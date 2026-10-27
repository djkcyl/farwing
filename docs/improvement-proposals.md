# Farwing 协议与功能改进建议（第二轮评审）

日期：2026-10-14。状态：**仅建议，尚未实施**。

本轮按 7 个领域对全部功能实现做了只读评审：

1. 连接与传输
2. 视频链路
3. 控制面与显示
4. 输入、光标与音频
5. 剪贴板、拖放、文件、端口与诊断
6. 批注、协助、账号与服务端
7. 被控运行时与横切部分

评审目标是找出两类问题：一是 v1 协议与实现中的结构性妥协，二是迁移后留下的不一致，并给出 Farwing 原生的方案。标注“已核实”的条目，都由汇总人对照代码亲自确认过。行号以当前工作树为准。

## 0. 结论摘要

- **有 8 个问题应该立即修复（第 1 节）。**
  - 两个是安全漏洞：协助会话可以冒充账号会话来提权；“密码 + 确认”模式可以绕过密码。
  - 其余是迁移带来的真实缺陷：离线判断比较的是服务端不会返回的状态字符串；显示设置失败后遮罩不会消失；无人值守首连可能超时；单 STUN 部署下 NAT 被误判，打洞变得过于激进；虚拟屏文案里的品牌名不是 Farwing。
  - 这些都不需要改协议，工作量小。
- **v1 的结构性妥协集中在 6 处（第 2 节）。** 建议在首次公开发布前，一次性升级到 **Farwing Protocol v2**。现在双端和服务端都还没发布，正是做不兼容清理成本最低的时候。
  - **能力协商四套并存：** 账号能力表、`extensions`、HELLO 等级、`ConnectOptions` 里的数值码。
  - **控制面仍是 `PbControlMessage`：** 包装层把请求关联方式和错误模型各搞了三套。
  - **视频把“画质档”和“分辨率盒”绑死：** 非 16:9 屏会被非整数缩放，文字发虚。
  - **数据面全部挤在同一个 SCTP 调度上：** 没有流控，存在跨通道队头阻塞。
  - **输入只传 Windows VK：** 键盘布局不同时字符出错；锁定键语义不对；光标位置不同步。
  - **搬文件有四套实现，非原生路径已经走不到：** 约 1500–2000 行代码在 Farwing 之间不可达。
- **产品目标需要新的身份与授权模型（第 3 节）。** 局域网免账号、无服务器配对、观看与控制分开授权、多人共享，都依赖它：设备密钥、DTLS 指纹绑定、PAKE、按会话和按能力授予的权限。
- **应保留的成熟部分见第 4 节，需要你决策的问题见第 5 节，建议的实施顺序见第 6 节。**

---

## 1. 立即修复（不改协议，工作量都是 S）

| # | 问题 | 证据 | 影响 | 修复 |
| --- | --- | --- | --- | --- |
| B1【安全·高，已核实】 | 被控按主控自报的 `options.connect_type` 决定是否按协助会话处理；服务端认证过的 `incoming.connect_type` / `incoming.controller` 从未被读取 | `src/transport/signal/host.rs:443-451`；服务端只校验信封层的 `connect_type`（`server/src/hub.rs:1061-1068`），`options` 原样转发 | 协助主控把 `options.connect_type` 改为 1，就能绕过协助限制，获得文件、端口、诊断、虚拟屏，被控关闭协助后会话也不会被撤销；还能伪造 `controller_device_id` 冒充别人 | 被控只信 `session.incoming` 的 `connect_type`/`controller`/`purpose`；从 `ConnectOptions` 删除这些字段；服务端拒绝不一致的请求 |
| B2【安全·中高，已核实】 | `POST /assist/join/confirm` 在 `request_id` 为空时，不检查被控模式、也不验签，直接发起确认；被控收到确认推送时不检查自己的模式 | `server/src/http/assist.rs:197-209`；`src/features/host/assist.rs:611-639` | “密码 + 确认”模式下，只要骗到主人点一下就能进入；“仅密码”模式下任何人都能弹窗骚扰，且没有限速 | 纯确认请求只在被控模式为 confirmation 时受理；需要密码的模式必须带已验签的 `request_id`；被控按模式拒绝；加限速 |
| B3【缺陷，已核实】 | 离线判断写成 `status == "DISCONNECTED"`，而 Farwing Server 只返回 `online`/`offline` | 7 处：`account/power.rs:35`，`app/power.rs:37,40,113`，`app/device_status.rs:91`，`app/view/devices.rs:108,202` | 设备中心的网络唤醒恒报“只有离线设备可以发送开机请求”；关机/重启完成检测不到，120 s 后报超时；“离线”筛选恒为 0 | 统一改为 `!is_connected()`，status 反序列化为枚举 |
| B4【缺陷，已核实】 | 显示设置被拒绝或被控没有回应时，主控的“正在切换分辨率”遮罩不会消失：回执只记日志，没有超时 | `features/stream_control/display_settings.rs:289-301`；被控 `refresh()` 失败时不回复（`host/peer/channels.rs:366`） | 用户只能手动“继续观看” | 收到 -1/-2 就结束等待并显示原因；加 10 s 超时；被控对每个请求都回复 |
| B5【缺陷，已核实】 | 无人值守常驻进程的编码预检与上线并行，游客模式直接跳过预检 | `platform/windows/host_service/resident.rs:916-926` | 重启后未登录 Windows 这一核心场景，第一次连接可能撞上主控 12 s 的时限（实测冷探测约 11.5 s） | 与设备中心、命令行一致，预检完成后再上线；根治方案见 §2.7 |
| B6【缺陷，已核实，WAN 影响待实测】 | 内置的增强打洞协调器按“STUN 服务器 ≥2 个”判定锥形 NAT；Farwing 一般只下发 1 个 STUN，于是双方都被判为 Random，进入 mode 9（向 100 个随机端口硬喷探测包，60 s 一轮） | `vendor/webrtc-ice/src/agent/punch.rs:90-111`；`punch/actor.rs:236-249,616-630` | 公网连接时打洞策略失准：探测量大、可能触发防火墙或运营商告警，成功率也未必更高。局域网 host↔host 会先被选中，所以测试中没有暴露 | 短期：只有一个 STUN 时标为 Unknown（不进入硬喷）。长期见 §2.7 |
| B7【文案】 | 虚拟显示器名称与“最多支持 3 个虚拟屏”提示里的品牌名不是 Farwing | `viewer/stream_menu/topology_menu.rs:146`；`stream_control/display_topology.rs:222` | 品牌残留 | 改文案 |
| B8【安全·中】 | 限速“先检查、失败后计数”，并发可绕过；`assist/mode` 不限速，可用于枚举；登录 Argon2 没有并发上限（游客默认开放，任何人都可登记设备后压测登录）；协助码允许 6 位纯数字（约 20 bit） | `server/src/ratelimit.rs:15-41`，`http/assist.rs:59-120`，`http/account.rs:29-57`，`host/assist.rs:95` | 在线猜码、枚举、资源耗尽 | 改为先预留再验证；按 connect_id 做全局失败锁定，并提示被控“有人在尝试”；`mode` 按账号限速；Argon2 用信号量限制并发；协助码 ≥8 位且字母数字混合 |

另有两项需要先验证：

- **剪贴板等待无超时（P0 候选）：** 粘贴方的同步 `GetData` 可能一直挂起（`features/clipboard/mod.rs:626-643`），被控端队列满时还会直接丢消息（`host/clipboard.rs:84-90`）。修复：30 s 内没有进展就超时并发送 Cancel。
- **内置 TURN 未过滤对端地址（P2）：** 可能被用作访问服务器回环或内网的跳板（`server/src/turn_server.rs:109-125`），且凭据在会话结束后 12 h 内仍有效。需要先确认 webrtc-rs turn 的默认行为。

修完后建议补一轮**双机**回归，作为后续改造的基线。键鼠、剪贴板、拖放、显示设置至今没有端到端验证过。

---

## 2. Farwing Protocol v2：去掉六处结构性妥协

> 原则：媒体标准（WebRTC、RTP、Opus、H.264/HEVC/AV1）和成熟算法（goog_cc、libwebrtc 接收管线、NetEq、CM256）不动；替换的是业务语义的承载方式。双端和服务端一起升级，HELLO `protocol_version=2`。

### 2.1 能力与会话参数统一

**现状：同一件事有四到五个来源，而且大多是常量。**

- 服务器登记的设备 `capabilities`：静态列表，例如 `smart_mouse`、`manual_transfer`、`fps_super_screen`。它表达的是“能做什么”，不是“允许做什么”，例如被控没开远程电源，主控仍会显示关机按钮（`account/feature_ability.rs:58-79`）。
- `session.start.extensions`：主控单向声明，恒为 true（`session/negotiation.rs:87-95`）。
- HELLO 特性等级：`capture_setting=6` 是单调版本阶梯（≥3 色度，≥4 自定义码率，≥5 DPI/虚拟屏，≥6 HDR）。另有 `qos/system_metrics/virtual_mouse/fct` 声明了但没人读；`simple_action/private_screen/update_acquire` 常量未使用。
- 各特性自带的带内 hello：DRAG、DIAG、ANNOTATION 各有一套。
- `ConnectOptions` 里的数值码：`kind` 1/5/9 与 `purpose` 重复；`connect_type` 1/2 与信封重复（这正是 B1 的根源）；`client_type` 与账号 `platform` 是两套互相冲突的平台编号（1=iOS 对 1=Windows）。
- `CaptureParams`：`fps` 用档位 1–4 再加 `fps_count`；`quality` 1–6（文档写 6=自动，代码里 5=自动）；`chroma` 1/3；`resolution_type` 1–4。

**建议**

- **会话前门控只用账号能力**，而且把“能做”和“允许”分开：`capabilities` 加 `policy{control, files, ports, power, wol, assist}`。
- **会话内一律只看 HELLO：** 双向交换，特性按“名称 + 版本”声明，并且只声明运行时真实具备的。例如：
  - `display.virtual` 只在显示驱动可用时声明；
  - `video.hdr` 只在有 HDR 屏时声明；
  - `input.keyboard=2` 表示支持按位置传键（见 §2.5）。
- **删除：** `extensions` 布尔、特性等级阶梯、`client_type`、`kind`、`ConnectOptions.connect_type/controller_device_id`。
- **平台统一为字符串枚举**，例如 `windows/macos/linux/android/ios`。
- **服务端认证过的身份、目的、类型只存在于 `session.incoming`。**
- **HELLO 只交换一次，带超时。** 现在是双方各发请求、每 500 ms 重发、最多 30 次（`transport/rtc/channels.rs:324-380`），在可靠通道上没有必要。

**收益：** 消除 B1 这一类问题；门控能反映真实情况；以后新增功能只需加一个特性名。

**工作量：** M。

### 2.2 控制面：用 FPP 自身的请求/响应/错误，替换 `PbControlMessage`

**现状**

- SESSION 帧体仍是 `PbControlMessage{seq, timestamp, oneof}`，里面嵌套 `RpcRequest(bytes)` / `RpcResponse(message)`、SimpleAction 数字码 + JSON 参数。
- FPP 头的 `request_id` 和 RESPONSE/ERROR 标志实际无人使用，一律写 0。规范要求的“未知帧回 ERROR”也没有实现。
- 错误模型有三套：`CaptureSettingResponse.errors[]` 里 0 也算一条“错误”，-5/-6 其实是带 JSON 详情的通知；DisplayResult 只有码没有文字；麦克风和视频轨道各有自己的结果结构。
- 被控入站是“逐个解码器试探”：control 通道依次试 input→touch→files→draw→display→receive，同一帧最多解码 4–5 次；还残留 `bytes.first()==b'{'` 的 JSON 嗅探（`host/peer/ingress.rs:60-147`，`host/peer/channels.rs:175-343`）。主控已经是干净的按类型分发。
- 被控的升级通知借用 `ReportError(-6)`，而同一个 -6 在 CaptureSetting 回执里表示色度结果。

**建议**

- 统一用 FPP 头的 request_id + RESPONSE；失败用 ERROR 标志加 `PeerError{code 枚举, message, detail}`；“被调整”类通知放进结果的 `adjustments[]`。
- 去掉 SESSION 内层的 seq、timestamp 和 Rpc 包装；主控用统一的待回复表加超时。
- 被控只解码一次头部，按 FrameType 分派，每种消息只走规定的通道。删除 CONTROL 通道上的 FILE 入口（注释写着“iOS sends file RPCs here”）。
- 新增 `HostEvent{updating|restarting|sleeping|session_switch|shutting_down, expected_downtime_ms}`，替代 `ReportError(-6)`，并与 §2.7 的“等待设备”联动。
- 已有的双端扩展（`AUDIO_QUALITY` 的 set/applied/rejected + revision，ANNOTATION 的 token/request/result）可以直接作为样板。

**工作量：** M–L。

### 2.3 显示管理 v2

**现状**

- **一份快照包办一切：** 18 字段的 `CaptureSettingRequest` 全量快照同时承载画质、帧率、编码、光标合成、分辨率、DPI 和超级屏转换，靠哨兵值区分语义（screen_id -2/-1，宽高 -1，dpi 0/-1）。
  - 局部改动也会发出整份快照，被控收到就把 revision 加 1 并重置自动画质。
  - 智能鼠标模式下，游戏每切换一次相对/绝对鼠标，自动码率就重置一次（`stream_control/input.rs:182-234,336-351` → `capture_worker.rs:114-131`）。
- **屏幕列表字段失效或缺失：**
  - `dpr`、`pixel_*`、`builtin_screen_type` 被写成固定值；没有刷新率、HDR，也没有可用于排布的位置。
  - `screen_type=1` 把会话虚拟屏和常驻兜底屏混在一起，主控对后者显示“关闭”按钮后又被被控拒绝。
  - `screen.id` 是进程内递增号，被控重启后就变了。
- **操作语义难以预测：**
  - 虚拟屏固定按被控主屏尺寸、144 Hz 创建，主控请求的尺寸只是追加进模式列表。
  - 请求不受支持的模式时会**自动进入超级屏**，也就是关闭全部物理屏。
  - 帧率升档是两次请求，不是原子操作。
  - 分辨率和 DPI 必须分两次应用。
  - 主控 VirtualMode 的刷新率被写死为 60，被控也不读取。

**建议**

- **拆成三块：**
  - `StreamPolicy`（全局画质/编码，幂等快照 + revision）；
  - `TrackView`（每条轨道的屏幕和光标合成）；
  - `DisplayConfig`。
  - 被控只重置受影响的子系统。
- **DisplayList v2：**
  - 列表级：`revision`、`active_id`、`limits`。
  - 每屏：
    - `id`、稳定的 `uid`；
    - `kind`：physical / session_virtual / machine_fallback / foreign_virtual / exclusive；
    - `desktop_rect`、`rotation`；
    - `current/preferred/initial{w,h,refresh}`、`modes`；
    - `scale{cur,rec,list}`、`hdr{capable,enabled}`；
    - `ops{set_mode,set_scale,remove}`、`track`。
  - `display_geometry` 扩展并入 `preferred`。
- **`DisplayRequest` oneof：**
  - `SetMode{id,w,h,refresh,scale?,fallback: fail|exclusive_virtual}`
  - `CreateVirtual{w,h,refresh,scale,placement}`
  - `Remove`、`Enter/ExitExclusive`、`Restore`
  - 一次请求内原子应用（被控已有 journal 回滚能力）；结果显式返回，并与列表 revision 对齐（顺带根治 B4）。
  - 匹配本机的 W×H@Hz，对标 Moonlight。

**工作量：** M–L。

### 2.4 视频 v2

**F1（P0 快修，已核实）：分辨率不再与画质档绑定。**
- 画质档 1/2/3/4 对应 720p/1080p/1440p/2160p 的 16:9 盒子（`media/geometry.rs:54-61`），Auto 从 2 档起步。
- 实测 2560×1600 桌面被编码成 1728×1080，相当于 0.675 倍的非整数缩放；竖屏不会旋转盒子，缩得更厉害。
- Auto 升档等于换分辨率，会触发编码器重建、IDR 和解码器硬重置。
- 快修：按面积或长边限制；原生尺寸在上限内就不缩放；必须缩放时优先整数比。
- 正式方案：`resolution_policy{Native|MaxPixels|MaxLongEdge}` 与 `bitrate_policy` 分开；能力中给出实际探测的 `max_w/max_h/max_pixel_rate`。

**F2（P0 快修 / P1 RFI）：丢包恢复。**
- 现状：NACK 修不好的丢包要等解码超时才发 PLI（初始 3 s，稳态 1 s）；NACK 放弃时也不触发 PLI；被控对 PLI 有 600 ms 节流；编码器无限 GOP、单参考，没有 LTR 或参考帧失效。
- 快修：判定不可恢复（缺包超过 max(2.5·RTT, 2 帧, 约 60 ms)，或 NACK 已放弃）就立即发 PLI；被控节流改为 max(100 ms, 1.5·RTT)，IDR 在途时合并请求。
- 长期：加 Farwing 帧描述扩展（frame_id、参考帧）和 FPP `RecoveryRequest`；用 NVENC/AMF/QSV 的 LTR 或参考帧失效编出参考旧好帧的 P 帧；弱网可选帧内刷新。
- 收益：冻结时间从约 1 s 降到约 1 个 RTT。
- 状态（2026-10-18）：快修已在 P5 完成；长期部分按 [rfi-ltr-brief.md](protocol/rfi-ltr-brief.md) 实现了 NVENC H.264/HEVC 的 LTR 恢复（`frame-meta`、`VideoFeedback`），恢复帧比关键帧小约 30–40 倍；冻结时间仍由 2.5×RTT 的判定门限主导。

**F3：媒体协商三条消息。**
- 用 `MediaCapabilities`、`StreamRequest`、`StreamState` 取代以下几项：CaptureParams 的档位、DeviceCapability JSON（codec_impl 32/37 等数值码）、两张码率表、四套画质编号、靠 sdr_10bit 打补丁的 10-bit/HDR 混用。
- `StreamRequest` 使用精确整数 fps、kbps、分辨率策略，色度/动态范围/位深三者独立，并带编解码偏好和 latency_mode。
- 由被控单方决策，并在 `StreamState` 回报实际值和原因码。
- 顺带修正 H.264 fmtp 的 profile 声明：现在声明 Baseline 3.1，实际发的是 High/High444。

**F4：RTP 头扩展精简。**
- 现状：注册了 14 个视频扩展，ID 用到了 15，每个包被迫用两字节扩展头；发送时延被 toffset、sending-delay、video-timing 重复表达三次；playout-delay 恒为 0/0。
- 只保留 tcc、mid，再加首包的 `urn:farwing:frame-meta`（frame_id、关键帧/新画面/换源/恢复标志、source_epoch、参考帧）；color-space 只在变化时发，video-timing 抽样。
- 删除 SDP 和代码里 Farwing 被控从不发送的 RED、ULPFEC、FlexFEC-03、goog-remb、CVO、rid。
- 收益：单包有效载荷约多 4–7%（估算）。
- 状态（2026-10-18）：视频扩展已是 8 项一字节头；`urn:farwing:rtp-hdrext:frame-meta` 已实现（纪元、帧号、参考帧号、关键帧 / 恢复 / 长期参考标志），新画面与换源仍用各自的扩展。

**F5：FEC。**
- 现状：跨帧分组，小帧要攒够包数才封块，修复延迟约等于一个 RTT，并不比 NACK 快；冗余假设丢包相互独立，不适合 Wi-Fi 突发；FEC 与 RTX 会重复修复同一个包。
- 方案：按帧封块；用 TWCC 的丢包游程估计突发模型；接收端在修复包预计到达前暂缓 NACK。
- 需要弱网实测。

**F6：发送节奏。**
- 现状：6 ms 内排空整帧，近乎线速突发，再加 5 帧 VBV；浅缓冲链路上会自己造成丢包。
- 方案：在帧间隔的 α 倍内发完，α 随丢包和延迟自适应；保留突发学习器作为上限；低延迟模式用 1–2 帧 VBV。

**F7：码率与自适应。**
- 现状：起始速率 = 上限/5，截到 0.3–3 Mbps；Auto 每秒按档位跳。
- 起始速率按路由类型和该对端上次会话的带宽估计设定。
- 码率连续跟随 GCC；分辨率和帧率按 bpp 阶梯切换并带滞回；区分文字为主（保分辨率、降帧率）和运动为主（反之）。
- 码率表可以借此改为基于 bpp 的公式。

**F8–F12（P2）：**
- 出帧策略保持现状（零缓冲解码 + 最新帧呈现 + 1 帧延迟的 waitable 交换链）：P 帧链在解码阶段不会丢帧；只有到达抖动接近一个刷新周期时才会合并呈现，常见 Wi-Fi/公网下不可感知（已核实：`transport/timing.rs`、`windows_presenter/worker.rs`、`renderer.rs:205-216,668-678`）。只建议增加可选的“平滑”（观影）模式做音画同步，并删除写死的 playout-delay 扩展（改由接收端本地决定）。
- 轨池大小按被控屏幕数协商；不可见的轨道暂停编码；带宽按焦点加权。
- 主控用 1 Hz 的 `ReceiverStats` 回报解码 p95、丢帧、刷新率；TWCC 间隔明确设为 20–33 ms（现在因为 8.1 Mbps 的固定常量，实际恒为 50 ms）。
- SDR 改为全范围 BT.709/sRGB；SDR 10-bit 只在源本身是高位深时启用。
- 帧率允许超过 144；静态画面降到 5–10 fps 保活。

### 2.5 输入、光标与音频 v2

**A1（P0，协议形状要在发布前定，已核实）：键码。**
- 现状：协议只传 Windows VK（`protocol/input.rs:97-110`）；主控已经拿到的扫描码被丢弃（`windows_keyboard.rs:885`）；被控用自己前台窗口的布局反推扫描码。
- 后果：两端键盘布局不同时符号键出错，游戏按位置读取的 WASD 也会错；AltGr、死键只在布局相同时才正确；将来的非 Windows 被控必须另做映射。
- 方案：`Key` 增加 `usage`（USB HID 键盘页）和 `repeat`，VK 只作提示；HELLO 协商键盘模式：
  - 按位置：游戏默认；
  - 按字符：主控 `ToUnicodeEx` 后用 `Text` 注入；
  - 旧 VK 模式。
- 被控用 `KEYEVENTF_SCANCODE` 注入；HID 路径直接用 usage。

**A2：锁定键。**
- 现状：Caps/Num/Scroll 的按下被丢弃，只在松开时比较状态、补按一次（`lockkeysstatus` 语义）；CapsLock 无法被按住，用它做按键说话或重映射的软件都会失效。
- 方案：锁定键改为普通键；另外同步 `LockState{caps,num,scroll,seq}`。

**A3：卡键保护。**
- 现状：是否启用由服务器 `/config` 的 `keylock_optimize` 决定；超时固定 150 ms，Wi-Fi 抖动就会让游戏里按住的键被弹起。
- 方案：写进 FPP，不再读服务器配置；超时取 max(150 ms, 3×RTT + 抖动)；任何 `INPUT` 都算保活；删除移动端分支。

**A4：移动端输入接口。**
- 现状：`MouseClick`、`RelativeScaled`、`Text`、`Combination`、触摸、SimpleAction 快捷命令都没有发送方。
- 方案：删除，或冻结为保留编号。`RelativeScaled` 的算法疑似有误（0.1 屏宽约移动 6553 像素）。

**A5：光标。**
- 现状：整张 PNG 借 SystemStateChange 发送，没有缓存；被控上程序自己移动光标时，位置不会同步到主控。
- 方案：启用保留的 `CURSOR` 帧：
  - `Shape{hash, hotspot, png 仅首次发送}`，主控做 LRU 缓存；
  - `State{seq, shape, visible, screen, x, y, origin}`，事件驱动；位置变化若来自被控上的程序（而不是主控的输入），就驱动主控移动本地光标。

**A6–A9：**
- MOUSE_POLICY 由 JSON 改为 protobuf，带 `screen_id` 和 revision。
- 游戏白名单改为被控本地配置，并加入 ClipCursor 检测。
- `InputBatch{seq, sent_us, events[]}`，可选的“运动走不可靠子流 + 累计计数”，并在 HUD 显示输入延迟。
- Ctrl+Alt+Del 和锁屏改为显式的 `SystemCommand`，键流按原样注入（便于嵌套远程或虚拟机）。
- 触控和笔：后续实现。

**音频**

- **B1（P1）：** NetEq 在设备回调里插包，到达时间被回调周期量化；没有设置 `max_delay`。改为在网络线程插入、带接收时间，并设 120–200 ms 上限。
- **B2：**
  - 游戏模式加 `a=ptime:10`（编码器已支持），可省约 10 ms 延迟。
  - 高码率时 Opus 带内 FEC 大概率不起作用（需实测），桌面音频改用 RED 或音频 NACK。
  - `AUDIO_QUALITY` 扩展为 `AudioConfig`。
- **B3（P1）：**
  - 麦克风没有回声消除、降噪和自动增益，主控外放时被控那边会听到回声；且按立体声音乐模式 128 kbps 编码。
  - 方案：引入 WebRTC APM（AEC3/NS/AGC2），以本进程 NetEq 的输出作为参考信号；需要额外引入源码，见决策项。麦克风改用单声道 VOIP 32–48 kbps。
- **B4：**
  - 麦克风控制仍是 RPC 17 + SimpleAction 23–27 + 数值错误码；而且必须先开键鼠控制才能开麦克风。
  - 方案：改为 `AUDIO_CONTROL` 帧，麦克风权限与键鼠控制解耦。
- **B5：** 音画同步，用两路的发送端报告对齐，并在 HUD 显示偏差。

### 2.6 数据面：承载层、流控与统一传输引擎

**承载层（P0 级设计问题，已核实部分）**

- 剪贴板、文件、端口、拖放共用一个 SCTP association，所有通道都是有序可靠的；通道优先级只写进了 DCEP，SCTP 并不使用。
- 发送准入是一把锁加 2,000,000 B 的全局预算。调度只在消息边界切换流、不交错（`vendor/webrtc-sctp/src/queue/pending_queue.rs:13-24,127-140`）。端口消息 524,160 B、剪贴板文件块 512,000 B、文件块 204,800 B，大消息会拖住 session 通道（没协商 FCT 时 control 通道也一样）。
- 接收端在 `on_message` 里 await 业务队列；对端没有基于信用的流控。推断（需压测）：读循环停住后共享的 rwnd 被占满，所有通道都会停。
- 方案：
  - 按服务等级拆通道：`bulk`（文件、剪贴板大数据）、`tunnel`（端口 TCP）、`dgram`（无序、不重传，用于 UDP）。
  - bulk 子帧不超过 64 KiB；每个逻辑流配信用（`WINDOW_UPDATE`），数据真正写盘或写入 socket 后才归还。
  - `on_message` 只分发、绝不 await；单写者调度器按 control > session > tunnel > drag > bulk 出队。
  - 数据通道改为预协商 id（`negotiated: true`），省掉 DCEP，也消除“FCT 比通道打开先到”的竞态。

**统一传输引擎**

文件、剪贴板文件、拖放数据、诊断包共用一套“清单 + 流式 + 窗口 + 校验”的引擎。

**FILE v2（P1）**

- 现状：
  - 每个文件一次 FileAsk/Confirm，上传还要再等一次 Result：1 万个小文件在 RTT 50 ms 时约需 8–17 分钟。
  - 固定窗口 16×200 KB，约 3.2 MiB/RTT。
  - 没有内容哈希；续传只核对大小和秒级 mtime。
  - 空目录会丢；任何重解析点（如 OneDrive 占位文件）都会让整个任务失败。
- 方案：
  - 一次性发送清单（含目录、属性、纳秒 mtime、可选 BLAKE3）；接收端一次性回复所有续传偏移。
  - 连续流式发送，累积确认，窗口按带宽时延积自适应（上限 64 MiB）。
  - 文件末尾校验；可协商 zstd；重解析点逐项跳过并汇报。
  - 用单一 `files:3` 取代 `files/files_v2`。

**剪贴板 v2**

- 保留“格式清单 + 延迟渲染”模型，它与 RDP CLIPRDR 同构，是对的。
- 现状：
  - 文件内容停等，每个 RTT 约 500 KB。
  - 格式数据每 20 ms 一块 128 KiB，上限约 6.25 MiB/s，不压缩：一张 4K 截图（约 33 MB 的 DIB）至少要 5 s。
  - 对端回的确认被直接忽略。
- 方案：控制消息用 FPP 请求关联；超过 64 KiB 的数据走传输引擎；文本、DIB、HTML、RTF 用 zstd 压缩。
- **隐私（P1）：**
  - 遵守 `ExcludeClipboardContentFromMonitorProcessing` 等排除标记；
  - 远端写入的内容标记为不进剪贴板历史、不上传云剪贴板；
  - 提供按方向的开关和“仅文本”选项。

**拖放**

- 删除非原生路径：OleDrop、AutoSave、SendTarget 队列窗口、FILE_DROP 门控，约 1500 行。
- 理由：主控恒声明 `drag_drop`，所以非原生路径在 Farwing 之间基本不可达；而且它与原生路径的权限门控不一致。
- 原生拖放的会话状态机保留，数据面换成传输引擎。

**PORT v2（P0/P1）**

- 现状：
  - 缓冲满（单连接 8 MiB / 全局 32 MiB）就直接断开连接，慢客户端下载大文件会中途断。
  - 不支持 TCP 半关闭，一端 EOF 会丢掉另一方向还在途的数据。
  - 每 30 s 对目标发一次 `HEAD / HTTP/1.1` 探测，对 SSH、数据库等服务会产生误报和日志噪音。
  - 目标只能填 IP。
  - UDP 走可靠有序通道，超额时整个关联被杀。
- 方案：
  - `Open{stream, proto, host, port}`、`OpenResult`、`Data`、`Window`、`ShutdownWrite`、`Reset` 六种消息。
  - stream id 由发起方分配；两个方向各自结束。
  - UDP 走 `dgram` 通道，超额只丢单个数据报。
  - 默认只做 TCP 建连探测，HTTP 探测改为手动。

**诊断包**

32 KiB 停等改为窗口传输；单独设置一项许可（现在复用的是文件访问许可）。

### 2.7 连接建立与恢复

**首连（根治 B5）**

- 被控收到会话请求后先应答 SDP，媒体准备与 ICE 并行；或者先回 `session.preparing{eta}`。主控的时限从 ICE 开始计算。
- 编码能力探测结果持久化到 ProgramData，以适配器、驱动版本和 GPU 调度设置为键，失效时退回实时探测。

**三级恢复**

现状：

- 会话失败后最多立即重试 5 次，没有退避。
- 本机网络错误和 `device_offline` 都直接终止。
- ICE `Failed` 时两端都拆掉会话；没有睡眠唤醒和网络变化的监听。
- 重连时会关闭其余屏幕窗口（读代码得出，未实测）。
- 失败页只有“关闭”。

方案：

1. **会话内 ICE 重启：** `Disconnected` 持续 2 s 以上、或收到网络变化/唤醒事件时触发；被控在 `Failed` 后保留几秒等待重启。
2. **重建会话：** 指数退避；把 `device_offline` 和本机网络错误当作“等待设备”，由在线推送唤醒，而不是轮询。服务端需要配合：现在新的在线连接会结束该设备的全部会话（`server/src/hub.rs:439-468`）。
3. **界面：** 重连期间保留窗口布局，显示最后一帧和“正在重新连接”；失败页加“重试”。被控睡眠前主动下线，唤醒后立即重连。

**信令**

- 主控每次会话都新开一条 WSS（与在线连接分开），建立时多一次 TLS + WebSocket 握手。建议在在线连接上复用会话信令：信封已经带 `sid`。
- `network` 取值 0–3，自动切中继按单向阈值。建议改为命名枚举；切换依据为两条路径的实测 RTT/丢包（保留中继候选对作备份），而不是单向阈值。在带宽有限的自建 TURN 上，盲目切到中继可能更差。

**NAT 打洞**

- vendored webrtc-ice 内置了增强打洞协调器：固定魔数探测包、按 NAT 类型选择 10 种 plan、端口预测（`vendor/webrtc-ice/src/agent/punch*`、`allocator.rs`）。
- 它在 Farwing 之间能工作，但没有写进规范，而且依赖多个 STUN 服务器的部署（B6）。
- 两种选择：
  - **保留：** 作为 Farwing 机制写进规范；服务端提供两个 STUN 端点（内置 TURN 多监听一个端口或 IP），或实现 RFC 5780 行为探测；探测包带上由 ICE 凭据派生的令牌。
  - **去掉：** 回到标准 ICE + TURN 兜底。

### 2.8 服务端 API 清理（与 v2 一起冻结）

- **接口合并：**
  - `/devices` 与 `/devices/groups`（desktop/mobile/tv）合并，分组交给客户端；
  - 设备详情从中文标签对改为结构化对象；
  - 平台改为字符串枚举。
- **去掉多余字段：** 客户端里的 `controlled_support`、`support_wol`、`version_name`、`participants_info`；恒为 0 的 `update_started_at`；恒为 false 的 `router_wol`。
- **协助接口：** `assist_confirmation_required` 改为 `200 {"status":"pending_confirmation"}`，不再用错误信封加 HTTP 202。
- **connect_id：** 支持刷新；为临时协助提供一次性短号或邀请链接。
- **电源与唤醒：**
  - 结果接口校验上报者；推送中带上请求者。
  - 发起唤醒的设备若与目标同网，就在本地发魔术包；协助发包设备改为按局域网网段匹配，而不是公网 IP。
- **隐私：** 硬件指纹（system_id、machine_guid、MAC）和壁纸的自动上报改为可选，默认关闭。

---

## 3. 身份、信任与授权模型（产品目标的前提）

**现状**

- 同账号就是完全信任，协助是服务端代验的共享码。
- 每台被控同一时间只有一个会话：协助一律 `force`，会顶掉主人自己的会话。
- 权限是本机全局开关，与具体主控无关。
- “只看不控”只在主控界面实现：被控端注入输入时只检查租约（`host/input/backend.rs:55,109`）。
- 令牌是不轮换的 bearer 凭据，访问令牌永不过期。
- SDP 中的 DTLS 指纹没有与设备身份绑定：服务端或中继理论上可以做中间人，规范 §5 关于“端到端”的表述因此过强。
- 协助码以明文到达服务端；被控上报的是 `sha256(salt‖code)`，可以离线穷举。

**建议（L，路线图 P1）**

1. **设备密钥：** 每次安装生成 Ed25519 密钥，`devices/init` 登记公钥；请求签名（DPoP 风格）或换取短期令牌；访问令牌改为短期令牌加刷新令牌。设备密钥同时对 SDP 中的 DTLS 指纹签名（如 `a=x-farwing-identity`），双方校验。
2. **协助改用 PAKE：** 在实时面中转 CPace 或 SPAKE2，双方直接用协助码协商密钥并确认 DTLS 指纹；服务端只转发和限速；被控自己判定、自己计数失败次数。
3. **授权：**
   - 服务端数据模型：`grants(device, principal: user|device|group, caps, expires, granted_by)`；`session.incoming` 下发 `granted_caps`。
   - 能力：view / control / clipboard / files / ports / power / audio / annotation / diagnostics。
   - 被控与本机策略取交集，逐项在被控端强制执行，包括输入注入。
   - 新增 FPP `PERMISSIONS` 帧和实时 `session.permissions`，主人可以随时授予或收回控制权。
4. **多参与者：** 参与者 ID 已经存在；单一“控制令牌”仲裁（请求、批准、收回）；被控对多个 peer 复用编码输出。短期先做到：协助遇到已有会话时返回 busy 或请主人确认，结束原因按白名单透传。
5. **无服务器场景：**
   - 配对：一次性码或二维码 + PAKE，双方互存公钥。
   - 局域网发现：mDNS `_farwing._udp`。
   - 直连信令：被控在本机监听一个轻量信令端点，复用 `rtc.*` 消息。
   - 客户端支持多个服务器实例和多个账号。

---

## 4. 建议保留（成熟或已是 Farwing 自有）

- **服务端实时面骨架：** 单次入场券、resume/epoch、对中转消息逐条校验 sid/attempt/方向/大小、吊销联动；Argon2id 参数、令牌只存哈希、扩展未知键透传。
- **被控 Lease 所有权模型：** permission/media/stream 三层代次。它是 §3 授权逐项强制的天然落点。
- **被控进程拆分：** 服务 / 常驻 / 采集代理 / 输入代理 / 用户后端。这是 Windows 安全边界（安全桌面、用户令牌、UIAccess）决定的。还有 DPAPI 保险库，以及“全机组件只属于默认 profile”。
- **媒体：** goog_cc 核心、libwebrtc 接收管线移植（PacketBuffer、FrameBuffer、NACK、VCMTiming）、CM256 与二项分布函数、RTX、PacerV2 突发学习器（作为上限）、WindowAdmission、编码器后端与跨 GPU 传输、各编码格式的 RTP 封包、RTCP RTT 测量。
- **输入：**
  - `remote_input` 的所有权 / 释放检查点 / 代次 / 恢复；键盘钩子的双路排序；被控 SYSTEM 代理与 HID 租约。
  - 绝对坐标模型（屏幕 ID + 归一化）、Raw Input 相对鼠标、节流、高精度滚轮、FCT 输入字节预算与本地合并。
- **音频：** NetEq 接收、虚拟麦克风喂入、默认设备租约与恢复。
- **显示：** 显示 journal、恢复、兜底屏、headless 交接；“以实际屏幕状态为准”的原则；5 条预协商轨道（数量可协商）；按主控保存的被控布局偏好；DualCapability 编解码协商。
- **数据特性：**
  - 剪贴板的延迟渲染与回环防护；文件路径与句柄安全（safe_relative、重解析点检查、`.downloading` + rename）；文件 IO 在用户身份下执行。
  - 原生拖放的会话状态机；诊断包的脱敏、zstd、SHA-256；SCTP 中“整条消息预留后才分配 SSN”的不变量。
- **双端扩展：** 原生批注 v1、AUDIO_QUALITY、原生拖放、诊断、display_geometry。它们是新消息的样板，保留语义，只换承载方式。
- **插件 ABI、日志与诊断包结构。**

## 5. 需要你决策的问题

1. **协议升级方式：** 首次发布前一次性不兼容升级到 v2（推荐），还是 v1/v2 共存一个版本周期？
2. **服务端信任边界：** 只自用或家庭单管理员时，设备密钥、PAKE、指纹绑定可以延后；多人共用实例时应提前。
3. **多主控：** 是否支持多人同时观看或控制？近期要做的话，授权与 Ownership 设计要先留出参与者维度。
4. **协助遇到已有会话：** 保持强制顶替，还是返回 busy、请主人确认、或以“仅观看”身份加入？“仅密码”模式是否还允许确认请求（建议不允许）？
5. **键盘默认模式：** 按位置（游戏优先）还是按字符？是否保留旧 VK 模式？激活时是否同步锁定键状态？
6. **默认画质与延迟：** 原生分辨率优先，还是流畅优先？是否增加“办公 / 游戏”内容模式？是否需要可选的“平滑（观影）”模式？（零缓冲出帧保持为默认，见 §2.4 F8 的更正）
7. **视频投入：** 是否做按厂商的 RFI/LTR（需要 NVIDIA、AMD、Intel 三家测试机）？是否支持超过 144 fps 和超宽、5K 原生分辨率？FEC 冗余预算定多少？
8. **数据面：**
   - 删除非原生拖放和被控“发送到主控”队列窗口？
   - 端口目标只允许回环、白名单，还是任意内网地址？是否允许在被控侧解析主机名？
   - UDP 是否改为不可靠传输？
   - 队头阻塞用单 association + 调度（推荐）解决，还是给 bulk 单开一个 PeerConnection？
9. **剪贴板默认值：** 是否默认双向且包含文件？是否默认遵守密码管理器的排除标记、不进入剪贴板历史？
10. **被控离线或重启后：** 主控自动等待 2–5 分钟（推荐，可取消），还是立即失败？
11. **远端触发更新：** 实现服务器编排的更新，还是删除相关界面和能力？
12. **主控默认值：** 对自己账号下的设备，是否默认可控、默认开启剪贴板？
13. **麦克风回声消除：** 是否引入 WebRTC APM 源码（体积、构建时间、许可），还是先用系统通讯音效过渡？
14. **打洞：** 增强打洞是保留为 Farwing 机制（需要服务端提供第二个 STUN 端点），还是回到标准 ICE？

## 6. 建议的实施顺序

| 阶段 | 内容 | 规模 |
| --- | --- | --- |
| 0 | §1 的 B1–B8 加剪贴板等待超时；双机回归基线（键鼠、剪贴板、拖放、显示设置） | S，约 1–2 天 |
| 1 | v2 基线：能力统一（§2.1）、控制面请求/错误模型（§2.2）、被控按类型分发、删除不可达的非原生路径与多余字段（约 1500–2000 行）、服务端 API 清理（§2.8）、视频分辨率快修（§2.4 F1）与快速 PLI（F2 快修）、输入键码形状（A1）、`HostEvent` 与连接恢复（§2.7） | M–L |
| 2 | 数据面承载与流控、统一传输引擎、FILE/剪贴板/PORT v2（§2.6）；显示管理 v2（§2.3）；媒体协商三消息、头扩展精简、发送节奏与自适应（§2.4）；光标、锁定键、音频修正（§2.5） | L |
| 3 | 身份与授权（§3）：设备密钥与指纹绑定 → PAKE 协助 → grants 与 PERMISSIONS → 多参与者 → 局域网发现、配对与直连 | L，路线图 |

每个阶段结束时用 `dev` 命令和双机脚本回归，并更新 `docs/protocol/`。

---

## 7. 决策记录（2026-10-14，用户确认）

| # | 决策 |
| --- | --- |
| 1 | 直接不兼容升级到 Farwing Protocol v2，不考虑与 v1 共存 |
| 2 | 自己账号下已登录的设备，以及已添加的好友，可以直接全权访问；需要实现好友功能和好友权限管理 |
| 3 | 支持多人同时观看或控制同一台设备。新增原生的“主控光标显示”（带身份与样式）；删除批注里早期的“鼠标指示”工具 |
| 4 | 协助不顶替已有会话；无论多少人，都以相同方式加入；授权模式与并发无关 |
| 5 | 键盘按位置传键，不保留 VK 模式，同步锁定键状态 |
| 6 | 分辨率保持 720p / 1080p / 2K / 4K 档位逻辑，只修复竖屏（旋转后）缩放基准边错误；新增“观影模式”；新增“弱网策略”设置：带宽临时不足时由用户选择降码率或降帧率（不降分辨率，避免编码器重建造成卡顿）；不做办公/游戏模式 |
| 7 | RFI/LTR 放到最后单独作为一个课题；新增 240 fps 档；其他分辨率暂不考虑；FEC 冗余由实现方决定 |
| 8 | 删除非原生拖放与被控“发送到主控”队列窗口；端口目标允许任意地址并允许在被控侧解析主机名；UDP 改为不可靠传输；队头阻塞采用单 association + 调度方案 |
| 9 | 剪贴板默认双向且包含文件；遵守密码管理器的排除标记，不进入剪贴板历史 |
| 10 | 被控离线或重启后，主控自动等待 2–5 分钟，可取消 |
| 11 | 实现远端触发更新 |
| 12 | 自己账号下的设备默认可控、默认开启剪贴板 |
| 13 | 不引入 WebRTC APM（回声消除） |
| 14 | 打洞以提高成功率为目标设计 |

待补充的细节问题见对话记录。

### 7.1 细节决策（第二批，2026-10-14）

未单独列出的条目均按推荐方案执行。

**多人与光标**
- **键鼠：** 控制权先到先得，同一时刻只有一人开启键鼠控制，只由持有者释放（关闭控制、离开或被收回权限）；主人可以收回控制权或禁止某人控制。非持有者统一观看 1080p60 观影流（2026-10-16 由“谁操作谁短时独占”改为此方案，见 v2-design §4.2、§4.4）。
- **编码：** 同一屏幕、同一参数的参与者共用一路编码，参数不同时另开，上限 4 路编码。
- **权限与剪贴板：** “显示设置”是一项单独的权限，操作时通知其他参与者。剪贴板只在当前控制者与被控之间双向同步，其他人手动拉取。
- **麦克风：** 多路混音后送入被控虚拟麦克风。
- **人数：** 默认上限 8 人，可配置。
- **主控光标：** 在被控物理屏上叠加显示，同时由各参与者在本地绘制。只看不控的人也显示“指示光标”，并带点击反馈（左键蓝色、右键橙色）。批注中的“鼠标指示”工具与激光笔删除，由参与者光标代替。
- **光标样式：** 颜色按加入顺序分配，用户可改；标签为昵称，悬停显示设备名；空闲 3 s 后淡出；正在控制的人也带身份标签。

**好友与权限**
- 好友限同一服务器实例内的账号，用户名申请、对方确认后生效。
- 权限按“好友 × 设备”设置，提供“所有设备”快捷方式。**接受好友申请时选择给对方的权限（预选观看、声音、批注），申请方给出的默认也是这三项**；主人设定权限后，连接时无需再确认。
- 被控本机开关是上限；协助参与者使用被控设置里的权限模板；权限变更对进行中的会话立即生效。
- 本轮一起实现：设备密钥、DTLS 指纹绑定、协助 PAKE。

**画质**
- “自动”只在会话开始时按带宽估计选定档位，之后按弱网策略处理，不再自动切换分辨率。
- 弱网策略默认“降码率”：降帧率最低到 30 fps，降码率最低到档位上限的 25%；一种手段到底后再用另一种；按设备记住。
- 观影模式：主控端做缓冲、音画同步和按刷新率对齐出帧；被控端改为画质优先参数。手动开关，按设备记住。
- 16:10 屏在 2K 档输出 2304×1440 是预期行为，可以在此基础上改进缩放质量。
- **不做 240 fps。**

**取消的功能**
- **远端更新：** 删除远端触发更新的界面和能力。本机更新保留，被控在本机更新期间主控自动等待。

**打洞**
- 服务端支持多个 STUN 端口与多个公网 IP（用于 NAT 行为探测）。
- 激进打洞仅在必要时启用，可以关闭；启用 UPnP / NAT-PMP / PCP；优先 IPv6。

**范围**
- 局域网发现、IP/域名直连、免账号配对：在本轮其余工作完成后实施。
- 手机主控：预留触控和笔的帧类型。
- **麦克风：** 行为不变，只把消息迁移到 v2 格式。
- **版本号：** 0.1.0。

**测试与驱动**
- 允许在本机做键鼠注入测试；全部完成后提供双机手动测试清单。
- 驱动用自签名证书，本机已安装 WDK。

## 8. 实施计划

| 阶段 | 内容 |
| --- | --- |
| P0 | §1 的缺陷修复（B1–B8，以及剪贴板等待超时） |
| P1 v2 基线 | 协议 crate v2（HELLO 版本 2、能力按名称+版本、统一 Platform、FPP 请求/响应/错误、`HostEvent`、预留帧类型）；被控按类型分发；控制面类型化消息（取代 `PbControlMessage`）；删除不可达的非原生路径、远端更新和移动端输入；服务端 API 清理；版本号 0.1.0 |
| P2 身份与权限 | 设备密钥与请求签名、短期令牌；SDP 指纹签名；协助 PAKE；好友；grants；`PERMISSIONS`；被控端逐项强制 |
| P3 多人 | 服务端单设备多会话；被控多 peer、共享编码；输入仲裁；剪贴板与麦克风规则；参与者列表与踢出；原生主控光标；协助不顶替 |
| P4 数据面 | 通道按服务等级拆分、预协商、信用流控与调度；统一传输引擎；FILE、剪贴板（含隐私）、PORT、诊断 v2；删除非原生拖放 |
| P5 媒体与输入 | 竖屏缩放修复与 16:10 改进；自动档位固定；弱网策略；观影模式；快速 PLI；发送节奏；起始码率；头扩展精简；FEC 按帧；接收端统计；按位置传键、锁定键同步、按 RTT 自适应的保活、`SystemCommand`；`CURSOR` 帧；NetEq 插包时机 |
| P6 连接 | 首连（先应答、并行准备、探测结果缓存）；三级恢复与自动等待；会话复用在线连接；NAT 行为探测（多端口、多 IP）、打洞规划、UPnP/NAT-PMP/PCP、优先 IPv6；中继切换改为按实测 |
| P7 | 局域网发现、IP/域名直连、配对 |
| P8 | 驱动自签名构建与嵌入；交付双机测试清单 |
| 之后 | RFI/LTR 单独课题 |
