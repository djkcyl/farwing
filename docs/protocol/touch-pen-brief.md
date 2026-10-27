# 原生触摸与笔输入（TOUCH / PEN）设计简报

本文是把主控的**真实多点触摸和笔**注入 Windows 被控的设计依据。笔的属性包括压力、倾斜、旋转、橡皮擦、笔杆键和悬停。覆盖两种主控：

- 原生 Windows 客户端（`src/application/viewer`）；
- 浏览器（`server/web`）。

现状：

- 协议只保留了 TOUCH（0x0104）和 PEN（0x0105）两个类型号，没有帧体。
- 浏览器把触摸模拟成鼠标手势，笔走鼠标兼容事件。
- 原生客户端不转发触摸和笔。
- 被控没有任何触摸、笔注入代码。

阅读约定：

- 结论来自代码阅读，`文件:行` 对应 2026-10-17 的代码快照。代码与 `docs/` 不一致时以代码为准，差异汇总在 §7。
- Windows 与浏览器的行为来自外部资料：
  - 微软文档：InjectSyntheticPointerInput、CreateSyntheticPointerDevice、InjectTouchInput、InitializeTouchInjection、POINTER_PEN_INFO、POINTER_TOUCH_INFO 的参考页；
  - 公开实现：Chromium `remoting/host/touch_injector_win.cc`、Weylus PR #128、Vibepollo PR #34；
  - MDN 的 Pointer Events。

  标“需实测”的项目没有在任何机器上验证过。
- 路径前缀：
  - `winit-0.30.13`、`windows-0.62.2`、`egui-winit-0.36.1` 指 `D:\Rust\.cargo\registry\src\index.crates.io-1949cf8c6b5b557f\` 下的同名目录。
- 用语同 [web-control-brief.md](web-control-brief.md)（下称“控制简报”）：被控 = host；主控 = controller；“原生”指 Windows 客户端主控。
- 帧头：`46 57 02 <flags> <type u16 BE> <request_id u32 BE>`（`crates/protocol/src/peer/mod.rs:4-13`、`:217-254`）。
  - TOUCH 帧头：`46 57 02 00 01 04 00 00 00 00`；
  - PEN 帧头：`46 57 02 00 01 05 00 00 00 00`。

---

> 实施结果（2026-10-17）：第 1–7 步已实现——线格式（`src/protocol/pointer.rs`）、被控事件与校验、合成指针注入层（`src/platform/windows/input/pointer.rs`）、引擎集成与状态机（`src/features/host/input/pointers.rs`，含鼠标降级与回环保护）、`CURSOR_SUPPRESSED` 修法、HELLO 运行时声明、浏览器与原生主控的发送。原生主控通过消息钩子读取 `WM_POINTER*`（没有另装子类，`WM_POINTERCAPTURECHANGED` 不处理），观看窗口取消 `RegisterTouchWindow`；接触面积暂不发送。注入到真实窗口的效果尚未在本机验证，见 `docs/selftest-results.md` §11。第 10 步（HID 数字化仪）未做。

## 0. 结论速览

### 0.1 现在缺什么

| 环节 | 现状 | 位置 |
|---|---|---|
| 协议 | TOUCH/PEN 只有类型号。特性名 `touch`、`pen` 已定义，注释写“Reserved for mobile controllers” | `crates/protocol/src/peer/mod.rs:76-78`、`:90-93` |
| HELLO | 两端的特性表都是写死的列表，不含 `touch`/`pen` | `src/protocol/peer.rs:156-215`；测试断言不声明，`:322` |
| 被控入口 | control 通道上的 TOUCH 帧依次经过：输入模块不认（`Receiver::receive` 只解 INPUT）→ 不是 POINTER → 不是文件 → 只当 HELLO 解析 → 静默丢弃 | `src/features/host/peer/ingress.rs:44-96`，`src/features/host/input/mod.rs:229-240`，`src/features/stream_control/publisher.rs:380-383` |
| 被控注入 | 没有任何 `InjectTouchInput` 或合成指针代码。已开启的特性 `Win32_UI_Input_Pointer` 全仓库都没有用到 | `Cargo.toml:133` |
| HID 驱动 | 只有键盘、消费者键、相对鼠标三个集合，没有数字化仪 | `drivers/input/reports.h:10-27` |
| 原生主控 | 鼠标靠 winit 消息钩子观察原始输入；winit 的 `WindowEvent::Touch` 只交给 egui，不转发；winit 只取笔的压力 | `src/application/viewer/mod.rs:43-45`，`windows_mouse.rs:311-477`；winit 见 §3.1 |
| 浏览器 | `pointerType === 'touch'` 走鼠标手势；笔走鼠标兼容事件 | `server/web/src/ui/remoteInput.ts:76-103`、`:126-244` |

所以，要做的是**一条完整的新输入路径**：线格式、被控注入、两种主控的采集。

服务端不用改：帧在 DTLS 内端到端传输，授权仍然是 `control`。

### 0.2 推荐方案

1. **注入 API：`CreateSyntheticPointerDevice` + `InjectSyntheticPointerInput`**（Windows 10 1809 起）。
   - 建两个设备：PT_TOUCH 最多 10 点，PT_PEN 1 支。笔的压力、倾斜、旋转、橡皮擦、笔杆键和悬停都能注入。
   - 三个函数要用 `GetProcAddress` 动态取，否则旧系统上整个程序无法启动（§2.2）。
   - 不选 `InjectTouchInput`：它只能注入触摸，没有笔。
   - 不选 HID 数字化仪：要改驱动、处理显示器映射，驱动本身也还没有装机实测过（§2.7）。这条留作第三阶段的可选项，用于安全桌面。
2. **在现有输入线程里执行**。TOUCH/PEN 帧进入与 INPUT 相同的 `Receiver`、门控、64 条队列、代次、几何检查和 `Hub`/`Engine`。服务模式下，事件以 JSON 经输入代理送到 SYSTEM 会话代理执行（§1、§2.8）。
3. **线格式**（§5）：
   - TOUCH：一帧列出该参与者的**全部活动触点**，每个触点带自己的阶段（DOWN/MOVE/UP/CANCEL）。
   - PEN：一帧带 1–16 个按时间排序的样本。每个样本带阶段（HOVER/DOWN/MOVE/UP/LEAVE/CANCEL）、压力、倾斜、旋转和按钮。
   - 坐标与 `MouseAbsolute` 相同，是屏内归一化值；`screen_id` 必写；数值用 `float` 编码。
4. **协商与回退**：
   - 被控在运行时探测到上述 API，才声明 `touch: 1`、`pen: 1`。
   - 主控只在对方声明后发 TOUCH/PEN，否则沿用鼠标模拟。
   - 被控在运行时注入失败（例如安全桌面上不可用）时，自己把主触点或笔降级为鼠标（§5.4）。
5. **不留卡住的触点**（§5.6）：
   - 主控在有触点或笔接触时，每 50 ms 至少发一帧（可以是 INPUT 心跳）。
   - 被控每隔不超过 100 ms 重注入一次静止的触点，防止 Windows 自己取消。
   - 被控超过 `max(150 ms, 3×RTT+50 ms)` 收不到该参与者的输入，就取消全部触点。
   - 现有的所有“释放全部”时机也取消触点：换代、队列满、几何或桌面变化、交接控制权、出错。
6. **主控采集**（§3）：
   - 原生：
     - 给观看窗口加子类过程，读 `WM_POINTER*`；
     - 用 `GetPointerType`、`GetPointerFrameTouchInfo`、`GetPointerPenInfoHistory` 取完整数据；
     - 控制时调用 `UnregisterTouchWindow`，让触摸也走 `WM_POINTER`；
     - 过滤由触摸、笔提升出的鼠标输入。
   - 浏览器：Pointer Events 的 `pointerType`、`pressure`、`tiltX/tiltY`（或 `altitudeAngle/azimuthAngle`）、`twist`、`width/height`、`buttons`（笔杆键 2、橡皮擦 32）、`getCoalescedEvents()`。

### 0.3 一览（方案实施后）

| 功能 | 授权 | 被控 HELLO 特性 | 通道 | 帧 | 主控回退 |
|---|---|---|---|---|---|
| 多点触摸（≤10） | `control`，并持有控制权 | `touch` | control | TOUCH 0x0104 | 浏览器：现有鼠标手势；原生：主触点当左键 |
| 笔（压力、倾斜、旋转、橡皮擦、笔杆键、悬停） | 同上 | `pen` | control | PEN 0x0105 | 笔尖 = 左键，笔杆键 = 右键，悬停 = 移动 |
| 保活 | 同上 | 无 | control | INPUT Heartbeat `42 00` | — |

---

## 1. 现有 INPUT 的做法（TOUCH/PEN 的模板）

### 1.1 线格式、通道与大小

- 帧体是 `InputMessage{oneof kind}`。tag 1–8 依次为 Key、LockState、MouseAbsolute、MouseRelative、MouseButton、MouseWheel、SystemCommand、Heartbeat（`src/protocol/input.rs:15-117`）。字节示例见控制简报 §2.9。
- 只走 `farwing.control.v2`。被控只把 control 通道上、当前绑定代次的帧交给输入模块（`src/features/host/peer/channels.rs:242-249`，`ingress.rs:44-49`）。
- 被控不按输入处理大于 4 KiB 的帧（`src/features/host/input/wire.rs:8`，`mod.rs:229-232`）。TOUCH/PEN 也应遵守：10 个触点约 260 字节，16 个笔样本约 400 字节。
- 原生的发送规则：
  - 非释放事件发出前，先等 control 通道积压低于 `INPUT_SEND_BUDGET_BYTES`（1159 字节，`src/transport/fct.rs:29-31`、`:46-50`）。
  - 释放事件（键抬起、按钮抬起）不等（`src/transport/rtc/channels.rs:502-509`）。
  - 等待期间，连续的移动会被合并（`src/features/remote_input.rs:1082-1116`）。
- 浏览器：`bufferedAmount` 超过 1 KiB 时暂存移动（`server/web/src/lib/input.ts:190`、`:367-378`）。

### 1.2 被控门控（`src/features/host/input/mod.rs`）

| 检查 | 位置 | 说明 |
|---|---|---|
| 流绑定 | `:201-212`、`:259-261` | 只接受当前 control 通道（stream）上的帧；通道重建会换代 |
| 许可 | `:318-329` | 未取消、许可有效、有 `control` 授权、PeerConnection 已连接、本参与者持有控制权（`src/features/host/participants.rs:307`） |
| 故障态 | `:275-277` | `faulted` 期间丢弃；等旧代按住的状态释放后再恢复（`:107-112`、`:503-533`） |
| 持有者复核 | `:341-343`、`:402-405` | 输入线程每 200 ms 核对一次，持有者是否仍可控制（`participants.rs:387`） |
| 解析错误 | `:244-249` | 只记在被控本地状态（`src/features/host/access.rs:1785`），不回给主控 |

### 1.3 队列、批处理与几何

- **队列**：每位参与者一个待执行队列，上限 64 条。满了就作废本代、清空队列、释放全部按住的状态，之后只接受新输入（`mod.rs:291-311`）。
- **合并**：只合并相邻、同一屏、同一代的绝对移动，保留最后一个；其他事件都不合并。代码注释明确要求保留“drawing/contact paths”，不跨事件合并（`mod.rs:53-76`）。
- **批处理**：一次最多执行 16 个移动（`MAX_BATCH`，`:27`、`:91-106`）。服务代理也按这个上限校验批次长度（`src/features/host/input/broker.rs:260-263`）。
- **几何**：入队时记录屏幕几何，执行时几何不同就丢弃（`mod.rs:278-283`、`:623-629`）。屏幕几何变化或输入桌面切换时，引擎先释放全部状态（`src/features/host/input/engine.rs:245-262`）。

### 1.4 执行与交接

- 整个被控只有一个 `host-input` 线程和一个后端（`mod.rs:330-339`、`:353-369`、`:397-438`）。
- 换持有者时，`Hub::admit` 先对上一位调用 `release()`（`mod.rs:374-385`）。
- 每次执行前都检查门控（`mod.rs:655-662`）。
- 执行出错时释放全部（`:672-680`）。例如 `screen_id` 不存在，会报“输入目标屏幕已失效”（`engine.rs:60-65`）。
- 心跳只对已经持有输入的会话有效（`mod.rs:630-634`）。

### 1.5 保活与卡键

- **超时**：`max(150 ms, 3×RTT+50 ms)`，RTT 上限按 3 s 计（`mod.rs:36-44`）。每批执行时作为 `hold` 交给引擎（`:665-666`，`engine.rs:174-176`）。
- **超时只释放键盘**：`tick()` 超时后只调用 `neutral_keys`，而且只在 `keyboard_interrupts` 开启时才做（`engine.rs:673-688`、`:177-186`）。按住的鼠标键不会因超时释放，只在 `release()` 时释放（`:689-716`）。
- **心跳**：原生只在**有键按住**时发心跳（`src/features/remote_input.rs:490-503`）；浏览器在有键或鼠标键按住时发（`server/web/src/lib/input.ts:251-266`）。

所以**触点必须有自己的超时**，不能依赖 `keyboard_interrupts`。悬在半空的触点，Windows 会当作一根一直按着的手指。

### 1.6 注入后端

| 后端 | 何时使用 | 位置 |
|---|---|---|
| 本进程 `Engine`（普通用户，中完整性） | 没有被控服务 | `backend.rs:62-71`；界面标签“Windows · 当前用户”（`engine.rs:161-169`） |
| 服务常驻进程（resident） | 本进程就是服务拥有的常驻进程 | `backend.rs:46-60` |
| 输入代理 | GUI 被控经服务管道连到服务，服务在活动会话里用 SYSTEM 令牌启动代理进程 | `broker.rs:210-318`、`:320-398` |

- **事件格式**：代理收到的是 JSON 序列化的 `wire::Event`（`broker.rs:14-33`，测试见 `wire.rs:298-321`）。新增的 TOUCH/PEN 事件必须可序列化，并在代理里再次 `validate()`（`broker.rs:264-267`）。
- **代理存活**：代理 500 ms 收不到请求就退出（`broker.rs:247`）；主进程每 100 ms 发一次 `Alive` 维持它（`:173-178`）。代理空闲时调用 `engine.tick()`（`:248-251`）。所以重注入可以放在 `Engine::tick` 里，本地和服务两条路径都会执行。
- **按键与鼠标的注入路线**：
  - 走 HID 报告，需同时满足：装了 Farwing HID 驱动、处于普通桌面、前台不是发起控制的观看窗口（`engine.rs:236-244`、`:263-279`）。
  - 否则走 `SendInput`，带标记 `dwExtraInfo = "FWUI"`（`src/platform/windows/input/system.rs:10-15`、`:88-102`）。
  - HID 设备只有 SYSTEM 能打开（`drivers/input/FarwingInput.inf:29`），并且在安全桌面上关闭（`engine.rs:257-259`）。安全桌面由服务的系统路径（`SendInput`）负责。
- **桌面切换**：引擎每次执行都用 `OpenInputDesktop` 比较桌面名。变化时先释放，再 `SetThreadDesktop`（`system.rs:146-203`，`engine.rs:251-262`）。
- **DPI 与几何**：输入线程设为 Per-Monitor V2（`system.rs:125-129`），程序清单也是 PerMonitorV2（`assets/windows.manifest:8-9`）。几何来自 DXGI 的 `DesktopCoordinates`（`src/platform/windows/capture.rs:113`），都是物理像素。
- **回环保护**：本机控制本机时，注入的输入不会被观看窗口再次转发。
  - 观看窗口忽略带 `FWUI` 标记的原始输入和窗口消息（`src/application/viewer/windows_mouse.rs:400-405`、`:479-482`）。
  - 前台是发起进程时，HID 路径退回 `SendInput`（`engine.rs:236-244`）。

### 1.7 TOUCH/PEN 必须复制的机制

| 机制 | INPUT 的实现 | TOUCH/PEN 的做法 |
|---|---|---|
| 通道与代次 | control 通道 + `bind/generation` | 同一个 `Receiver::receive`，按帧类型分支 |
| 许可 | `Shared::permitted()` | 同一个函数，不另设开关 |
| 队列上限 | 64 条，满了换代并释放 | 与 INPUT 共用这 64 条；换代时取消触点 |
| 合并 | 只合并绝对移动 | 被控不合并触点或笔样本，保留轨迹；只把纯移动的帧计入 16 条批次 |
| 几何 | 入队时快照，执行时比对 | 同上；几何变化时释放，即取消触点。Windows 在 `WM_DISPLAYCHANGE` 时也会取消全部注入触点（§2.4） |
| 交接 | `Hub::admit` → `release()` | `release()` 同时取消触点和笔 |
| 出错 | 释放全部 | 同上。**解析失败的 TOUCH/PEN 帧要转换为“取消全部”事件**，不能像 INPUT 那样只丢弃：丢掉一个 UP 就会留下卡住的触点 |
| 保活 | 有键按住时 50 ms 心跳，被控超时释放 | 触点或笔接触期间同样 50 ms 心跳；被控无条件超时取消 |
| 服务模式 | `wire::Event` 的 JSON 经两段管道 | 新事件同样走 JSON，批次 ≤16 |
| 桌面切换 | 先释放再切换；HID 在安全桌面关闭 | 先取消触点、销毁合成设备，在新桌面上重建（§2.6） |
| 回环 | `FWUI` 标记 | `POINTER_INFO` 没有 `dwExtraInfo` 字段，打不了标记；改为检查落点窗口所属的进程（§2.8） |

---

## 2. Windows 被控端的注入

### 2.1 方案对比

| 方案 | 最低系统 | 类型与点数 | 笔属性 | 备注 |
|---|---|---|---|---|
| `InitializeTouchInjection` + `InjectTouchInput` | Windows 8 | 只有触摸，最多 `MAX_TOUCH_COUNT` = 256 | 无 | 每个进程一个注入上下文；Chromium 远程桌面用它 |
| **`CreateSyntheticPointerDevice` + `InjectSyntheticPointerInput`** | **Windows 10 1809** | PT_TOUCH ≤256（建议 10）；PT_PEN 必须为 1 | 压力 0–1024、旋转 0–359、倾斜 −90…90、`PEN_FLAG_BARREL/INVERTED/ERASER`、悬停 | 每个设备一个句柄，触摸设备和笔设备可以同时存在。**推荐** |
| WinRT `InputInjector`（`Windows.UI.Input.Preview.Injection`） | 1809（笔） | 触摸、笔、鼠标、键盘 | 同上 | 要引入 WinRT 投影和新的 crate 特性；按 UWP 受限能力设计；底层能力与上一行相同。不推荐 |
| HID 数字化仪（Farwing 驱动经 VHF 上报） | 驱动 INF 要求 19041（`FarwingInput.inf:11-12`） | 由驱动描述符决定 | 由描述符决定 | 系统当作真实硬件；但要改驱动并映射显示器，见 §2.7 |

微软文档要点：

- **`InjectSyntheticPointerInput(device, POINTER_TYPE_INFO[], count)`**：类型必须与创建设备时一致。PT_TOUCH 时 `0 < count ≤ MAX_TOUCH_COUNT`；PT_PEN 时 `count` 必须为 1。
- **`CreateSyntheticPointerDevice(type, maxCount, mode)`**：`mode` 是接触可视化方式，可取：
  - `POINTER_FEEDBACK_DEFAULT`（1）：跟随用户的“笔和触控”设置；
  - `POINTER_FEEDBACK_INDIRECT`（2）：覆盖用户设置；
  - `POINTER_FEEDBACK_NONE`（3）：不显示。
- **InjectTouchInput 的说明**。合成指针的文档没有展开这些规则，本文按同一套处理：
  - 注入只送到注入进程所在会话的桌面（“The injected input is sent to the desktop of the session where the injection process is running.”）。
  - `contacts` 表示桌面上的**全部**触点。
  - 按住手势要持续发帧，否则输入会被取消。
  - `WM_DISPLAYCHANGE` 会取消全部注入触点。
  - UP 帧的位置必须与上一个 UPDATE 帧相同。否则返回 `ERROR_INVALID_PARAMETER`，并取消全部触点。
  - 两次调用间隔小于 0.1 ms 时可能返回 `ERROR_NOT_READY`，需要重发同一帧。
  - 自定义时间戳（`dwTime` 或 `PerformanceCount`）一旦使用，就必须一直用到所有触点都 UP，并且不能比当前时钟新。

### 2.2 windows-rs 现状与加载方式

- **已开启的特性**：`Win32_UI_Input_Pointer` + `Win32_UI_WindowsAndMessaging`（`Cargo.toml:133`、`:145`）。它们已经提供以下 API（`windows-0.62.2/src/Windows/Win32/UI/Input/Pointer/mod.rs:1-160`）：
  - `InjectTouchInput`、`InitializeTouchInjection`；
  - `POINTER_TOUCH_INFO`、`POINTER_PEN_INFO`、`POINTER_INFO`；
  - `GetPointerType`、`GetPointerFrameTouchInfo(History)`、`GetPointerPenInfo(History)`、`SkipPointerFrameMessages`、`EnableMouseInPointer` 等。
- **合成指针需要 `Win32_UI_Controls` 特性，目前没有开启**：
  - `CreateSyntheticPointerDevice`：`UI/Controls/mod.rs:105-108`；
  - `DestroySyntheticPointerDevice`：`:297-298`；
  - `HSYNTHETICPOINTERDEVICE`：`:3309`；
  - `POINTER_TYPE_INFO`：`:7617-7619`；
  - `InjectSyntheticPointerInput` 本身在 `Input/Pointer/mod.rs:139-142`，但同样要求 `Win32_UI_Controls`。
- **不要静态链接**。windows-rs 用 `link!` 在加载时绑定 user32 的导出；1809 之前的系统没有这些导出，进程会直接起不来。
  - 做法：用 `GetProcAddress(user32, "CreateSyntheticPointerDevice")` 等方式取这三个函数。仓库里已有同样的写法，见 `SendSAS`（`src/platform/windows/host_service/service.rs:26-31`）。
  - `POINTER_TYPE_INFO` 可以本地定义：`#[repr(C)] struct { r#type: POINTER_INPUT_TYPE, u: union { touch: POINTER_TOUCH_INFO, pen: POINTER_PEN_INFO } }`。这样不必开启庞大的 `Win32_UI_Controls`。
- **`POINTER_INFO` 没有 `dwExtraInfo` 字段**（`Input/Pointer/mod.rs:282-299`）。注入的指针不能像 `SendInput` 那样带 `FWUI` 标记。
- **常量**（取自 `windows-0.62.2`）：

| 名称 | 值 | 名称 | 值 |
|---|---|---|---|
| `PT_TOUCH` / `PT_PEN` | 2 / 3 | `POINTER_FLAG_INRANGE` | 0x2 |
| `POINTER_FLAG_INCONTACT` | 0x4 | `POINTER_FLAG_FIRSTBUTTON` / `SECONDBUTTON` | 0x10 / 0x20 |
| `POINTER_FLAG_PRIMARY` | 0x2000 | `POINTER_FLAG_CANCELED` | 0x8000 |
| `POINTER_FLAG_DOWN` / `UPDATE` / `UP` | 0x10000 / 0x20000 / 0x40000 | `MAX_TOUCH_COUNT` | 256 |
| `PEN_FLAG_BARREL` / `INVERTED` / `ERASER` | 1 / 2 / 4 | `PEN_MASK_PRESSURE` / `ROTATION` / `TILT_X` / `TILT_Y` | 1 / 2 / 4 / 8 |
| `TOUCH_MASK_CONTACTAREA` / `ORIENTATION` / `PRESSURE` | 1 / 2 / 4 | `POINTER_FEEDBACK_DEFAULT` / `INDIRECT` / `NONE` | 1 / 2 / 3 |

- **结构字段的取值**（`POINTER_TOUCH_INFO`、`POINTER_PEN_INFO` 参考页）：
  - 触摸：`pressure` 0–1024，未报告时 Windows 取 512；`orientation` 0–359；`rcContact` 是接触区域的屏幕像素矩形，未报告时为 0×0。
  - 笔：`pressure` 0–1024；`rotation` 0–359，顺时针；`tiltX` 右倾为正；`tiltY` 朝向用户为正。

### 2.3 每帧的标志位

**触摸**：每次调用列出所有活动触点。

| 线上阶段（§5） | `pointerFlags` |
|---|---|
| DOWN | `INRANGE \| INCONTACT \| DOWN` |
| MOVE（含本帧没动的触点） | `INRANGE \| INCONTACT \| UPDATE` |
| UP | `UP`，位置等于上一帧 |
| CANCEL | `UP \| CANCELED` |

- 不做触摸悬停。Chromium 也不做。
- 第一个按下的触点加 `POINTER_FLAG_PRIMARY`。是否必须由注入方设置，需实测。

**笔**：每次调用 1 个样本。

| 线上阶段 | `pointerFlags` | `penFlags` |
|---|---|---|
| HOVER | `INRANGE \| UPDATE` | 按住笔杆键 → `BARREL`；橡皮擦端朝下 → `INVERTED` |
| DOWN | `INRANGE \| INCONTACT \| DOWN`（建议加 `FIRSTBUTTON`，需实测） | 同上；橡皮擦端接触时再加 `ERASER` |
| MOVE | `INRANGE \| INCONTACT \| UPDATE`（同上） | 同上 |
| UP | `INRANGE \| UP` | 同上 |
| LEAVE | `UPDATE`，不带 `INRANGE` | 0 |
| CANCEL | 接触中：`INRANGE \| UP \| CANCELED`，然后再发一次 LEAVE | — |

- `penMask` 固定为 `PRESSURE | ROTATION | TILT_X | TILT_Y`。主控不知道的值发 0，与 Windows 的“未报告”默认值相同。
- 按住笔杆键时，是否还要设 `SECONDBUTTON` 和对应的 `ButtonChangeType`，要用真实的笔对照 `WM_POINTER` 消息确认（需实测）。

### 2.4 补帧与超时

- 文档只说“按住要持续发帧，否则输入会被取消”，没有给出时长（InjectTouchInput 参考页）。
- 公开实现：
  - Chromium 远程桌面观察到的超时“约 1 秒”，按 100 ms 的间隔重注入全部活动触点（`kKeepAliveInterval`）。它的注释还说，多点手势（例如捏合）不重注入就不能用。
- **建议**：
  - 有活动触点（或笔处于接触、悬停），且距上次注入已 ≥100 ms 时，`Engine::tick()` 把当前帧原样再注入一次，全部用 UPDATE。
  - 笔悬停也补帧。是否必要需实测；补帧本身不改变状态。
- **`ERROR_NOT_READY`**：重试 3 次。
  - 注意 `thread::sleep(1 ms)` 在 Windows 上可能睡 1–15 ms。
  - 笔的合并样本要依次注入时，用自旋等待 ≥0.1 ms，或者只注入最后几个样本。
- **其他失败**（尤其 `ERROR_INVALID_PARAMETER`）：Windows 已经取消了全部触点。被控清空自己的触点表，不重放。
- **v1 不使用自定义时间戳**：规则太严（必须单调、不能超前、一旦使用就要用到全部 UP），网络抖动下难以保证。线上的 `time_us`/`age_us` 先只用于诊断（§5）。

### 2.5 坐标、DPI 与多屏

- **映射与鼠标相同**（`engine.rs:619-635`）：

  ```
  px = clamp(round(left + width × x), left, right − 1)
  ```

  y 同理。`left/top/width/height` 来自 DXGI，单位是物理像素（`capture.rs:113`）。注入线程已经是 PMv2（`system.rs:125-129`）。
- **`rcContact`**：以 `(px, py)` 为中心，宽 = `width × 屏宽` 像素，高同理，并钳在屏内。触点的方向写入 `orientation`。
- **坐标原点有两种说法，需实测**：
  - `InjectSyntheticPointerInput` 文档说 ptPixelLocation “specified relative to top left of the virtual screen”。Weylus 据此减去虚拟屏的左上角。
  - Vibepollo（Apollo/Sunshine 的分支，2026-10-11）的修复正好相反：改回普通屏幕坐标（加回 `SM_XVIRTUALSCREEN`）后，副屏在主屏左侧或上方时，点击位置才正确。
  - 单屏，或副屏在主屏右侧、下方时，两种算法结果相同。
  - 实现时把换算集中在一个函数里，用“在主屏左侧或上方放一块屏”的布局实测后定下来（§8）。
- **精度**：只能注入整数像素（`ptPixelLocation`），亚像素会丢掉。在高分屏上对笔画影响很小。
- **屏幕旋转、分辨率变化**：由几何快照覆盖，几何一变就取消触点。

### 2.6 权限、会话与安全桌面

| 场景 | 预期 | 依据或状态 |
|---|---|---|
| 普通用户模式（中完整性，`uiAccess=false`，`assets/windows.manifest:15`） | 往管理员权限窗口注入触摸或笔时被 UIPI 拦截，与 `SendInput` 相同 | 需实测 |
| 服务模式（常驻进程或会话代理，SYSTEM 令牌，在活动会话里运行） | 可以注入任何完整性级别的窗口 | `broker.rs:210-236`；需实测 |
| Session 0 里的服务本身 | 不能注入：注入只送到注入进程所在会话的桌面 | Farwing 不在 session 0 注入 |
| UAC 安全桌面、锁屏（Winlogon 桌面） | 引擎会切换线程桌面；合成设备是否跟随桌面、是否要在新桌面上重建，**未知** | 需实测。建议切换桌面时先取消触点、销毁设备，到新桌面上用到时再重建；失败就走被控内部的鼠标降级（§5.4） |
| Ctrl+Alt+Del | 与触摸无关，仍然用 `SystemCommand SAS` | 控制简报 §2.8 |
| 合成设备存在本身 | 设备存在期间，Windows 或应用是否改变行为（例如认为有触摸屏、弹出触摸键盘），未知 | 需实测。建议收到第一帧时才创建；空闲 30 s、会话结束或桌面切换时销毁 |

### 2.7 Farwing HID 驱动能否做触摸屏或笔

**驱动现状**：

- 描述符只有键盘（ID 1）、消费者键（ID 6）、相对鼠标（ID 5）三个集合（`drivers/input/reports.h:10-27`）。
- `ValidReport` 只认这三种报告（`driver.c:65-72`）；`Neutral` 也只复位这三种（`:53-60`）。
- 没有特性报告回调：`VHF_CONFIG_INIT` 只传了描述符（`:168-169`）。
- 长度上限：IOCTL 输入不超过 284 字节（`:77`），用户态报告不超过 256 字节（`src/platform/windows/input/hid.rs:96`）。
- 看门狗：500 ms 收不到心跳就复位（`driver.c:116-124`）。
- 驱动给自己分配了独立的容器 ID，避免被当作笔记本的触控板（`driver.c:170-175`）。

**要支持触摸和笔，需要改这些**：

- 新增数字化仪的顶层集合：
  - 触摸屏：Usage Page 0x0D / Usage 0x04。每指一个 Finger 集合，含 Tip Switch、Contact Identifier、X/Y 绝对坐标，可选 Width/Height/Pressure；另有 Contact Count，以及特性报告 Contact Count Maximum。
  - 笔：0x0D / 0x02，含 In Range、Tip Switch、Barrel Switch、Invert、Eraser、X/Y、Tip Pressure、X/Y Tilt、Twist。
- 实现 `EvtVhfAsyncOperationGetFeature`，回答特性报告请求。Windows 可能还要求别的特性报告（例如认证状态），需实测。
- 扩展 `ValidReport` 和 `Neutral`：复位时要抬起全部触点，并让笔离开感应范围。
- 升级 `OU_VERSION` 和 `hid.rs` 的 `VERSION`（`reports.h:4`，`hid.rs:16-17`）。

**优点**：

- 系统把它当作真实硬件：
  - 不受 UIPI 限制；
  - 笔与触摸的仲裁、防误触由 Windows 处理；
  - 读 HID 数字化仪的应用也能收到。
- 理论上在安全桌面、锁屏上与真实触摸屏一样可用。但目前引擎在安全桌面上会关闭 HID（`engine.rs:257-259`），是否放开要单独评估。

**缺点**：

- 一个数字化仪只映射到一块显示器（在“平板电脑设置 → 设置”里配置）。坐标是这块显示器内的逻辑值，多屏时要按 `screen_id` 重新映射，或者每块屏一个集合。
- 驱动只有 SYSTEM 能打开（INF `:29`），所以只在服务模式下可用。
- 驱动“尚未在任何机器上安装和实测”（`drivers/README.md:56`），而且用的是自签名证书。

**结论**：第一、二阶段用合成指针。HID 数字化仪作为第三阶段的可选项，用来解决两类需求：安全桌面和锁屏上的真实触摸；只认硬件的应用。

### 2.8 推荐的被控结构

1. **新模块 `src/platform/windows/input/pointer.rs`**：
   - 动态加载三个函数，本地定义 `POINTER_TYPE_INFO`。
   - `TouchDevice`：`CreateSyntheticPointerDevice(PT_TOUCH, 10, POINTER_FEEDBACK_DEFAULT)`。
   - `PenDevice`：`CreateSyntheticPointerDevice(PT_PEN, 1, POINTER_FEEDBACK_DEFAULT)`。
   - 可视化选 `DEFAULT`，尊重被控本机的设置。也可以提供 `INDIRECT` 选项，强制显示触点圈，方便远端用户确认触点落在哪里。
   - 只有控制权持有者会注入，所以一个笔设备就够了。
   - 注入时重试 `ERROR_NOT_READY`；任何失败都清空状态；`Drop` 时先取消全部触点，再销毁设备。
2. **`Engine`（`engine.rs`）**：
   - 新增状态：
     - 触点表 `BTreeMap<wire_id, Slot{windows_id 0..=9, last_px, flags, rc, pressure, orientation}>`；
     - 笔状态（离开、悬停、接触）和最后一个样本；
     - `last_pointer_inject`、`last_pointer_input`。
   - Windows 指针 id 取 0–9 中空闲的值。微软的示例从 0 开始，用 0–9 最稳妥。
   - `apply_inner` 新增 `Event::Touch`、`Event::Pen` 两个分支：
     1. 查屏幕。缺省或不存在都算出错，释放全部。
     2. 换算成像素。
     3. 按 §5.2 的规则更新触点表，组出 Windows 帧。
     4. 如果 UP 的位置与上一帧不同，先补一帧 UPDATE，再发 UP。
     5. 注入。
   - `tick()`：
     - 100 ms 补帧（§2.4）；
     - 距 `max(last_heartbeat, last_input)` 超过 `threshold` 时，取消全部触点和笔。这一步**不看** `keyboard_interrupts`。
   - `release()`：触点全部取消（`UP|CANCELED`）；笔先 UP，再 LEAVE。
   - `synchronize()`：在桌面切换路径（`engine.rs:251-262`）里，`release()` 之后销毁设备。
3. **回环保护**：
   - 注入前用 `WindowFromPoint(px, py)` 取落点窗口所属的进程。
   - 如果是 `origin`（发起控制的观看进程），这一帧改走带 `FWUI` 标记的鼠标降级：主触点 → 绝对移动 + 左键。思路与 `hid_allowed` 相同（`engine.rs:236-244`）。
   - `origin` 的来源：本进程见 `backend.rs:55`、`:67`；代理里来自 Hello（`broker.rs:233-236`）。
   - 浏览器与被控在同一台机器上时无法防回环，只能靠测试布局规避（§6）。
4. **被控内部降级**：设备不可用（创建失败、当前桌面不支持、系统太旧）时：
   - 触摸：主触点 → `MouseAbsolute` + 左键，其余触点丢弃；
   - 笔：接触 → 左键，笔杆键 → 右键，悬停 → 移动；
   - 用 `input_status` 记一次原因（`src/features/host/access.rs:1785`）。
5. **光标状态**：`src/platform/windows/cursor_shape.rs:85` 把 `CURSOR_SUPPRESSED` 当作隐藏，建议改为“保持上一个形状”。原因见 §5.7。

---

## 3. 主控端的输入来源

### 3.1 原生主控现状

**鼠标的采集方式**：

- 事件循环注册了 `with_msg_hook(desktop_input_message)`（`src/ui/windows.rs:25`；播放器窗口见 `src/application/viewer/windows_presenter/connection.rs:74-77`）。钩子先交给键盘处理，再交给 `RawRouter::message`（`src/application/viewer/mod.rs:43-45`）。
- `RawRouter` 只观察、不消费消息（`windows_mouse.rs:311-312`）：
  - 绝对模式：位置取 `msg.pt`，经 `Target::position` 换算（`:429-433`、`:450-453`）；
  - 相对模式：取 `lLastX/Y`（`:454-456`）；
  - 按键：取 `usButtonFlags`（`:408-412`、`:457-473`）；
  - 滚轮：来自 winit 的 `MouseWheel`。
- `Target::position` 已经处理了 egui 浮层、窗口缩放边框、视频旋转、画面矩形和屏幕边缘的钳位（`:114-166`）。触摸和笔可以直接复用。

**winit 0.30.13 在 Windows 上的行为**：

- 有就绪的数字化仪时，创建每个窗口都会调用 `RegisterTouchWindow(hwnd, TWF_WANTPALM)`（`winit-0.30.13/src/platform_impl/windows/window.rs:1129-1135`）。
  - 结果：**触摸到达的是 `WM_TOUCH`，不是 `WM_POINTER`**。
  - `TWF_WANTPALM` 还关掉了掌触抑制。
- `WM_TOUCH` → `WindowEvent::Touch`，`force: None`（`event_loop.rs:1938-1983`，`:1974`）。不检查 `TOUCHEVENTF_PEN/PALM/INRANGE`。
- `WM_POINTERDOWN/UPDATE/UP`：所有指针类型都变成 `WindowEvent::Touch`（`:1985-2130`）。
  - 只读取 `GetPointerTouchInfo` / `GetPointerPenInfo` 的 `pressure`，换算为 `Force::Normalized(p/1024)`；压力为 0 时变成 `None`（`:2071-2100`、`:994-999`）。
  - **丢掉的数据**：倾斜、旋转、`penFlags`（笔杆键、橡皮擦）、接触区域与方向。
  - 阶段只看 `DOWN/UP/UPDATE`（`:2109-2118`），所以**悬停与接触移动分不开**，也不会产生 `Cancelled`。
  - `device_id` 是常量（`platform_impl/windows/mod.rs:86`）。
  - 处理完返回 0，不调用 `DefWindowProc`（`:2127-2129`），所以笔不会被提升为鼠标消息。
- winit 不识别提升鼠标的签名（`GetMessageExtraInfo` 的 0xFF515700），也不注册数字化仪的原始输入。
- `with_msg_hook` 在 `TranslateMessage/DispatchMessageW` 之前拿到投递（post）的消息（`event_loop.rs:408-420`），看不到发送（send）的消息，例如 `WM_POINTERCAPTURECHANGED`。

**egui-winit 0.36.1**：`WindowEvent::Touch` → `egui::Event::Touch`，同时把其中一个触点模拟成鼠标左键（`egui-winit-0.36.1/src/lib.rs:368-382`、`:900-935`）。所以本地的 egui 界面现在能用触摸操作，但画面里的触摸不会转发。

**现在触摸原生画面会发生什么：未知，需在触屏机上实测。**

- Windows 对 `WM_TOUCH` 窗口仍会生成提升的鼠标输入。
- 如果原始输入里也有这些鼠标报告（SDL 的代码按“有”处理），`RawRouter` 会把它们当作物理鼠标，转发成绝对移动和左键。原因是 `ulExtraInformation` 只和 `FWUI` 比较（`windows_mouse.rs:400-401`），不看 0xFF5157xx 签名。

### 3.2 原生的建议做法

1. **让触摸也走 `WM_POINTER`**：开启触摸转发时，对观看窗口调用 `UnregisterTouchWindow`（需要新特性 `Win32_UI_Input_Touch`，见 `windows-0.62.2/.../UI/Input/Touch/mod.rs:47`）。之后触摸和笔都走 `WM_POINTER*`，winit 仍会把它们转成 `Touch` 交给 egui。
2. **用子类过程读完整数据**：参照光标的 `SetWindowSubclass`（`windows_cursor.rs:126-129`），给观看窗口加一个 `pointer_proc`。子类能收到投递和发送的消息，读完后照常调用 `DefSubclassProc`，不影响 winit。
   - `WM_POINTERDOWN/UPDATE/UP`：
     - 先用 `GetPointerType(id)` 区分类型；
     - 触摸：用 `GetPointerFrameTouchInfo(id)` 取整帧，包括 id、位置、`rcContact`、`orientation`、`pressure`、`pointerFlags`；
     - 笔：用 `GetPointerPenInfoHistory(id, historyCount)` 取合并的样本。返回的样本新的在前，要倒成旧的在前，最多 16 个。
   - 阶段判断：

     | `pointerFlags` | 阶段 |
     |---|---|
     | `INCONTACT` 从无到有 | DOWN |
     | 有 `INRANGE`，无 `INCONTACT` | HOVER |
     | `UP` | UP |
     | `UPDATE`，且没有 `INRANGE` | LEAVE |
     | `CANCELED` | CANCEL |

   - 出现 `WM_POINTERCAPTURECHANGED`、`WM_POINTERLEAVE`、窗口失焦或退出控制时，对应的触点或笔 → CANCEL。
3. **坐标与属性**：
   - 位置：`ptPixelLocation` 经 `Target::position(point, dragging = 有触点按下)` 得到 `(screen, x, y)`。
   - 接触区域：`rcContact` 的宽高除以画面矩形的宽高，得到归一化的 `width/height`。
   - 压力：`pressure / 1024`。倾斜与旋转原样发送。
   - 按钮：`penFlags & BARREL` → buttons bit0；`penFlags & (INVERTED|ERASER)` → bit1。
   - 在画面外按下的触点不转发；已按下的触点拖出画面时钳在边缘，与鼠标一致（`windows_mouse.rs:156-164`）。
4. **过滤提升的鼠标**：
   - `RawRouter` 在 `(ulExtraInformation & 0xFFFFFF00) == 0xFF515700` 时忽略这条原始鼠标。bit 0x80 为 1 表示来自触摸，为 0 表示来自笔。
   - `host_message` 同样用 `GetMessageExtraInfo()` 判断。
   - 只在触摸或笔转发开启时过滤。被控不支持触摸时，这些提升的鼠标正好是现成的回退。
5. **发送**：
   - `RemoteInput` 增加 `InputEvent::Touch/Pen`，`encode()` 输出 TOUCH/PEN 帧（`src/features/remote_input.rs:171-215`）。
   - 含 UP/CANCEL/LEAVE 的帧归为“释放”，不等发送预算（`src/transport/rtc/channels.rs:502`）。
   - 纯 MOVE 的触摸帧可以被更新的帧替换，与绝对移动同理（`remote_input.rs:1082-1116`）。笔样本则**追加**到待发帧里（≤16 个），不丢。
   - 心跳条件加上“有活动触点或笔接触”（`remote_input.rs:490-503`）。
   - 只转发开启控制之后才按下的触点，与键盘“等本地全部松开”的规则一致（`remote_input.rs:368-386`）。
6. **全屏时的边缘手势**：主控本机 Windows 的边缘滑动会打开系统界面。全屏控制时，可以用 `SHGetPropertyStoreForWindow` 设置 `PKEY_EdgeGesture_DisableTouchWhenFullscreen` 来禁止。`Win32_UI_Shell_PropertiesSystem` 已开启（`Cargo.toml:144`），属性键需要本地定义。
7. **不要用 `EnableMouseInPointer`**。它对整个进程生效，会把鼠标也变成 `WM_POINTER`，打乱现有的原始输入路径。

### 3.3 浏览器 Pointer Events

| 属性 | 含义 | 用法 |
|---|---|---|
| `pointerType` | `mouse` / `touch` / `pen` | 分流 |
| `pointerId` | 每个触点或笔唯一 | 映射到线上 id 1–255，UP/CANCEL 后回收 |
| `pressure` | 0–1。不支持压力的硬件在接触时为 0.5，否则为 0 | 触摸：0.5 可以照发（Windows 的默认值就是 512）；笔：原样 |
| `tiltX` / `tiltY` | −90…90 度，右倾、朝向用户为正；不支持时为 0 | 方向与 Windows 相同 |
| `altitudeAngle` / `azimuthAngle` | 弧度；笔垂直于屏幕时 altitude = π/2 | 只有这两个值时换算：`tiltX = atan(cos(az) / tan(alt))`，`tiltY = atan(sin(az) / tan(alt))`，再转为度 |
| `twist` | 0–359，顺时针 | → `twist` |
| `width` / `height` | 接触几何，CSS 像素，默认 1 | 除以画面框的 CSS 宽高得到归一化值；≤1 px 视为未报告 |
| `buttons` | 1 接触，2 笔杆键，32 橡皮擦；`button == 5` 表示橡皮擦状态变化 | bit 2 → 笔杆键，bit 32 → 橡皮擦 |
| `getCoalescedEvents()` | 一次 `pointermove` 内合并的中间样本 | 有就用，作为笔样本 |
| `pointercancel` | 浏览器接管了手势、防误触等 | → CANCEL |
| `pointerleave` / `pointerout`（笔） | 离开感应范围或元素 | → LEAVE |

- **保持 `touch-action: none`**。它已经设置（`server/web/src/styles.css:1323-1328`），去掉后浏览器会自行滚动、缩放，并发出 `pointercancel`。
- **笔要在 `pointerdown` 上调用 `preventDefault()`**，阻止兼容鼠标事件。现在笔会经 `mousedown` 进入鼠标路径（`remoteInput.ts:95-103`），不阻止就会两条路径重复发送。
- **笔悬停**：硬件支持时，悬停表现为 `buttons == 0`、`pressure == 0` 的 `pointermove`。浏览器通常只在橡皮擦接触时报 `buttons & 32`，悬停时一般分不出橡皮擦端，所以悬停样本的 bit1 可能为 0。被控在 DOWN 时补上 `INVERTED|ERASER` 即可。
- **触摸点数**：本地超过 10 个触点时，多出的不发送。
- **坐标**：沿用 `position(clientX, clientY, clampOutside)`（`remoteInput.ts:44-55`），触点按下后钳在边缘。屏幕 id 取 `map.current.screenId`。
- **浏览器差异**：Firefox 和 Safari 对 tilt、twist、altitude、合并事件和悬停的支持与 Chromium 不同，一律用特性检测，见 §6。

### 3.4 统一映射

| 线上字段（§5） | 原生来源 | 浏览器来源 |
|---|---|---|
| `screen_id` | `Target::position` 返回的屏幕（即 `mouse_screen(track)`） | `PictureMap.screenId` |
| `x` / `y` | `ptPixelLocation` → `Target::position` | `clientX/Y` → `position()` |
| 触点 `id` | `pointerId` → 1–255 | `pointerId` → 1–255 |
| 触摸 `pressure` | `POINTER_TOUCH_INFO.pressure / 1024`（有 `TOUCH_MASK_PRESSURE` 时） | `pressure`，0 视为未报告 |
| `width/height` | `rcContact` ÷ 画面矩形 | `width/height` ÷ 画面框 |
| `orientation` | `POINTER_TOUCH_INFO.orientation` | 无，发 0 |
| 笔 `pressure` | `POINTER_PEN_INFO.pressure / 1024` | `pressure` |
| `tilt_x/tilt_y` | `tiltX/tiltY` | `tiltX/tiltY`，或由 altitude/azimuth 换算 |
| `twist` | `rotation` | `twist` |
| `buttons` bit0 / bit1 | `PEN_FLAG_BARREL` / `INVERTED\|ERASER` | `buttons & 2` / `buttons & 32` |
| 阶段 | `pointerFlags` 的变化（§3.2） | `pointerdown/move/up/cancel/leave` 与 `buttons` |
| `age_us` | 历史样本 `PerformanceCount` 的差 | 合并事件 `timeStamp` 的差 |

---

## 4. Farwing 文档的现有说法

| 文档 | 内容 |
|---|---|
| `docs/protocol/v2-design.md:241-242` | TOUCH 0x0104、PEN 0x0105，通道 control，预留 |
| `docs/protocol/v2-design.md:297` | HELLO 特性：“预留：`touch`、`pen`” |
| `docs/protocol/v2-design.md:361`、`:384` | 删除 v1 的 `InputEvent`（SESSION 内触摸）和移动端动作；“TOUCH / PEN 帧类型保留未实现” |
| `docs/protocol/farwing-protocol.md:578-579`、`:617` | 帧表中这两项都是“—/预留”；`touch`、`pen` 已定义名称，但当前不声明 |
| `docs/protocol/peer-messages.md:727` | “`TOUCH`（0x0104）与 `PEN`（0x0105）尚未定义帧体。” |
| `docs/improvement-proposals.md:222-224`、`:237`、`:510` | A4：移动端接口（含触摸）没有发送方，删除或冻结；“触控和笔：后续实现”；“手机主控：预留触控和笔的帧类型” |
| `docs/protocol/web-control-brief.md:48`、`:768-783` | 触摸未实现，建议浏览器用鼠标模拟（现已实现） |
| `docs/web-console.md:10`、`:157` | 网页“支持触屏”，指的是鼠标手势 |

---

## 5. 线格式提案

### 5.1 设计要点

- **单独的帧类型**：沿用预留的 TOUCH、PEN，不放进 INPUT 的 oneof。
  - 两种做法的兼容性相同：旧被控收到 TOUCH/PEN 会静默丢弃（§0.1）；收到 INPUT 里不认识的 oneof，也会当作空事件丢弃（`wire.rs:100-101`）。
  - 单独的类型号让文档更清晰，也给将来的移动端主控留出空间。
  - 被控在 `Receiver::receive` 里按帧类型分支，进入同一个门控和队列，与 INPUT 保持先后顺序。
- **坐标**：与 `MouseAbsolute` 一样是屏内归一化值 [0,1]；接触区域的宽高也按屏幕归一化，不随分辨率变化。`screen_id` 用 proto3 `optional`，并且**总是写出**。
  - 数值用 `float`（4 字节）而不是 `double`。在 [0,1] 内 float32 的精度约 6×10⁻⁸，乘以 16384 像素也只有 0.001 像素。
  - 浏览器的 `Writer.float` 已经有了（`server/web/src/lib/pb.ts:66-73`）。
- **TOUCH 一帧 = 一个 Windows 帧**：列出全部活动触点，每个触点带自己的阶段（整帧只有一个阶段时，多指各自起落的信息不够）。被控可以直接组出 `InjectSyntheticPointerInput` 需要的帧。
- **PEN 一帧 = 1–16 个样本**：保留 120–240 Hz 的笔的中间样本，同时把帧率压到画面帧率附近。被控队列只有 64 条，批次 16 条。
- **0 表示“未报告”**：除 `screen_id` 外都不用 `optional`。这样浏览器编码器不必新增“值为 0 也写出”的 float 或 sint。
- **时间**：`time_us` 是主控单调时钟的微秒数，uint32，会回绕，只有差值有意义。v1 的被控只把它用于诊断（HUD 上的输入延迟、抖动），注入时不用（§2.4）。

### 5.2 TOUCH（0x0104）

```proto
// control 通道，C→H 推送（rid 0）
message Touch {
  optional int32 screen_id = 1;     // 必写；屏 0 也写 08 00
  repeated Contact contacts = 2;    // 本参与者此刻的全部活动触点 + 本帧结束的触点；0..=10 个
  uint32 time_us = 3;               // 可选：主控单调时钟（µs，回绕）
}
message Contact {
  uint32 id = 1;            // 1..=255，主控分配；UP/CANCEL 之后可复用
  TouchPhase phase = 2;
  float x = 3;              // 屏内归一化 [0,1]
  float y = 4;
  float pressure = 5;       // (0,1]；0 = 未报告（Windows 默认 512）
  float width = 6;          // 接触区域宽，按屏宽归一化；与 height 都为 0 = 未报告
  float height = 7;         // 按屏高归一化
  uint32 orientation = 8;   // 0..359，0 = 沿 x 轴，顺时针增加
}
enum TouchPhase { TOUCH_PHASE_UNSPECIFIED = 0; DOWN = 1; MOVE = 2; UP = 3; CANCEL = 4; }
```

规则：

1. `contacts` 为空，表示**取消全部**触点。
2. 被控表中有、本帧里没有的触点，按 CANCEL 处理。
3. 本帧没动的活动触点也要列出，阶段为 MOVE，位置不变。
4. UP 带触点的最终位置。如果与上一帧的位置不同，被控先补一帧 MOVE。
5. 一帧只对应一块屏。触点分布在不同的观看窗口（不同的屏）时，以第一个按下的触点所在的屏为准；在其他窗口里新按下的触点，等全部触点抬起之前不发送。
6. 发送时机：任一触点有变化（按下、移动、抬起、取消）就发一帧。触点静止时靠心跳保活（§5.6）。
7. 被控的处理：

   | 情况 | 处理 |
   |---|---|
   | DOWN，该 id 不在活动中，有空槽 | 分配 Windows id 0–9，注入 `DOWN` |
   | DOWN，该 id 已在活动中 | 先取消旧触点，再按新触点处理 |
   | DOWN，已有 10 个活动触点 | 忽略这个触点，并记住它的 id，之后它的 MOVE/UP 也忽略 |
   | MOVE/UP/CANCEL，id 未知 | 忽略。这多半是已经被取消的触点 |
   | 校验失败（见 §5.5） | 整帧作废，并**取消全部触点** |

### 5.3 PEN（0x0105）

```proto
// control 通道，C→H 推送（rid 0）
message Pen {
  optional int32 screen_id = 1;     // 必写
  repeated PenSample samples = 2;   // 1..=16，按时间先后；最后一个是当前状态
  uint32 time_us = 3;               // 可选：最后一个样本的主控时间（µs，回绕）
}
message PenSample {
  PenPhase phase = 1;
  float x = 2;                // 屏内归一化 [0,1]
  float y = 3;
  float pressure = 4;         // [0,1]；接触时有效，0 也合法
  sint32 tilt_x = 5;          // −90..90 度，右倾为正
  sint32 tilt_y = 6;          // −90..90 度，朝向用户为正
  uint32 twist = 7;           // 0..359 度，顺时针
  uint32 buttons = 8;         // bit0 (1) 笔杆键；bit1 (2) 橡皮擦端
  uint32 age_us = 9;          // 本样本早于 time_us 的微秒数；最后一个样本为 0
}
enum PenPhase { PEN_PHASE_UNSPECIFIED = 0; HOVER = 1; DOWN = 2; MOVE = 3; UP = 4; LEAVE = 5; CANCEL = 6; }
```

被控的笔状态机（每个参与者一支笔）：

| 当前状态 | 收到的阶段 → 新状态 | 其他阶段的处理 |
|---|---|---|
| 离开 | HOVER → 悬停；DOWN → 接触（允许直接落笔，没有悬停能力的硬件就是这样） | 忽略 |
| 悬停 | HOVER → 悬停；DOWN → 接触；LEAVE → 离开 | MOVE 当作 HOVER；UP 忽略 |
| 接触 | MOVE → 接触；UP → 悬停；CANCEL → 离开（先 `UP\|CANCELED`，再 LEAVE） | DOWN 当作 MOVE；HOVER 先补一个 UP；LEAVE 先补一个 UP |

- `buttons` 在被控的换算见 §2.3：bit0 → `PEN_FLAG_BARREL`；bit1 → `PEN_FLAG_INVERTED`，接触时再加 `PEN_FLAG_ERASER`。
- 合并样本依次注入，每两次之间间隔 ≥0.1 ms（§2.4）。

### 5.4 HELLO 协商与回退

| 一方 | 规则 |
|---|---|
| 被控 | 运行时能用 `GetProcAddress` 取到三个合成指针函数，才在 HELLO 里声明 `touch: 1`、`pen: 1`。`host_features()` 现在是写死的列表（`src/protocol/peer.rs:187-215`），要改成运行时生成。“只声明运行时具备的”这条规则见 `crates/protocol/src/peer/mod.rs:42-43` |
| 主控 | 实现了发送就声明 `touch: 1`、`pen: 1`。这是软件能力，不看本机有没有触屏。被控**不按**主控的声明门控，与 `input.*` 相同 |
| 主控发送 | 对方声明了 `touch` 才发 TOUCH，声明了 `pen` 才发 PEN。原生用 `state.peer.has(feature::TOUCH)` 判断（同类用法见 `src/features/stream_control/input.rs:343`）；浏览器看 `hostHello.features.touch`（`server/web/src/lib/viewer.ts:701-704`） |
| 主控回退 | 对方没声明 `touch`：浏览器沿用现有手势（`remoteInput.ts:126-244`），原生把主触点当作绝对移动 + 左键。没声明 `pen`：笔当鼠标用，笔尖 = 左键，笔杆键 = 右键，悬停 = 移动 |
| 被控运行时降级 | 设备不可用时转成鼠标（§2.8 第 4 点），主控无感知 |
| 用户开关 | 浏览器和原生都提供“触控方式：原生触摸 / 鼠标模拟”。推荐默认值：被控支持时用原生触摸；笔总是用原生。原生触摸下，双指手势交给被控，本地缩放画面要改用工具栏 |

### 5.5 被控校验（`wire.rs` 新增；失败 → 取消全部）

| 项目 | 规则 |
|---|---|
| 帧大小 | ≤4 KiB，与 INPUT 共用 `MAX_MESSAGE` |
| `screen_id` | 必须存在。执行时屏幕不存在算出错，释放全部（与 `Geometry::screen` 相同，但不接受缺省） |
| 触点 | 每帧 ≤10 个；`id` 在 1..=255 之间且帧内唯一；`phase` 在 1..=4 之间 |
| 笔样本 | 每帧 1..=16 个；`phase` 在 1..=6 之间 |
| 坐标 | 有限值，且 ∈ [0,1] |
| `pressure`、`width`、`height` | 有限值，且 ∈ [0,1] |
| `orientation`、`twist` | <360 |
| `tilt_x`、`tilt_y` | −90..=90 |
| `buttons` | 只允许 bit0、bit1（`& !3 == 0`） |

### 5.6 不留卡住的触点

| 触发 | 谁做 | 动作 |
|---|---|---|
| 有活动触点或笔接触，且 50 ms 内没发过任何 control 帧 | 主控 | 发 INPUT `Heartbeat`（`42 00`）。每 15 ms 检查一次，与键盘相同 |
| 浏览器 `pointercancel`；原生 `WM_POINTERCAPTURECHANGED` | 主控 | 该触点 CANCEL，或笔 CANCEL |
| 失焦、`visibilitychange`、关闭控制、控制权丢失 | 主控 | 发一帧 `contacts` 为空的 TOUCH，或笔 CANCEL；再走现有的统一释放 |
| 有活动状态，距上次注入 ≥100 ms | 被控 `tick()` | 原样重注入（§2.4） |
| 超过 `max(150 ms, 3×RTT+50 ms)` 没收到持有者的任何输入帧 | 被控 `tick()` | 取消全部触点；笔先 UP 再 LEAVE。不看 `keyboard_interrupts` |
| 换代、队列满、几何变化、桌面切换、交接、许可失效、通道关闭、执行出错 | 被控 `release()` | 取消全部，复用 §1.2–§1.4 的全部路径 |
| TOUCH/PEN 解析失败 | 被控 | 转成“取消全部”事件入队，仍受门控 |
| `WM_DISPLAYCHANGE` | Windows | 自动取消全部注入触点。被控也会因为几何变化清空触点表 |

### 5.7 与鼠标、控制权的关系

- **控制权**：与 INPUT 完全相同，只有持有者的 TOUCH/PEN 会被注入。
  - 交接时，`Hub::admit` 里的 `release()` 会取消上一位的触点（`mod.rs:374-385`）。
  - 没有控制权的参与者，本地触摸只用来缩放或平移画面，不发送。
- **与鼠标并存**：同一参与者可以交替使用鼠标与触摸、笔。
  - 被控分别保存鼠标按键和触点的状态，`release()` 两者都释放。
  - 同一个物理触点不能既发 TOUCH 又发鼠标模拟。
- **光标隐藏会触发智能相对模式（必须修）**：
  - 注入触摸后，Windows 会隐藏鼠标光标（`CURSORINFO.flags = CURSOR_SUPPRESSED`）。
  - 被控用 `flags == CURSOR_SHOWING` 判断光标是否可见（`src/platform/windows/cursor_shape.rs:85`），于是向主控报“隐藏”（`src/features/host/peer/cursor.rs:61-64`，`src/features/remote_cursor.rs:209-213`）。
  - 原生主控的“智能”鼠标模式在被控光标隐藏时，会切到相对模式（`src/features/stream_control/input.rs:390-401`）。
  - 结果：用户摸一下屏幕，之后的鼠标就变成了相对模式。
  - 修法：被控把 `CURSOR_SUPPRESSED` 当作“保持上一个形状”，不报隐藏。浏览器在被控光标隐藏时只显示普通箭头（`server/web/src/ui/ViewerScreen.tsx:44-47`），不受影响。
- **系统光标**：笔会移动系统光标（悬停时显示笔光标），触摸不会。被控的光标采样照常进行。
- **批注模式**：开启批注时，触摸和笔用于本地画笔，不发 TOUCH/PEN，与鼠标相同。
- **拖放**：原生拖放占用指针时（`mod.rs:191-197`、`:256-258`），被控只丢弃鼠标移动。TOUCH/PEN 不要算作“移动”，否则会丢掉触点的中间帧。建议开始拖放时直接取消触点。

### 5.8 主控的发送规则

- **TOUCH**：
  - DOWN、UP、CANCEL 帧立即发送，发送前先发出暂存的移动帧。
  - 纯 MOVE 帧在通道积压时只保留最新的一帧。积压的判断：浏览器看 `bufferedAmount > 1 KiB`，原生用 `INPUT_SEND_BUDGET_BYTES`。
- **PEN**：
  - 样本追加到待发帧里。满 16 个、阶段变为 MOVE/HOVER 以外的值、或者通道空闲时，把这一帧发出。
  - 通道一直积压时，丢掉最旧的 MOVE/HOVER 样本，保留最新的 16 个。DOWN/UP/LEAVE/CANCEL 不丢。
- **心跳**：有活动触点或笔接触时，与键盘共用 15 ms 检查、50 ms 发送的心跳。浏览器 `input.ts:251-266` 的 `holding` 要把触点算进去；原生见 §3.2。
- **浏览器编码**：
  - 需要的 `Writer.intAlways(1, screenId)`、`.message(2, …)`、`.float(...)`、`.sint(...)` 都已存在（`pb.ts:51-73`、`:106-110`）。
  - `fpp.ts` 的 `Type` 加 `TOUCH: 0x0104`、`PEN: 0x0105`（`server/web/src/lib/fpp.ts:21-37`）。
  - `VIEWER_FEATURES` 加 `touch`、`pen`（`:135-146`）。

### 5.9 字节示例

**TOUCH**：帧头 `46 57 02 00 01 04 00 00 00 00`。

- float：0.25 = `00 00 80 3e`，0.5 = `00 00 00 3f`，0.75 = `00 00 40 3f`，0.01 = `0a d7 23 3c`，0.02 = `0a d7 a3 3c`。
- Contact 的字段键：x = `1d`，y = `25`，pressure = `2d`，width = `35`，height = `3d`，orientation = `40`。

| 事件 | 帧体 |
|---|---|
| 单指按下（屏 0，id 1，x 0.5，y 0.25） | `08 00 12 0e 08 01 10 01 1d 00 00 00 3f 25 00 00 80 3e` |
| 1 号移到 x 0.75；2 号按下（x 0.25，y 0.5） | `08 00 12 0e 08 01 10 02 1d 00 00 40 3f 25 00 00 80 3e 12 0e 08 02 10 01 1d 00 00 80 3e 25 00 00 00 3f` |
| 1 号原地抬起；2 号不动 | `08 00 12 0e 08 01 10 03 1d 00 00 40 3f 25 00 00 80 3e 12 0e 08 02 10 02 1d 00 00 80 3e 25 00 00 00 3f` |
| 2 号抬起 | `08 00 12 0e 08 02 10 03 1d 00 00 80 3e 25 00 00 00 3f` |
| 取消全部（屏 0） | `08 00` |
| 屏 1，3 号按下（x/y 0.5，压力 0.5，宽 0.01、高 0.02，方向 90°），time_us 1 000 000 | `08 01 12 1f 08 03 10 01 1d 00 00 00 3f 25 00 00 00 3f 2d 00 00 00 3f 35 0a d7 23 3c 3d 0a d7 a3 3c 40 5a 18 c0 84 3d` |

**PEN**：帧头 `46 57 02 00 01 05 00 00 00 00`。

- PenSample 的字段键：x = `15`，y = `1d`，pressure = `25`，tilt_x = `28`，tilt_y = `30`，twist = `38`，buttons = `40`，age_us = `48`。
- 倾斜 30° 的 zigzag 是 60（`3c`），−15° 是 29（`1d`）。

| 事件 | 帧体 |
|---|---|
| 悬停（屏 0，x/y 0.5，倾斜 x 30°、y −15°） | `08 00 12 10 08 01 15 00 00 00 3f 1d 00 00 00 3f 28 3c 30 1d` |
| 落笔（压力 0.25，倾斜同上） | `08 00 12 15 08 02 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 80 3e 28 3c 30 1d` |
| 一帧两个移动样本（第一个早 4167 µs，压力 0.5；第二个 x 0.75，压力 0.75） | `08 00 12 14 08 03 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 00 3f 48 c7 20 12 11 08 03 15 00 00 40 3f 1d 00 00 00 3f 25 00 00 40 3f` |
| 抬笔，回到悬停（x 0.75） | `08 00 12 0c 08 04 15 00 00 40 3f 1d 00 00 00 3f` |
| 离开感应范围 | `08 00 12 02 08 05` |
| 橡皮擦端接触（buttons 2，压力 0.5） | `08 00 12 13 08 02 15 00 00 00 3f 1d 00 00 00 3f 25 00 00 00 3f 40 02` |
| 悬停时按住笔杆键（buttons 1） | `08 00 12 0e 08 01 15 00 00 00 3f 1d 00 00 00 3f 40 01` |
| 取消 | `08 00 12 02 08 06` |

**其他**：

| 消息 | 字节 |
|---|---|
| 触点按住期间的心跳（INPUT） | `46 57 02 00 01 01 00 00 00 00` + `42 00` |
| HELLO `features` 中的 `touch: 1` | `2a 09 0a 05 74 6f 75 63 68 10 01` |
| HELLO `features` 中的 `pen: 1` | `2a 07 0a 03 70 65 6e 10 01` |

对 TOUCH 表第二行（两指那一帧），被控组出的 Windows 帧是：

```
[{id 0, INRANGE|INCONTACT|UPDATE, (0.75·W, 0.25·H)},
 {id 1, INRANGE|INCONTACT|DOWN,   (0.25·W, 0.5·H)}]
```

只调用一次 `InjectSyntheticPointerInput(touch, …, 2)`。

---

## 6. 风险

| 风险 | 说明 | 绕过办法 |
|---|---|---|
| 合成坐标的原点 | 文档说“相对虚拟屏左上角”，实践中两种结论都有（§2.5） | 换算集中在一处；用“主屏左侧或上方有副屏”的布局实测后定下来 |
| 安全桌面、锁屏、UAC | 合成指针能否在 Winlogon 桌面上注入、设备是否要重建，都未知 | 切换桌面时取消触点、销毁并重建设备；失败就降级为鼠标（服务模式下 `SendInput` 能操作安全桌面）。将来可以考虑 HID 数字化仪 |
| UIPI | 普通用户模式下注入不到管理员窗口 | 与键鼠相同，提示安装被控服务 |
| Windows 自动取消 | 静止的触点约 1 s 后被取消（Chromium 的观察值） | 被控 100 ms 补帧；主控 50 ms 心跳 |
| 卡住的触点 | 漏掉一个 UP，被控上就留下一根“一直按着的手指”，之后的触摸手势全部异常 | §5.6 的全部取消路径；解析失败也取消 |
| 被控端的边缘手势 | 注入的滑动如果从屏幕边缘开始，可能打开通知中心、小组件或任务视图。拖出画面时触点钳在边缘，更容易触发 | 需实测；必要时把钳位改为离边缘 1–2 像素，或在界面上提示 |
| 主控端的边缘手势 | 主控本机的边缘滑动、笔按钮快捷方式会被本机系统截走 | 原生全屏时设置 `PKEY_EdgeGesture_DisableTouchWhenFullscreen`；浏览器做不到，只能靠全屏减少 |
| 防误触 | 合成触摸没有防误触；winit 用 `TWF_WANTPALM` 关掉了掌触抑制 | 原生改走 `WM_POINTER`；笔在感应范围内或接触时，主控忽略触摸（“笔优先”）；可选：过滤宽高很大的触点 |
| 触摸键盘 | 用触摸点被控上的文本框，Windows 可能弹出被控的触摸键盘 | 对手机主控可能正好有用；需实测，必要时在被控设置里说明 |
| 合成设备的副作用 | 设备存在期间，系统或应用可能认为本机有触摸屏 | 收到第一帧时才创建，空闲 30 s 后销毁 |
| 应用不认合成指针 | 走 Wintab 的绘图软件（Photoshop、Krita、CLIP STUDIO 的 Wintab 模式）、读 HID 数字化仪的程序都收不到；按设备类型过滤的应用可能忽略 | 文档里提示改用 Windows Ink 模式；有需要时再做 HID 数字化仪 |
| 智能鼠标模式被触摸打乱 | 触摸让被控光标进入 `CURSOR_SUPPRESSED`，原生的智能模式随之切到相对模式 | §5.7 的修法 |
| 原生现状不明 | 触摸原生画面时，提升出的原始鼠标可能已经被当作鼠标转发 | 在触屏机上实测；按 §3.2 过滤 0xFF5157xx |
| 浏览器差异 | Chromium 的笔属性最全；Firefox 对 tilt、twist、悬停、合并事件的支持不同；Safari（iPad + Apple Pencil）有 altitude、azimuth，较新的机型支持悬停；iOS 长按会弹出系统菜单 | 一律特性检测；没有 tilt 时由 altitude、azimuth 换算；设置 `-webkit-touch-callout: none`；阻止 `contextmenu` |
| 浏览器的笔重复发送 | 笔的兼容鼠标事件会同时进入鼠标路径 | 在 `pointerdown` 上 `preventDefault()`；鼠标路径跳过 `pointerType === 'pen'` |
| 队头阻塞与延迟 | 浏览器走 SCTP，丢包时后面的笔样本都要等重传 | 样本合并成帧，积压时丢最旧的 MOVE；HUD 用 `time_us` 显示延迟 |
| 服务模式的开销 | 每批事件要经过两段管道、两次 JSON 编解码（`broker.rs:45-73`、`:365-390`） | 批次 ≤16；笔样本合并成帧 |
| `ERROR_NOT_READY` | 两次注入间隔小于 0.1 ms | 自旋等待后重试，最多 3 次 |
| 本机自测的回环 | 本机控制本机时，注入如果落在观看窗口上，会被再次采集、再次转发 | 原生：按落点进程降级为带标记的鼠标（§2.8）。浏览器：测试时把被测窗口放在画面映射不到浏览器的位置，或者用另一台设备当主控 |

---

## 7. 文档与代码的出入（以代码为准）

| 文档说法 | 代码事实 |
|---|---|
| `improvement-proposals.md:95`：被控的 control 入口“依次试 input→touch→files→draw→display→receive” | 现在的顺序是 input → POINTER → files → HELLO 颜色 → `publisher::receive`，已经没有 touch 解码器（`src/features/host/peer/ingress.rs:44-96`） |
| `migration-status.md:112`：“键鼠注入、触摸……”需要双机测试 | Farwing 没有任何触摸注入代码（v2 已删除，`v2-design.md:384`），这一行里的“触摸”已过时 |
| `v2-design.md:241-242`：TOUCH、PEN 的通道为 control | 代码没有为它们绑定任何通道或处理函数。control 通道上的 TOUCH/PEN 落到 HELLO 解析后被静默丢弃（`ingress.rs:79`，`publisher.rs:380-383`）。`farwing-protocol.md:578-579` 写的“—”与代码一致 |
| `crates/protocol/src/peer/mod.rs:76` 的注释：`touch` “Reserved for mobile controllers” | 本方案的主要使用者是原生 Windows 主控和浏览器，注释应改为通用说明 |
| `v2-design.md:297`、`farwing-protocol.md:617`：`touch`、`pen` 已定义但不声明 | 与代码一致（`src/protocol/peer.rs:156-215`，测试见 `:322`）。但 HELLO 的规则是“只声明运行时具备的”（`crates/protocol/src/peer/mod.rs:42-43`），被控的特性表却是写死的。加入 `touch`/`pen` 时必须改为运行时生成 |
| `web-console.md:10`：网页“支持触屏” | 只是鼠标模拟（`remoteInput.ts:126-244`）；笔经兼容鼠标事件当作鼠标处理 |
| `src/features/host/input/mod.rs:25-26`、`:71-72` 的注释提到“contact boundary”“drawing/contact paths” | 被控没有任何触点事件。这些注释是 v2 删除的触摸代码的遗留，新方案正好用得上 |
| `Cargo.toml:133` 开启了 `Win32_UI_Input_Pointer` | 全仓库没有代码用到。合成指针还需要 `Win32_UI_Controls`（或本地定义类型），并且必须动态加载 |
| 控制简报 §6：原生主控“没有触摸转发” | 确实不发 TOUCH。但触摸原生画面时，提升出的鼠标原始输入可能已经被当作鼠标转发了（§3.1，未验证） |

---

## 8. 实现清单（按顺序）

验证条件的标记：

- **[单机]**：一台 Windows 即可验证，不需要触屏或笔。
- **[触屏]**：需要触摸屏。本开发机可能有，可在“设置 → 系统 → 系统信息 → 笔和触控”查看，或读 `GetSystemMetrics(SM_MAXIMUMTOUCHES)`。
- **[笔]**：需要主动笔。
- **[两端]**：需要第二台设备当主控（手机、平板或另一台电脑），或者用虚拟机当被控。

1. **线格式**（`src/protocol/touch.rs`，或放进 `crates/protocol`）：用 prost 定义 §5.2、§5.3 的消息，实现 `encode/decode` 和往返测试，把 §5.9 的字节作为测试向量。**[单机]**
2. **被控事件与校验**：**[单机]**（单元测试）
   - `wire::Event` 增加 `Touch{screen, contacts}`、`Pen{screen, samples}`、`PointerCancel`，可以 JSON 序列化（服务管道要用），并实现 `validate()`；
   - `Receiver::receive` 接受 TOUCH/PEN 帧，解析失败时转成 `PointerCancel`；
   - 批处理只计入纯移动的帧，不合并；
   - 拖放占用指针时，不丢触点帧。
3. **注入层 `pointer.rs`**：**[单机]**
   - 内容：动态加载；本地定义 `POINTER_TYPE_INFO`；触摸设备与笔设备；组帧；重试与失败时清空；`Drop` 时取消。
   - 自测：做一个开发用的入口，在本机注入脚本化的序列：
     - 双指捏合；
     - 笔：悬停 → 落笔 → 压力渐变 → 倾斜 → 笔杆键 → 橡皮擦 → 抬笔 → 离开。
   - 观察：用本机上记录 Pointer Events 的网页（看 `pointerType`、`pressure`、`tiltX`、`buttons`）、画图或 Whiteboard。
   - 不需要触屏或笔，因为注入的是本机桌面；但测试会占用正在使用的桌面（见 `migration-status.md:112`）。
4. **引擎集成**：**[单机]**
   - 内容：
     - 触点表与 Windows id 0–9；
     - 100 ms 补帧，超时取消；
     - `release()`、交接、几何或桌面切换时取消触点并重建设备；
     - 回环保护；被控内部降级为鼠标；
     - `CURSOR_SUPPRESSED` 的修法。
   - 验证：本机控制本机（原生主控连本机被控），用第 3 步的注入器在观看窗口以外制造输入，检查超时与取消。服务模式与普通模式各测一次。
5. **HELLO**：被控在运行时声明 `touch`/`pen`；两种主控都声明，并按对方的声明决定是否发送；对方没声明时回退。**[单机]**
6. **浏览器**：
   - 内容：
     - `fpp.ts` 加类型号与特性名；
     - `input.ts` 加触摸、笔的控制器：待发帧合并、心跳、`releaseAll` 时取消；
     - `remoteInput.ts` 按 `pointerType` 和 `hostHello.features` 分流；笔的 `preventDefault`；
     - 工具栏的“触控方式”开关，“笔优先”；
     - `control.test.ts` 加 §5.9 的字节测试。
   - 验证：
     - 单点触摸：Chrome DevTools 的触摸模拟 **[单机]**；
     - 多点触摸 **[触屏]**；
     - 笔属性：先用第 3 步的注入器向浏览器窗口注入合成笔，浏览器会收到真实的 `pointerType: pen` 事件 **[单机]**；最后用真笔核对 **[笔]**；
     - Firefox、Safari（iPad）、Android **[两端]**。
7. **原生主控**：
   - 内容：
     - 给观看窗口加子类 `pointer_proc`；控制时调用 `UnregisterTouchWindow`（新特性 `Win32_UI_Input_Touch`）；
     - 读 `GetPointerType`、`GetPointerFrameTouchInfo`、`GetPointerPenInfoHistory`，经 `Target::position` 换算；
     - `RemoteInput` 的 `Touch/Pen` 事件、心跳和释放分类；
     - 过滤 0xFF5157xx 的提升鼠标；全屏时禁用边缘手势。
   - 验证：
     - 先实测“现在触摸原生画面会不会发鼠标” **[触屏]**；
     - 笔路径：向观看窗口注入合成笔 **[单机]**，再用真笔核对悬停、橡皮擦和笔杆键 **[笔]**。
8. **联调矩阵**：
   - 本机回环，注意 §6 的回环布局。**[单机]**
   - 手机或平板浏览器控制本机，只需要一台 Windows。**[两端]** 验证：
     - 触摸；
     - 锁屏；
     - UAC 安全桌面：被控以服务模式运行。开发机此前只在关闭安全桌面时验证过 UAC（`docs/dual-machine-runsheet.md:62`）；
     - 管理员窗口（UIPI）。
   - 坐标原点：主屏左侧或上方有副屏。可以接一块显示器，或者把 Farwing 虚拟显示器放到主屏左侧，但该驱动尚未装机实测。**[两端]**
   - 被控端的边缘手势、触摸键盘，Wintab 与 Windows Ink 两种模式的绘图软件。**[单机]**（用注入器）/ **[笔]**
9. **文档**：
   - `peer-messages.md` 的“预留”一节改为正式帧体；
   - `farwing-protocol.md` §5.5 的特性表与帧表；
   - `v2-design.md` §5.2、§7；
   - `web-console.md` 的“触屏”一行；
   - 控制简报 §6 指向本文；
   - `migration-status.md:112`；
   - `crates/protocol/src/peer/mod.rs:76` 的注释。
10. **可选（第三阶段）：Farwing HID 数字化仪**（§2.7）。
    - 内容：描述符、特性报告回调、`ValidReport/Neutral`、版本号、显示器映射；评估是否在安全桌面上启用。
    - 验证：**[单机]**（需要安装驱动、信任自签名证书，属于系统级改动）；与真笔对比 **[笔]**。
