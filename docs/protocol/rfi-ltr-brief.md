# RFI / LTR（参考帧失效与长期参考帧）丢包恢复实现简报

> **实施结果（2026-10-18）**：第 1–7、10、11.1 步与第 5 步的 DXVA 部分已实现。
> - 协议：`frame-meta` 扩展（`src/media/frame_meta.rs`）、特性 `video.recovery`、SESSION tag 8 `VideoFeedback`（主控经 control 通道 / FCT 发送）。
> - 主控：`transport/video_receiver/recovery.rs`（`D`、`L` 与放行判定）、`SeqNumOnlyRefFinder` 放行恢复帧并清理更早的搁置帧、`transport/rtc/recovery.rs`（恢复请求、兜底计时器 `max(2×RTT, 150 ms) + 34 ms`、恢复帧没到就退回关键帧直到下一个关键帧）。
> - 解码：DXVA H.264 对只参考一张已持有长期参考的帧放行 `frame_num` 跳号；DXVA HEVC 在 P 片实际不选用时容忍 `*_curr` 中缺失的短期参考（位置保留为无效项，列表下标不变）。两者都按码流内容判断，不需要额外标志。
> - 被控：`features/host/recovery.rs`（路由与决策：组长按自己观看者的确认标记两个槽、选最新的已持有 LTR、请求去重、无可用 LTR 时出 IDR、1 s 内连续 3 次又要关键帧就对该编码器关闭）；只有收到过确认后才周期标记，纯浏览器观看时只有关键帧带标记；恢复帧按关键帧级别做 FEC 与 RTX 保护；组员的恢复请求仍转成整组关键帧（第 9 步第一阶段的规则）。
> - 编码器：`FrameControl` / `LtrOutput`，NVENC H.264/HEVC 开启 2 个 LTR（`NV_ENC_CAPS_NUM_MAX_LTR_FRAMES ≥ 2` 时；初始化失败自动退回无 LTR；环境变量 `FARWING_NVENC_LTR=0` 可关闭），按实际输出（`ltrFrame`、`ltrFrameIdx`、`ltrFrameBitmap`）填写 `frame-meta`。AMF、QSV、软件 H.264 与 AV1 仍只出关键帧（第 8、12–14 步未做）；参考帧失效模式（§6.7 可选）未做。
> - 度量：主控注入器 `transport/rtc/impair.rs` 与 `dev video`（见 [client-usage.md](../client-usage.md)）。本机实测见 [selftest-results.md](../selftest-results.md) §13：H.264 与 HEVC 的恢复帧比关键帧小约 30–40 倍，整帧丢失场景下总流量少 41%–79%，随机丢包下避免了关键帧反复被丢的连锁；冻结时长只略短，因为仍由“2.5×RTT 判定”主导（§6.5 第 4 点的提前判定未做）。Chrome 观看开启了 LTR 的 H.264 / HEVC 码流正常。
>
> **补完（2026-10-18 第二批）**：
> - 提前判定（§6.5 第 4 点）：缺包超过 `max(1.5×RTT, 40 ms)` 且能用恢复帧时就请求，关键帧仍按 `max(2.5×RTT, 60 ms)`。本机 40 ms 单向延迟下，H.264 恢复 p50 从 234 ms 降到 174 ms、HEVC 从 216 ms 降到 183 ms；`dev video --recovery-late` 可关掉作对照。
> - 共享编码组（第 9 步）：组员把确认与恢复请求经 `Membership` 交给组长；组长在任一成员确认后开始周期标记，保留所有近 2 s 内确认过的成员都持有的最新参考，恢复请求用所有请求者都持有的参考。标记间隔按实测“标记到全员确认”的时间调整（`max(2×RTT, 1.5×确认时间)`，100 ms–1 s），带播放缓冲的观看者不会让候选槽在确认前被覆盖；主控兜底计时器加上自己的播放缓冲时长。本机两位主控同组时，丢包方 10 次全部由恢复帧恢复，无丢包方不再收到额外关键帧。
> - 软件 H.264（第 8 步）：SPS `max_num_ref_frames = 3`；IDR 用 `long_term_reference_flag` 成为长期参考 0；P 帧用 MMCO 4 + MMCO 3 把上一帧保留为长期参考；恢复帧把参考换成保存的重建平面并写 `ref_pic_list_modification`（`long_term_pic_num`）。编码 → 丢 3 帧 → 软件解码的单元测试逐位一致；端到端软件编码 + DXVA / 软件解码均由恢复帧恢复。
> - AMD AMF（H.264 / HEVC / AV1）与 Intel QSV（H.264 / HEVC）：按 §6.7 实现，**默认关闭**，分别用环境变量 `FARWING_AMF_LTR=1`、`FARWING_QSV_LTR=1` 开启；按输出属性（AMF `MarkedLTRIndex` / `ReferencedLTRIndexBitfield`，QSV H.264 `mfxExtAVCEncodedFrameInfo`）核对，HEVC 的 QSV 没有逐帧报告时按请求填写。本机没有对应显卡，未验证（清单 23.3）。
> - 仍未做：参考帧失效模式、浏览器的 Dependency Descriptor。（均见第四批：浏览器已实现，失效模式评估后不采用。）
>
> **第四批：浏览器（2026-10-18）**：
> - 浏览器协商 Dependency Descriptor（Chrome 默认不提供，网页端用 `setHeaderExtensionsToNegotiate` 请求；只在验证过的 Chromium 内核浏览器中请求，其他浏览器保持原来的行为），被控在每个包上写描述符，恢复帧只声明依赖它的长期参考（`src/media/dependency_descriptor.rs`）。
> - 浏览器每 100 ms 用 `VideoFeedback{ssrc, rtp_timestamp}` 报告最近解码的帧；被控按发送记录沿依赖链推出它持有的长期参考（`TrackFeedback::receive`）。
> - 被控拒绝重传时发只含原始序号的 RTX 包，让 libwebrtc 不再等这个包（它对 H.264/H.265 的组帧在缺包时挡住后续所有帧），再按浏览器的确认主动发恢复帧。
> - 实测（Chrome 155，被控每 3 秒丢一帧且拒绝重传，30 秒）：H.264 与 AV1 每次都由恢复帧恢复，最长帧间隔 48 ms，0 次 PLI、0 个关键帧；不协商描述符时（即原来的行为）出现 5 次约 3 s 的冻结，冻结合计 15 s。
> - H.265：Chrome 收到恢复帧但不解码（确认停在丢包前，约 3 s 后自己请求关键帧）。试过在恢复帧之前让 NVENC 失效短期参考（`nvEncInvalidateRefFrames`，使参考集只剩长期参考），结果相同，原因在 Chrome 内部，已撤回。H.265 浏览器与未协商描述符的浏览器改为：被控拒绝重传时立即出关键帧，最长帧间隔 48–73 ms，代价是每次一个关键帧。
> - 浏览器自己发的 PLI 不改用恢复帧：试过对确认过参考的浏览器用恢复帧回应 PLI，3 次中 3 次 Chrome 都在约 200 ms 后再次发 PLI，最后仍靠关键帧恢复（它发出 PLI 后只接受关键帧），只会多等 200 ms，已撤回；PLI 仍立即出关键帧。
> - 参考帧失效模式（§6.7 可选）评估后不采用：原生端的长期参考恢复帧已只有 2–18 KB，失效模式能省的很少；H.264 下失效后的帧号跳变要靠解码器的“缺帧补位”，其滑动窗口会挤掉要用的短期参考；在 H.265 上的试验也没有带来浏览器可解码的结果。
>
> **第三批（2026-10-18）**：
> - NVENC 升级到 API 13.0：绑定由 nv-codec-headers n13.0.19.0（MIT）用 bindgen 0.65.1 重新生成（`vendor/nvenc-sys`，生成命令记在文件头）。只支持 API 13（NVIDIA 驱动 570 或更新），不再兼容更早的驱动：结构版本号直接用 13.0 的常量，HEVC / AV1 位深用 `inputBitDepth` / `outputBitDepth`。驱动低于 570 时 NVENC 初始化失败并给出原因（“需要支持 API 13 的 NVIDIA 驱动（570 或更新）”），被控按编码器顺序改用其他编码器（AMF、QSV 或软件）。
> - NVENC AV1 长期参考：与 H.264/HEVC 相同的两个槽与恢复帧。另外请求了 `enableFrameIdNumbers`，但本机驱动（591.91）输出的序列头没有帧号，所以解码端的过期参考检查没有生效；为此用像素比对直接验证：两位主控同组（同一码流），一方注入丢包，丢包方恢复后解出的每一张画面都与无丢包方逐像素一致（AV1 875 张、H.264 886 张、HEVC 865 张，0 张不同）。
> - 可选增强：从不确认参考的观看者（浏览器）10 秒内 3 次以上要关键帧时，单独编码 60 秒，其他人不再跟着收关键帧；被控拒绝重传（包已不在历史、重传限速、保护预算）时，按该观看者最近的确认主动发恢复帧（每 200 ms 至多一次）。
> - 恢复帧没到时先再请求一次（新请求号），第二次仍没到才发 PLI。原因：恢复帧本身也可能丢；关键帧会清空所有参考、体积约为恢复帧的 20–50 倍，丢包下更容易再丢。
> - 观察到的限制：刚出过关键帧时，可用的参考只有关键帧附近的帧，恢复帧会达到 80–150 KB（平时 5–18 KB），因为编码器在关键帧之后的几帧里逐步提高静止区域的画质，引用早期帧要重做这部分；这只发生在会话开始或退回关键帧之后。
> - 播放流畅性（[selftest-results.md](../selftest-results.md) §16）：无丢包时长期参考开 / 关的帧间隔、抖动、帧大小相同，标记帧不比普通帧大；随机突发丢包下，与只用关键帧相比冻结合计从 7.9 s 降到 2.0 s，不再出现 0.5–1 s 的连续冻结。
> - 不采用帧内刷新兜底：刷新过程中解码端必须显示仍依赖丢失参考的区域，与“不显示错误画面”的原则冲突；有了 1–3 KB 的恢复帧，它的收益也很小。

本文是“丢包后不发完整关键帧也能恢复画面”的实现依据。项目决策把它列为“最后单独的课题”（[improvement-proposals.md](../improvement-proposals.md) §7 决策 7；[round2-status.md](../round2-status.md) §5）。本文要回答四个问题：

- 现在丢包后怎么恢复，代价有多大；
- 每个编码后端能做什么；
- 解码端和浏览器观看者要做什么；
- 协议、主控、被控各改什么，怎样在一台 Windows 机器上量出收益。

约定：

- 所有结论都来自代码阅读，`文件:行` 对应 2026-10-17 的代码快照。代码与 `docs/` 不一致时以代码为准，差异汇总在 §8。
- 用语：被控 = host；主控 = controller；“原生”指 Windows 客户端主控；“浏览器”指 `server/web` 的网页观看端。
- RFI（reference frame invalidation）：主控报告“哪些帧坏了”，编码器不再引用它们，改用更早的好帧作参考。
- LTR（long-term reference）：编码器把某些帧标成长期参考，长期保留在 DPB 里；主控确认收到后，丢包时编码器只参考这张已确认的帧。
- 恢复帧：引用旧的好帧、不是关键帧的 P 帧。
- FPP 帧头：`46 57 02 <flags> <type u16 BE> <request_id u32 BE>`（`crates/protocol/src/peer/mod.rs:4-13`）。

---

## 0. 结论速览

### 0.1 现状

- 丢包恢复只有一条路：PLI/FIR → 被控出 **IDR**。
- 所有编码器都没有使用 LTR、参考帧失效或帧内刷新（全仓库搜不到 `enableLTR`、`IntRefType`、`INTRA_REFRESH` 等的写入）。
- NVENC 固定单参考、无限 GOP（`src/platform/windows/nvenc.rs:345`、`:363-366`、`:387-388`、`:401-402`）。
- 主控用 libwebrtc 式的“按序号连续”判定参考关系（`src/transport/video_receiver/references.rs:87-136`）。丢一帧之后，后续所有非关键帧都被搁置，直到关键帧到达。**即使编码器发出了恢复帧，主控现在也不会解码它。**
- 原生 DXVA H.264 解码路径遇到 `frame_num` 跳号直接报错并重置（`src/platform/windows/decoder/rust_dxva/avc.rs:170-175`）。而任何恢复方式都会产生跳号。

### 0.2 阻断项（不改就做不成）

| # | 阻断 | 位置 |
|---|---|---|
| 1 | 码流里没有帧号和参考关系，主控无法判断“从这一帧起可解码” | 头扩展只有 7 项（`src/transport/rtc/negotiation.rs:299-320`，`src/features/host/peer/mod.rs:334-345`）；`frame-meta` 只停留在提案中 |
| 2 | 主控参考判定只认序号连续：有缺口的非关键帧一律搁置 | `references.rs:121-128` |
| 3 | DXVA H.264 拒绝 `frame_num` 跳号 | `rust_dxva/avc.rs:170-175` |
| 4 | 编码器接口只有一个 `keyframe: bool` | `src/platform/windows/encoder.rs:329-362` |
| 5 | 没有反馈消息：FPP 没有恢复请求或确认，RTCP 只处理 NACK、PLI、FIR | `src/features/host/peer/media.rs:162-200`，`src/features/host/keyframe.rs:37-64` |
| 6 | NVENC 绑定是 SDK 12.1，API 级别被钳到 12.1，**AV1 的 LTR 字段不存在** | `vendor/nvenc-sys/src/sys/windows_sys/nvEncodeAPI.rs:1088-1090`、`:630-658`、`:801-819`；`nvenc.rs:39` |

### 0.3 各后端能力（详见 §2）

“绑定可达”指仓库里已经 vendored 的绑定或头文件中有对应字段，不用新增 FFI。

| 后端 | H.264 | HEVC | AV1 |
|---|---|---|---|
| NVENC（SDK 12.1 绑定） | LTR ✓、RFI ✓、帧内刷新 ✓ | LTR ✓、RFI ✓、帧内刷新 ✓ | LTR ✗（SDK 13 才有，需更新绑定）；RFI 未知（接口与编码格式无关，需查能力位）；帧内刷新 ✓ |
| AMF 1.5.2 | LTR ✓；RFI ✗（没有失效接口，用 LTR 代替）；帧内刷新 ✓ | 同左 | 同左（另有 error resilient 开关） |
| QSV（oneVPL 2.17） | LTR ✓、RFI ✓（`RejectedRefList`）、帧内刷新 ✓ | LTR ✓、RFI ✓（别名结构体，运行时支持待实测） | 未知（2.6 起有通用别名 `mfxExtRefListCtrl`，运行时是否支持 AV1 未核实）；帧内刷新待实测 |
| 软件 H.264（自研） | 现在 ✗（单参考），可以自己实现 | — | — |
| Media Foundation | 项目没有使用 MF 编码器（`encoder.rs:251-256` 只有 4 个后端） | | |

### 0.4 推荐方案

1. **以 LTR 为主**。被控周期性地标记 LTR，主控解码成功后确认，丢包时被控编一帧只参考“已确认 LTR”的恢复帧。
   - 只要后端支持 LTR 就能用，与 RTT × fps 无关，三家厂商的 H.264/HEVC 都可用。
   - 参考帧失效（NVENC、QSV）作为第二阶段的可选模式，只在 RTT × fps 小、DPB 装得下时使用。
2. **关键帧兜底**，以下情况都退回现有的 PLI → IDR：
   - 没有已确认的 LTR；
   - 后端或编码格式不支持；
   - 恢复帧丢失或解码失败；
   - 浏览器观看者丢包。
3. **线上格式**：
   - 新 RTP 头扩展 `urn:farwing:rtp-hdrext:frame-meta`（6 字节，只放在每帧首包），携带纪元、帧号、参考帧号和标志（§6.2）。
   - 新 SESSION 推送 tag 8 `VideoFeedback`（C→H），走 `farwing.control.v2`（协商了 FCT 时经 FCT），用来确认 LTR 和请求恢复（§6.3）。
   - 新 HELLO 特性 `video.recovery`（§6.4）。
4. **主控**：
   - 解析 `frame-meta`；
   - 只确认带 LTR 标记的帧；
   - 判定不可恢复时先发恢复请求，超时再发 PLI；
   - 参考判定改为“恢复帧只依赖 `ref_id`”；
   - DXVA H.264 对恢复帧放行 `frame_num` 跳号。
5. **被控**：
   - 编码组长维护 2 个 LTR 槽（“全员已确认”槽 + “候选”槽）和每个接收方的确认状态；
   - 按“关键帧 > 恢复帧 > 标记 LTR”的顺序为每一帧做决定；
   - 浏览器成员丢包时仍出 IDR。
6. **先做单机能验证的路径**：
   - 软件 H.264 编码器自己实现 LTR，编出的码流用软件解码器和本机 DXVA 解码器验证；
   - 在主控入口加确定性丢包与延迟注入器，A/B 测冻结时长和字节数。
   - 三家硬件后端各需对应显卡的机器（§9）。

### 0.5 预期收益（推算，要实测）

- **字节**：桌面内容多数静止，恢复帧参考的 LTR 与当前画面几乎相同，体积远小于 IDR。IDR 受码控上限约束，可占约 125–200 ms 的链路码率（§1.8）。
- **冻结**：
  - 现在：`max(2.5×RTT, 60 ms)` 判定 + PLI 上行 + IDR 编码与发送 + 下行。
  - LTR：省掉大部分 IDR 发送时间。恢复很便宜，判定门限还可以提前到约 `max(1.5×RTT, 40 ms)`（§6.5）。
  - 提案里“约 1 个 RTT”的目标要靠提前判定才能接近。
- **多人**：原生成员丢包时，整组收到小恢复帧，不再收到 IDR。
- **连锁**：大 IDR 在丢包链路上更容易再丢、引发再次 PLI；恢复帧包数少，能缓解这一点。

---

## 1. 现在的丢包恢复链路

### 1.1 被控编码器

四个后端（`src/platform/windows/encoder.rs:251-256`），实现号 0/1/2/5（`:305-312`）。

| 后端 | 编码格式 | 参考与 GOP | 强制关键帧 | 关键帧判定（输出） |
|---|---|---|---|---|
| NVENC（`nvenc.rs`） | H.264、HEVC、AV1（AV1 需 API ≥ 12.0，`:224-227`） | `gopLength = u32::MAX`（`:345`）；无 B 帧（`:346`）；H.264 `maxNumRefFrames = 1`（`:388`）；HEVC/AV1 `maxNumRefFramesInDPB = 1`（`:402`、`:364`）；单 slice（`:390-391`、`:405-406`）；ULL 调优，H.264/HEVC 用 P1、AV1 用 P3（`:110-118`）；同步编码（`:434`） | `encodePicFlags = 6`，即 FORCEIDR\|OUTPUT_SPSPPS（`:612-614`；位定义 `nvEncodeAPI.rs:7464-7471`） | `pictureType == IDR`（`:650`） |
| AMF（`amf.rs`） | 同上 | H.264：无 B 帧、`IDRPeriod 0`（`:287-288`）；HEVC：`HevcGOPSize 1 000 000`（`:301`）；AV1：`INTRA_PERIOD 0`（`:541`）。**没有设置 `MaxNumRefFrames`**（头文件注释：HEVC/AV1 默认 1，H.264 依 usage 而定）。usage：H.264 为 LOW_LATENCY，HEVC 依次试 ULL、LL，AV1 为 ULL（`:108-112`、`:518`） | 在表面上设属性：H.264 `ForcePictureType=2`+SPS/PPS；HEVC 同理；AV1 `FORCE_FRAME_TYPE=KEY`+序列头（`:765-797`） | 输出属性 `OutputDataType`/`Av1OutputFrameType == 0`（`:881-904`） |
| QSV（`qsv.rs`） | 同上（AV1 需 oneVPL ≥ 2.5，`:593`） | `GopRefDist 1`、`TargetUsage 7`（`:623-624`）；`MaxDecFrameBuffering 1`（`:430`）。**没有设置 `GopPicSize`、`IdrInterval`、`NumRefFrame`**，由驱动默认 | `FrameType = I\|IDR\|REF`（`:860-866`） | `FrameType & IDR`，AV1 另看序列头（`:959-962`） |
| 软件 H.264（`crates/codec/kernels/h264/src/encoder.rs`） | 只有 H.264 4:2:0 8-bit（`crates/codec/src/encoder.rs:11-22`） | 只有一套参考平面（`encoder.rs:129`）；SPS `max_num_ref_frames = 1`、`gaps_in_frame_num_allowed = 0`（`encoder/syntax.rs:198-199`）；Baseline（`:191`）；**画面大变化时自己插 IDR**（`encoder.rs:255`） | `request_keyframe`（`src/platform/windows/software_encoder.rs:61-63`、`:91-93`） | IDR 标志（`encoder.rs:361`） |

- 软件候选总会加入，而且只有 H.264（`encoder.rs:59-64`）。被控设置中可以把编码方式强制为软件（`src/features/host/encoding_settings.rs:29-45`），单机测试会用到。
- 编码输入的时间戳 = 采集时刻相对 worker 启动的 100 ns 计数（`src/features/host/peer/capture_worker.rs:887`）。它单调且唯一，可以直接作为 NVENC `inputTimeStamp`（`nvenc.rs:608`），也就可以作为 `nvEncInvalidateRefFrames` 的索引。QSV 另有递增的 `FrameOrder`（`qsv.rs:858`）。

### 1.2 关键帧请求从哪里来

**网络请求（RTCP）**：

- 每个参与者、每条视频轨各有一个反馈任务：读 RTCP，NACK 交给传输层，RR 用来算 RTT。RR 到达前 RTT 按 100 ms 计（`src/features/host/peer/media.rs:146-203`、`:148-149`）。
- PLI/FIR 经 `keyframe::Feedback::accept` 过滤（`src/features/host/keyframe.rs:24-75`）：
  - 只认本轨 SSRC；
  - FIR 按发送方序号去重，17 ms 内的重复忽略；
  - **两次请求间隔不小于 `max(100 ms, 1.5×RTT)`**（`:65`）。间隔内的请求被直接丢弃，不会延后执行。
- 通过后置位 `request_keyframe`（`media.rs:195-200`），由采集线程在下一帧取走（`capture_worker.rs:877-878`）。

**本地原因**（都不经过上面的节流）：

| 原因 | 位置 |
|---|---|
| 新成员加入共享编码组，整组出关键帧 | `src/features/host/peer/encode_group.rs:232-234` |
| 任一成员的请求在组内共享 | `encode_group.rs:362-379`，`capture_worker.rs:856-858` |
| 组员的接收队列（3 帧）溢出出现缺口，丢弃非关键帧直到关键帧到达，1 s 未到就再请求 | `encode_group.rs:20-23`、`:441-450`；`capture_worker.rs:28-29`、`:468-504` |
| 组长在本人不收帧期间只给组员编码，恢复时出关键帧 | `capture_worker.rs:543-551` |
| 码率重配导致画质档变化 | `capture_worker.rs:872-874` |
| 采集源代次变化、编码出错、设备移除 | `capture_worker.rs:609-618`、`:936-963` |
| 要求了关键帧但输出不是关键帧，下一帧继续要求 | `capture_worker.rs:984-986` |
| 发送端轨道解绑、重新协商、格式退役 | `media.rs:274-284`、`:424-442` |
| 主控开始播放时主动发一次 PLI | `src/session/controller/mod.rs:825` |

### 1.3 打包、FEC、RTX、发送节奏

- **打包**：H.264 用 webrtc-rs 的 `H264Payloader`；HEVC 用 `src/features/host/hevc.rs`；AV1 用 `src/media/av1.rs`，关键帧置 N 位（`media.rs:318-328`）。
- **每帧头扩展**：`capture-index`、`new-frame`，以及按条件发送的 timing、color 等（`media.rs:373-423`）。
- **RTX**：
  - 发送历史至少保留 `max(1 s, 3×RTT)`（`src/features/host/transport.rs:1235-1266`）；
  - 重传受 0.7 倍速率上限约束，**关键帧的包不受此限**（`:1354-1360`）；
  - FEC 与 RTX 共用保护预算（`src/features/host/protection.rs`）。
- **RS-FEC**：
  - 每帧一个块（`src/features/host/fec.rs`）；
  - 目标块失败概率：普通帧 1e-3，**关键帧 1e-4**（`src/transport/fec_policy.rs:19-22`，`farwing-protocol.md` §6.4）；
  - 被控用 `transport.frame(..., frame.keyframe, ...)` 标记当前帧（`media.rs:285-290`，`transport.rs:605-627`）。
- **节奏**：每帧在 α × 帧间隔内发完，α 初始 0.4，范围 0.3–0.9，随丢包自适应（`transport.rs:126-136`）。

### 1.4 主控：检测丢包、NACK、PLI

逻辑在 `src/transport/rtc/feedback.rs`，由 `src/transport/rtc/receive.rs` 驱动：

- **NACK**：
  - 每 20 ms 处理一次；每包最多重试 17 次，退避底数 1.25、起始 50 ms；最多跟踪 1000 个缺包、10000 个序号的包龄（`feedback.rs:13-27`、`:381-388`）。
  - 缺包表溢出时清空并请求关键帧（`:219-235`）。
- **判定不可恢复**：缺包存在超过 `max(2.5×RTT, 60 ms)`，或某个包重试次数用尽（`feedback.rs:31-38`、`:305-306`）。
  - 判定后发 PLI，同一丢包事件的重发间隔不小于 `max(100 ms, 1.5×RTT)`（`:40-43`、`:324-337`、`:390-404`）。
  - **同一次 flush 里有 PLI 时，NACK 不发**（`:79-92`）。
- **其他 PLI 来源**：
  - 解码器返回“需要关键帧”，或解码失败（`src/transport/video_receiver/mod.rs:326-368`）；
  - 参数集缺失（`receive.rs:732`）；
  - 解码超时兜底：首次 3 s，之后按请求次数 1–3 s（`video_receiver/mod.rs:385-411`、`:592-643`）。
- 接收端自己请求关键帧时，同时清空 NACK 表（`receive.rs:801-811`）。

### 1.5 主控：参考关系与可解码判定

- `VideoStreamReceiver` 用 `SeqNumOnlyRefFinder`（`video_receiver/mod.rs:133`、`:197`），这是 libwebrtc 同名逻辑的移植（`references.rs`）：
  - 关键帧开一个新 GoP（`:88-103`）；
  - **非关键帧的首包序号必须紧接上一帧（含 padding）**，否则进入搁置队列，最多 100 帧（`:121-123`；`mod.rs:24`）；
  - 非关键帧固定引用“上一张图”（`:124-128`）。
- `FrameBuffer` 只放行参考帧都已解码的帧（`references.rs:296-318`）。解码失败后置 `keyframe_required`，非关键帧被丢弃（`:308-311`；`mod.rs:340-341`、`:507-513`）。
- 关键帧的判定（`src/transport/video_receiver/packets.rs:426-430`）：
  - H.264：IDR；
  - H.265：SPS + PPS + IDR；
  - AV1：N 位 + 序列头（N 位见 `depacketize.rs:139-161`）。

**结论**：没有显式的帧号和参考信息，主控无法知道丢包后到达的某一帧其实只依赖更早的好帧。头扩展加上参考信息之前，任何 RFI/LTR 在主控侧都无效。

### 1.6 解码器遇到缺参考时

| 解码器 | 行为 | 位置 |
|---|---|---|
| DXVA H.264 | 解码出错就整体重置，等下一个 IDR；**相邻参考帧的 `frame_num` 不是 +0/+1 就报错** | `rust_dxva/avc.rs:103-109`、`:164-169`、`:170-175` |
| DXVA HEVC | 重置后等随机接入点；当前帧要用的参考（`st_curr`/`lt_curr`）缺失就报错，`*_foll` 缺失可以容忍 | `rust_dxva/hevc.rs:61-65`、`:132-135`、`:262-290` |
| DXVA AV1 | 重置后等关键帧；7 个参考槽有空槽就报错（关键帧后槽都已填满，**槽内容过期时无法发现**） | `rust_dxva/av1.rs:37-44`、`:139`、`:285-291` |
| 软件 H.264 | 出错后等 IDR；DPB 完整支持 MMCO 与长期参考的重排（`Reorder::LongTerm`）；**不检查 `frame_num` 跳号** | `crates/codec/kernels/h264/src/stream.rs:217-223`、`:292-293`；`dpb.rs:61-124`、`:125-225` |
| 软件 AV1（rav1d 移植） | 按码流的槽引用解码 | `crates/codec/src/decoder/av1.rs` |
| HEVC 软件 | 没有，只做语法解析（`crates/codec/README.md` 的表格） | — |

- DXVA 的任何解码错误都映射为 `NeedKeyframe` 并重置（`rust_dxva/mod.rs:113-128`），接收端随即发 PLI。**所以恢复帧解码失败时会自动退回关键帧。**
- 每个关键帧都会触发一次解码器“切换重置”：`cutover` 包含 `frame.keyframe`（`src/application/viewer/decode_pipeline.rs:108`、`:143`）。DXVA 的重置只清空参考与输出，开销不大（`rust_dxva/mod.rs:170-177`）。

### 1.7 用户看到什么

- 丢包后的帧全部搁置，不送解码，**画面停在最后一张好帧上，不会出现花屏**。关键帧解码后画面跳到当前内容。
- 画面上没有专门的“正在恢复”提示。统计面板能看到：丢包率、未完成 NACK 数、解码前丢弃帧数、卡顿计数、已解码关键帧数（`src/diagnostics/performance.rs:37-51`、`:75-82`）。
- 冻结时间线（原生，单次丢包，NACK 修不好）：

  ```
  T_freeze ≈ max(2.5·RTT, 60 ms)        判定不可恢复（feedback.rs:31-38）
           + RTT/2                       PLI 上行（RTCP，丢了要等 max(100 ms, 1.5·RTT) 再发）
           + 0..1 帧间隔 + 编码耗时       等下一次采集迭代
           + IDR 发送时间                 受 VBV 上限约束，见 §1.8
           + RTT/2 + 组帧 + 解码器重置 + 解码
  ```

  例：RTT 50 ms、60 fps、20 Mbps，约为 125 + 25 + 17 + 编码 + IDR 发送（最多约 125 ms）+ 25，**约 200–320 ms**。这是推算，仓库中没有实测值。

### 1.8 关键帧的代价

仓库里没有关键帧大小或耗时的实测记录（`docs/` 只有“快速关键帧”的规则描述）。下表是由码控参数推出的**上限**：

| 后端 | 码控窗口（决定 IDR 能“借”多少码率） | 位置 |
|---|---|---|
| NVENC H.264/HEVC | `vbvBufferSize = max(5 帧, 1/8 s) × 目标码率`；60 fps 时取 125 ms；观影模式约 1 s；`lowDelayKeyFrameScale = 1` | `nvenc.rs:677-691`、`:358` |
| NVENC AV1 | `max(1 帧, 1/8 s) × 目标码率`，即 125 ms | `nvenc.rs:680-690` |
| AMF H.264/HEVC | 初始化时 VBV = 峰值码率（1 s）；运行时为 `max(5 帧 × 峰值/fps, 峰值/5)`，60 fps 时为 200 ms 的峰值码率；开启 RC 跳帧，注释称 IDR 之后会出现“重复画面” | `amf.rs:921-940`、`:271-279` |
| AMF AV1 | 1 帧 | `amf.rs:935-936`、`:536-539` |
| QSV H.264/HEVC | 低延迟时**不设置** `BufferSizeInKB`，由驱动默认（未知） | `qsv.rs:1040-1065` |
| QSV AV1 | 1 帧 | `qsv.rs:1055-1060` |
| 软件 H.264 | IDR 目标 = 4 × 每帧比特；P 帧限制在 0.55–1.5 倍 | `crates/codec/kernels/h264/src/encoder/rate.rs:243-257` |

此外还有这些额外代价：

- 关键帧用 1e-4 的 FEC 目标，冗余更多；RTX 不受速率限制（§1.3）。
- 一次请求让共享组里所有成员都收到 IDR（§1.2）。
- 主控每遇到一个关键帧就重置一次解码器（§1.6）。
- 大帧包数多，在丢包链路上整帧到齐的概率低，容易再次触发 PLI。

### 1.9 帧内刷新

没有使用。全仓库没有写入 `enableIntraRefresh`、`IntRefType`、`INTRA_REFRESH*`。三家绑定里的字段都可达：

- NVENC：`nvEncodeAPI.rs:561-562`、`:587-588`、`:638-639`、`:9048-9053`；
- AMF：`VideoEncoderVCE.h:268`、`VideoEncoderHEVC.h:179`、`VideoEncoderAV1.h:288`；
- QSV：`vendor/vpl-sys/src/bindings.rs:2137-2139`、`:2202`。

可以作为“重度丢包时不依赖反馈”的后续选项。本文不把它作为主方案：它持续消耗画质，而且仍需要主控能识别刷新完成的点。

---

## 2. 各编码 SDK 实际提供什么

### 2.1 NVENC（`vendor/nvenc-sys`，NVIDIA Video Codec SDK 12.1 的 Windows 绑定）

- **版本**：头文件是 12.1（`nvEncodeAPI.rs:1088-1090`）；运行时把 API 级别钳到 `min(driver, 0xc1)`（`nvenc.rs:39`），结构体版本号按级别计算（`:80-97`、`:596-598`）。
- **LTR（H.264/HEVC）**，字段都在：

  | 用途 | H.264 | HEVC |
  |---|---|---|
  | 开启（位域） | `enableLTR`（`:9100-9105`） | `enableLTR`（`:9424-9429`） |
  | LTR 数、信任模式 | `ltrNumFrames`、`ltrTrustMode`（`:567-568`） | 同名（`:590`、`:598`） |
  | DPB 与活动参考数 | `maxNumRefFrames`（`:563`）、`numRefL0`（`:572`） | `maxNumRefFramesInDPB`（`:589`）、`numRefL0`（`:600`） |
  | 每帧标记 / 使用 | `ltrMarkFrame`/`ltrUseFrames` 位域（`:10713-10731`）；`ltrMarkFrameIdx`、`ltrUseFrameBitmap`（`:766-767`） | 同名（`:10820-10838`；`:789-790`） |
  | 输出核对 | `NV_ENC_LOCK_BITSTREAM.ltrFrameIdx/ltrFrameBitmap`（`:889-890`）与位域 `ltrFrame`（`:11160-11165`） | 同左 |
  | 能力位 | `NV_ENC_CAPS_NUM_MAX_LTR_FRAMES = 40`（`:7618`） | 同左 |

  - SDK 文档（Context7，SDK 13.0 编程指南 §8.8）：LTR 用于 H.264、HEVC、AV1，不能与 B 帧同时使用（本项目不用 B 帧）。
  - `ltrTrustMode` 用 0，即“逐帧标记”模式；`ltrUsageMode` 置 0。
- **AV1 的 LTR**：12.1 绑定的 `NV_ENC_CONFIG_AV1` 与 `NV_ENC_PIC_PARAMS_AV1` 中没有任何 LTR 字段（`:630-658`、`:801-819`）。按 SDK 13.0 的发布说明，“AV1 Long-Term Reference”是 13.0 新增的。**要在 NVENC 上做 AV1 LTR，必须重新生成 13.x 绑定，并放开 `nvenc.rs:39` 的钳制**（结构体版本也随之改变）。
- **参考帧失效**：
  - 接口是 `nvEncInvalidateRefFrames(encoder, invalidRefFrameTimeStamp)`（类型 `:241-246`，函数表 `:1071`）；能力位 `NV_ENC_CAPS_SUPPORT_REF_PIC_INVALIDATION = 28`（`:7606`）。
  - 现在的接口完整性检查（`nvenc.rs:50-70`）没有包含这个函数，要补上。
  - 语义（SDK 文档）：客户端告诉编码器某个时间戳的帧坏了，编码器不再用它（以及由它重建的帧）作参考，改用更早的有效参考；**没有剩余参考时编一张帧内帧**。
  - 前提是 DPB 有余量。现在 DPB 为 1（`:388`、`:402`、`:364`），失效后必然编出帧内帧。
  - DPB 最大 16 帧。丢失帧到恢复帧之间隔着约 `(判定 + RTT + 编码) × fps` 帧，60 fps 下大约能覆盖 250 ms，144 fps 下约 110 ms。**所以 RFI 只适合低 RTT。**
  - AV1 是否支持失效，没有文档结论，要用 AV1 GUID 查询能力位。
- **AV1 帧号**：AV1 配置中有 `enableFrameIdNumbers` 位域（`nvEncodeAPI.rs:9933` 起的 setter 列表）。打开后码流带 `frame_id_numbers_present_flag`，解码端能发现参考槽内容不对（§3.1）。

### 2.2 AMF（`vendor/amf-sys`，AMF 1.5.2 公共头文件与生成的绑定）

- **LTR 属性三种编码格式都有**：

  | 用途 | H.264 | HEVC | AV1 |
  |---|---|---|---|
  | 初始化：LTR 数 | `MaxOfLTRFrames`（`VideoEncoderVCE.h:191`；`bindings.rs:89`） | `HevcMaxOfLTRFrames`（`VideoEncoderHEVC.h:160`；`bindings.rs:250`） | `Av1MaxNumLTRFrames`（`VideoEncoderAV1.h:232`；`src/av1.rs:30`） |
  | 初始化：LTR 模式 | `LTRMode`（`:192`，枚举 `:164-168`） | `HevcLTRMode`（`:161`） | `Av1LTRMode`（`:233`） |
  | 初始化：参考数 | `MaxNumRefFrames`（`:194`） | `HevcMaxNumRefFrames`（`:162`，默认 1） | `Av1MaxNumRefFrames`（`:234`，默认 1） |
  | 每帧（表面属性）：标记 | `MarkCurrentWithLTRIndex`（`:296`） | `HevcMarkCurrentWithLTRIndex`（`:261`） | `Av1MarkCurrentWithLTRIndex`（`:306`） |
  | 每帧：强制参考 | `ForceLTRReferenceBitfield`（`:297`） | `HevcForceLTRReferenceBitfield`（`:262`） | `Av1ForceLTRReferenceBitfield`（`:307`） |
  | 输出核对 | `MarkedLTRIndex`、`ReferencedLTRIndexBitfield`（`:308-309`） | `HevcMarkedLTRIndex`…（`:272-273`） | `Av1MarkedLTRIndex`…（`:316-317`） |
  | 能力 | `MaxReferenceFrames`（`:363-364`） | `HevcMaxReferenceFrames`（`:319`） | `Av1CapMaxNumLTRFrames`（`:365`；`src/av1.rs:151`） |

- **没有参考帧失效接口**。“只参考已确认 LTR”的语义靠 `ForceLTRReferenceBitfield` 实现。
- AV1 还有 `Av1ErrorResilientMode`（`VideoEncoderAV1.h:215`）与 `Av1FrameId`（`src/av1.rs` 的常量）。
- **现有代码怎么接入**：初始化属性用 `set_initial`（`amf.rs:475-488`），每帧属性写在 surface 上，与强制 IDR 同一处（`:765-797`），输出属性的读取方式同 `:881-904`。
- 注意 `RateControlSkipFrameEnable`（`:271-279`）：被跳过的帧上是否保留了 LTR 标记，要用输出属性核对。

### 2.3 QSV（`vendor/vpl-sys`，oneVPL 2.17 头文件与静态 dispatcher）

- 版本 2.17（`bindings.rs:83-85`）。
- **参考控制**：每帧在 `mfxEncodeCtrl.ExtParam` 中挂 `mfxExtAVCRefListCtrl`（`bindings.rs:2649-2661`、`:2857-2875`；类型号 `MFX_EXTBUFF_AVC_REFLIST_CTRL`，`:2306`）。
  - `LongTermRefList[16]`：把哪些 `FrameOrder` 标成 LTR；
  - `RejectedRefList[16]`：不用作参考，同时可以取消 LTR 标记，**这就是 RFI**；
  - `PreferredRefList[32]`：首选参考；
  - `NumRefIdxL0Active`。
  - 别名：`mfxExtHEVCRefListCtrl`、`mfxExtRefListCtrl`（`:3762-3765`）。
- **初始化**：`mfxInfoMFX.NumRefFrame`（`:1738`）。现在没有设置（`qsv.rs:622-628`）。
- **输出核对**：在 `mfxBitstream` 上挂 `mfxExtAVCEncodedFrameInfo`（`:2991-3011`，`MFX_EXTBUFF_ENCODED_FRAME_INFO`，`:2314`），可以读回 `LongTermIdx` 与 `UsedRefListL0`。
- **规范要点**（Context7，libvpl 规范附录 “Long Term Reference Frame” 与 “Reference List Selection”）：
  - 编码器自己从不标记 LTR，遇到 IDR 时清除所有 LTR；
  - 只能标记仍在编码器缓冲中的帧，建议在提交时标记；
  - `NumRefIdxL0Active < NumRefFrame` 时编码器可能忽略 LTR，要用 `PreferredRefList`；
  - 列表中的帧超出参考窗口时被忽略；找不到参考时编成无参考帧；
  - 2.6 起引入了通用别名 `mfxExtRefListCtrl`（当时为实验性 API）。
- **AV1 支持**：规范没有说明运行时对 AV1 是否生效，标为未知，要在 Intel 机器上用 `Query` 和 `EncodedFrameInfo` 实测。
- **其他**：
  - `GopPicSize`/`IdrInterval` 未设置，驱动可能插入周期 I/IDR，会清空 LTR。这本身无害，但要用 `frame-meta` 如实标出。
  - 提交时 `control` 没有挂扩展缓冲（`qsv.rs:860-867`），要改。

### 2.4 软件 H.264（自研，可以完整控制）

- **现状**：单参考平面（`encoder.rs:129`）；SPS `max_num_ref_frames = 1`、`gaps = 0`（`encoder/syntax.rs:198-199`）；Baseline（`:191`，Baseline 允许长期参考）。
- **实现 LTR 需要**：
  1. SPS 改为 `max_num_ref_frames = 1 + LTR 数`（建议 3）。
  2. 为每个 LTR 槽保存一份重建平面（与 `reference: [Plane; 3]` 同样的结构）。
  3. 标记帧在 `dec_ref_pic_marking` 中写 MMCO 4（`max_long_term_frame_idx_plus1`）与 MMCO 6（当前帧设为长期 idx）。也可以在 IDR 上置 `long_term_reference_flag`。
  4. 恢复帧把运动搜索的参考换成 LTR 平面，并在 slice 头写 `ref_pic_list_modification`（`long_term_pic_num`）。
  5. 恢复后丢掉其余短期参考。
- 解码侧的软件 DPB 已经支持这些语法（`dpb.rs:93`、`:142-186`），可以做逐位一致的单机回归（§6.10）。

### 2.5 Media Foundation

项目没有使用 MF 编码器（`encoder.rs:251-256`）。MF 的 `CODECAPI_AVEncVideoLTRBufferControl` 等与本课题无关，不纳入。

### 2.6 汇总（按编码格式 × 后端）

| | LTR | 参考帧失效 | 需要的绑定改动 |
|---|---|---|---|
| NVENC H.264 | 支持 | 支持（看能力位） | 无（补函数表检查） |
| NVENC HEVC | 支持 | 支持（看能力位） | 无 |
| NVENC AV1 | 不支持（12.1）；13.x 支持 | 未知 | 重新生成 13.x 绑定并放开 API 钳制 |
| AMF H.264/HEVC/AV1 | 支持 | 不支持（用 LTR 代替） | 无 |
| QSV H.264 | 支持 | 支持 | 无 |
| QSV HEVC | 支持（别名结构体），运行时待实测 | 同左 | 无 |
| QSV AV1 | 未知 | 未知 | 无（结构体可达） |
| 软件 H.264 | 自研后支持 | 可做，但单参考下意义不大 | 无（自研代码） |

---

## 3. 解码端

### 3.1 解码器要改什么

解码器原则上只需要照码流解码，LTR 和参考重排都是标准语法。要改的是我们自己的 DPB 管理和放行逻辑：

1. **DXVA H.264**：`avc.rs:170-175` 的跳号检查要对恢复帧放行。
   - 恢复帧的 `frame_num` 一定跳号，因为丢失的帧没有进 DPB。
   - 不插入“不存在的帧”：DXVA 的 `RefFrameList` 只能放真实表面（`avc.rs:261-275`）。
   - 滑动窗口在解码端少几张短期帧，不影响恢复帧要用的 LTR。
   - LTR 标记与 MMCO 已经支持（`avc.rs:296-330`、`:430-441`）。
   - 引用缺失照样报错，自动退回关键帧（§1.6）。
2. **软件 H.264**：不检查跳号，DPB 支持长期参考，**不用改**。
3. **DXVA HEVC**：
   - RPS 中丢失的帧落在 `*_foll` 时可以容忍；落在 `*_curr` 时报错退回关键帧（`hevc.rs:262-290`）。
   - 恢复帧的 `*_curr` 里仍列着丢失的短期帧，只是参考列表重排后不选它。这一项不能在 `RefPicSetStCurrBefore` 中留空（0xFF）：显卡按这几个数组的有效项数计算 NumPicTotalCurr，而 slice 头中 `list_entry_l0` 的位宽取决于它。少算一项，slice 头就会错位，整帧解错，而且一直错到下一个关键帧。现在用一张没有解码内容的表面顶替丢失的帧，与其他解码器生成“缺失参考”的做法相同。
   - 被控必须保证恢复帧及之后的帧不再把丢失的短期帧作为活动参考：`numRefL0 = 1`，或同时让丢失区间失效（§6.7）。
4. **DXVA / 软件 AV1**：
   - 槽位齐全时不报错，但**丢失帧刷新过的槽在解码端仍是旧内容，这种错误无法察觉**。
   - 要求编码器在恢复帧及之后只引用“LTR 槽 + 恢复帧本身刷新的槽”：AMF 的 `Av1MaxNumRefFrames` 默认为 1，满足这个要求。
   - 建议同时打开 AV1 frame id（NVENC `enableFrameIdNumbers`、AMF `Av1FrameId`），并在 `rust_dxva/av1.rs` 中核对 `ref_frame_id`，不一致就报错退回关键帧。
5. **解码器切换**：恢复帧不是关键帧，`decode_pipeline.rs:108`、`:143` 不会重置解码器，不用改。
6. **透传标志**：`EncodedVideoFrame` 加一个 `recovery: bool`（`receive.rs:866` 构造处），供第 1 点使用。

### 3.2 怎样判定“从这一帧起可解码”

依靠 `frame-meta`（§6.2）中的 `epoch`、`frame_id`、`ref_id` 与标志位，加上主控自己维护的解码状态：

- `D`：本纪元内连续解码链的最新 `frame_id`，在解码成功时更新（`DecoderFinished`，`receive.rs:465`）。
- `L`：已解码且仍被持有的 LTR 集合（按槽）。同一槽被新帧标记并解码后替换；关键帧或纪元变化时清空，只保留关键帧自身的标记。

恢复帧 F 的放行条件：

```
F.epoch == 当前纪元
&& 解码器没有处于“需要关键帧”状态（mod.rs:340-341）
&& ( F 带 REF_LTR 标志 ? F.ref_id ∈ L : F.ref_id <= D )
```

放行之后：

- 在 `SeqNumOnlyRefFinder` 中把 F 当作新 GoP 的起点，`references = [ref_id 对应的接收端帧 id]`；该帧 id 已在 `decoded` 集合里，集合容量 8192（`mod.rs:22`）。
- 清掉 F 之前的搁置帧（`clear_to`，`references.rs:204-207`），以及 F 首包之前的 NACK（`nack_requester.clear_up_to`）。
- F 之后的帧照常按序号连续链接到 F。

没有丢包的接收方看到的序号是连续的，按原路径解码恢复帧即可，不用任何特殊处理。

---

## 4. 浏览器观看者与多人共享编码

### 4.1 浏览器

- 浏览器由 libwebrtc 自动发 NACK、PLI、FIR，JS 不能主动发（`docs/protocol/web-viewer-brief.md` §4.7）。浏览器不发 RPSI，也不发 LNTF：`goog-lntf` 需要 Chrome 字段试验，默认不开。
- 浏览器的 offer 里没有 `frame-meta`，被控因此不会给浏览器发这个扩展：扩展按协商结果逐连接过滤（`media.rs:294-300`）。浏览器也不声明 `video.recovery`（`server/web/src/lib/fpp.ts` 的特性集合）。
- libwebrtc 对 H.264/AV1 在没有 Dependency Descriptor 时同样按序号连续判定参考。**浏览器丢包后收到的恢复帧会被搁置，最终仍发 PLI。** 所以被控对浏览器必须保持：
  1. PLI/FIR → IDR，节流不变（`keyframe.rs`）；
  2. RTX 与 FEC 不变；
  3. 浏览器没丢包时，组里发出的恢复帧对它就是普通 P 帧：序号连续，DPB 里也有对应的 LTR，可以正常解码。
- **后续方向**（需要实测）：给浏览器协商 Dependency Descriptor 扩展，在其中声明恢复帧依赖 LTR，让 Chrome 自己判定可解码；再由被控根据 NACK 推断浏览器丢包，主动发恢复帧。本文不展开。——已实现（文首“第四批”）：H.264 与 AV1 可用，H.265 在 Chrome 中不行。

### 4.2 共享编码组中的逐接收方反馈

- 每个成员仍有自己的反馈任务和传输（`media.rs:146-203`）；组长持有编码器（`encode_group.rs:1-9`），成员通过 `Membership` 向组长递交请求（`:362-379`）。
- 恢复请求与 LTR 确认沿同一条路径递交（§6.6）：
  - 成员 → 组长：`request_recovery(member, decoded, held)` 和 `ack(member, ltr)`；
  - 组长选 LTR 时要求被**所有正在请求恢复的成员**持有；
  - 没有请求恢复的成员在连续解码，自然持有组内所有 LTR。
- 组员转发的帧要带上组长的 `frame-meta`：`Shared` 和 `Encoded` 增加字段（`encode_group.rs:112-157`，`src/media/encoding/mod.rs:201-209`）。组员按自己的 RTP 时钟改写时间戳，但不改 `frame-meta`。
- 组长更换、编码器重建、格式变化时，纪元加 1，并以关键帧开始（与现在一致，`docs/protocol/v2-design.md` §4.2）。

### 4.3 混合会话规则

| 情况 | 处理 |
|---|---|
| 原生成员丢包，且有它已确认的 LTR | 整组收到一个恢复帧（便宜）；浏览器成员把它当普通 P 帧解码 |
| 原生成员丢包，没有可用 LTR | 整组 IDR（同现状） |
| 浏览器成员丢包（PLI） | 整组 IDR，同时清空 LTR 状态（同现状） |
| 新成员加入、组员队列缺口、本人暂停后恢复 | 关键帧（同现状）；组员缺口改为恢复请求可留到第二阶段 |
| 成员长期不确认（例如一直丢包） | 不阻塞他人：被控只在“全员已确认”槽可替换时更新候选槽（§6.6），超时后把该成员排除出确认集合 |

经常丢包的浏览器会让整组频繁出 IDR，这一点与现状相同。可以作为后续优化：把它单独分组。

---

## 5. 现有文档的预留

- `improvement-proposals.md` §2.4 F2 提出 FPP `RecoveryRequest` 与“用 NVENC/AMF/QSV 的 LTR 或参考帧失效”；F4 提出首包的 `urn:farwing:frame-meta`（frame_id、关键帧/恢复标志、source_epoch、参考帧）。**两者都只有名字，没有字段，也没有实现。**
- `farwing-protocol.md`：§5.4 没有相关帧类型；§5.5 没有相关特性；§6.3 的头扩展只有 7 项；§6.5 只有 PLI/FIR。
- `peer-messages.md`：SESSION 中没有相关 tag，“预留”只有 TOUCH/PEN（`peer-messages.md:725-727`）。
- `v2-design.md`：§4.2 的组内规则只写了关键帧。
- SESSION oneof 已用 tag 1–5、7、10–17、20–23（`src/features/stream_control/wire.rs:40-96`），本文取 **tag 8**（§6.3）。

---

## 6. 方案

### 6.1 总体

```
被控（组长）                                         主控（原生，声明 video.recovery）
 每 T_mark 标记一个 LTR（候选槽）  ──RTP + frame-meta[MARK_LTR]──>  解码成功 → VideoFeedback{ack}
 候选被全员确认 → 两个槽的角色互换  <──SESSION 8（control/FCT）────
 ...
 丢包                                                判定不可恢复 → VideoFeedback{recover, decoded, ltr[]}
 选全员持有的最新 LTR → 编恢复帧   <──────────────────
           ──RTP + frame-meta[RECOVERY|REF_LTR, ref_id]──>  放行（§3.2）→ 解码
 没有可用 LTR → IDR                                   T_fallback 内没恢复 → PLI（现有路径）
```

- 主路径是 LTR，适用于所有支持 LTR 的后端与编码格式。
- 参考帧失效是第二阶段的可选模式：NVENC H.264/HEVC 与 QSV，只在 `RTT × fps` 小、DPB 装得下时使用（§6.7）。
- 关键帧兜底全部保留（§0.4）。

### 6.2 RTP 头扩展 `urn:farwing:rtp-hdrext:frame-meta`

- 双方都注册之后才协商：
  - 主控 `register_header_extensions`（`negotiation.rs:299-320`）加入，视频扩展从 7 项变为 8 项，仍是一字节头（ID 1–14）；
  - 被控 `peer/mod.rs:334-345` 加入。
- **只放在每帧的首包**（`index == 0`）。RTX 重传会保留扩展（`transport.rs:1371-1373` 只改 SSRC、PT、序号）；RS-FEC 恢复出的是原包，这个扩展不在“可变扩展”名单里（`fec.rs:33-40`），不用改。
- 固定 6 字节：

  | 偏移 | 长度 | 字段 |
  |---|---|---|
  | 0 | 1 | `flags`：bit0 KEY（关键帧）；bit1 RECOVERY（恢复帧）；bit2 MARK_LTR（本帧被标为 LTR）；bit3 REF_LTR（与 RECOVERY 同时出现，表示 `ref_id` 是 LTR）；bit4–5 LTR 槽号（MARK_LTR 时有效）；bit6–7 为 0 |
  | 1 | 1 | `epoch`：编码器纪元，编码器重建、组长更换、格式变化时加 1 |
  | 2 | 2 | `frame_id`：u16 BE，本纪元内按编码顺序每帧加 1（回绕） |
  | 4 | 2 | `ref_id`：u16 BE。普通帧 = `frame_id − 1`（仅供参考，主控仍按序号连续链接）；KEY = `frame_id`；RECOVERY = 本帧及其后续帧所依赖的最新帧（含义见 §3.2） |

- 字节示例：一字节头元素 = `(ID << 4) | (6 − 1)`，下面假设协商出的 ID 为 8，即 `0x85`。

  | 帧 | 扩展元素 |
  |---|---|
  | 关键帧 1000（0x03E8），同时标为 LTR 槽 0（flags = KEY\|MARK_LTR = 0x05），纪元 3 | `85 05 03 03 e8 03 e8` |
  | 普通 P 帧 1201，参考 1200 | `85 00 03 04 b1 04 b0` |
  | 标为 LTR 槽 1 的帧 1240（flags = 0x04 \| 1<<4 = 0x14） | `85 14 03 04 d8 04 d7` |
  | 恢复帧 1250，参考 LTR 1180（flags = RECOVERY\|REF_LTR = 0x0A） | `85 0a 03 04 e2 04 9c` |
  | 失效模式的恢复帧 1250，依赖不晚于 1235 的帧（flags = 0x02） | `85 02 03 04 e2 04 d3` |
  | 只有这一个扩展时的完整扩展块（补 1 字节对齐） | `be de 00 02 85 00 03 04 b1 04 b0 00` |

- **被控必须按编码器的实际输出填写**：
  - 要求标记 LTR 但输出没有标记（用 NVENC `ltrFrameIdx`、AMF `MarkedLTRIndex`、QSV `EncodedFrameInfo` 核对），就不置 MARK_LTR；
  - 要求恢复帧但输出是 IDR，就置 KEY。
- 解析位置与 `capture-index` 相同（`video_receiver/mod.rs:241-296`），存进 `EncodedFrame`，由首包决定。

### 6.3 FPP：SESSION tag 8 `VideoFeedback`（C→H 推送）

选择理由：

- 复用现有帧类型和 protobuf 包装。
- 走 `farwing.control.v2`：协商 FCT 后经 KCP + RS 纠删，低延迟、可靠（`farwing-protocol.md` §5.6）。
- 已有 SESSION 走 control 通道的先例：被控在主控声明 `display.config` 时，把 `Screens` 推送发在 control 通道上（`farwing-protocol.md` §5.3）。
- 主控已有经 FCT 发送的接口（`src/transport/rtc/channels.rs:429`、`:459` 的 `fct.send_control`）。
- RTCP 方案的比较见 §6.9。

```proto
// SessionMessage 中：VideoFeedback video_feedback = 8;   // 推送，rid = 0，主控 → 被控
message VideoFeedback {
  fixed32 ssrc     = 1;   // 本轨视频的媒体 SSRC（主控 receive.rs 的 track.ssrc()；被控 encodings[0].ssrc）
  uint32  epoch    = 2;   // frame-meta 的纪元（0–255）
  uint32  decoded  = 3;   // 本纪元连续解码链的最新 frame_id（0–65535）
  repeated uint32 ltr = 4 [packed = true];  // 已解码且仍持有的 LTR frame_id，新的在前，最多 4 个
  bool    recover  = 5;   // true：decoded 之后发生了不可恢复的丢包，请求恢复帧
  uint32  request  = 6;   // 恢复请求序号，每个丢包事件加 1，用于去重；确认消息可以省略
}
```

- **确认**：解码成功的帧带 MARK_LTR 时立即发一次，50 ms 内合并为一条。其中 `decoded` 是该帧的 id，`ltr` 是当前持有的全部 LTR。`ltr` 列表本身就有冗余，补偿个别确认丢失。
- **请求**：`recover = true`，带上当前的 `decoded` 与 `ltr`。被控据此推出丢失区间的起点 = `decoded + 1`，失效模式要用这个值。
- 被控不回复。恢复帧本身就是回复：由 `frame-meta` 的 RECOVERY 标志和 `ref_id` 识别。
- 通道与校验：
  - 主控只在 control 通道发送；被控在 control 与 session 两个通道都接受，作为容错。
  - 被控在 `src/features/host/peer/ingress.rs:39-97` 的分发中，在 `publisher::receive` 之前识别 `SESSION` 且 body 为 tag 8 的帧，按 SSRC 转给对应轨道的恢复状态。
  - 未声明特性、SSRC 不认识、纪元不对的消息，一律记 debug 日志后丢弃，不回错误。

字节示例（SSRC 0x12345678，纪元 3）：

| 消息 | 字节 |
|---|---|
| 确认 LTR 1240（0x04D8），持有 [1240, 1180] | `46 57 02 00 02 01 00 00 00 00` + `42 10` `0d 78 56 34 12` `10 03` `18 d8 09` `22 04 d8 09 9c 09` |
| 请求恢复：已解码到 1200（0x04B0），持有 [1180]，请求序号 5 | `46 57 02 00 02 01 00 00 00 00` + `42 12` `0d 78 56 34 12` `10 03` `18 b0 09` `22 02 9c 09` `28 01` `30 05` |

编码要点：

- SESSION tag 8 是 LEN 类型，tag 字节为 `(8<<3)|2 = 0x42`；
- `ssrc` 是 fixed32，小端，tag 字节 `0x0d`；
- 1200 的 varint 为 `b0 09`，1240 为 `d8 09`，1180 为 `9c 09`；
- `ltr` 用 packed 编码，tag 字节 `0x22`。

### 6.4 特性协商

- 新特性名 `video.recovery`，版本 1：加入 `crates/protocol/src/peer/mod.rs:43-83`，以及 `src/protocol/peer.rs:156-215` 的主控、被控两个集合。
- 被控对某位参与者启用恢复的条件：该参与者的 HELLO 声明了 `video.recovery`（`PeerFeatures::has`，`src/protocol/peer.rs:141-144`），**并且**该连接协商出了 `frame-meta`（看 `get_parameters().rtp_parameters.header_extensions`，与 `media.rs:294-296` 相同）。
- 被控总是声明这个特性；当前编码器能不能做 LTR 是被控内部的事，做不到就出 IDR。
- 主控的条件：被控声明了特性，并且本纪元收到过 MARK_LTR 帧。不满足时照旧直接发 PLI。
- 任何一端没有声明（浏览器不声明），就完全按现状工作。

### 6.5 主控逻辑

位置：`src/transport/rtc/receive.rs` 加 `src/transport/video_receiver/`。

1. **解析**：首包的 `frame-meta` → `EncodedFrame{epoch, frame_id, ref_id, flags}`。
2. **解码成功时**（`DecoderFinished` 结果为 `accepted` 且不是 `RequestKeyframe`，`receive.rs:465-480`）：
   - 更新 `D`；
   - 关键帧时清空 `L`；
   - 带 MARK_LTR 的帧写入 `L[slot]`，并排队发确认。
3. **不可恢复判定**（`feedback.rs:305-306` 的 `stale`、`:219-235` 的溢出、`packets` 的 `cleared`）：
   - 如果特性生效、`L` 非空、解码器不在“需要关键帧”状态：发 `VideoFeedback{recover}`，**并且继续发 NACK**（不走 `flush` 中“有 PLI 就丢 NACK”的分支，`feedback.rs:82-91`），同时启动兜底计时器 `T_fallback = max(2×RTT, 150 ms) + 1 帧间隔`。
   - 否则照旧发 PLI。
   - 兜底计时器到期仍没有可解码的帧（恢复帧或关键帧），发 PLI，交给现有逻辑。
   - 同一个丢包事件最多发一次恢复请求；恢复帧丢失后不再重试恢复，直接 PLI。
4. **提前判定**（可选，第二阶段）：恢复很便宜，可以把判定门限从 `max(2.5×RTT, 60 ms)` 提前到约 `max(1.5×RTT, 40 ms)`。RTX 与恢复帧谁先到用谁，晚到的 RTX 包会被 `clear_to` 丢弃。只在特性生效时提前。
5. **参考判定**：按 §3.2 放行恢复帧。不满足条件的恢复帧照旧搁置，由兜底计时器发 PLI。
6. **解码**：把 `recovery` 标志透传给解码器（§3.1 第 6 点）。
7. **指标**（加进 `PerformanceMonitor`）：
   - 按方式计数：LTR / 失效 / 关键帧；
   - 每次恢复的冻结时长：丢包后最后一张好帧的呈现时刻 → 恢复后第一帧的呈现时刻；
   - 恢复帧字节数；
   - 恢复失败（退回 PLI）次数。

### 6.6 被控逻辑

**每个编码器（组长）的状态**，放在采集线程（`capture_worker.rs`）：

```
epoch: u8                       // 编码器创建、格式变化、组长更换时加 1
next_id: u16                    // 每输出一帧加 1
slots: [Option<Ltr{id, at, ts_100ns, frame_order}>; 2]
                                // 一个“全员已确认”槽 A，一个候选槽 B
recent: ring<(id, ts_100ns, frame_order)>
                                // 最近约 2 s 的帧；失效模式用来查时间戳或 FrameOrder
caps: RefCaps{ltr_slots, invalidation, dpb}
                                // 由后端在创建时报告（§6.8）
```

**每个接收方的状态**，放在 media worker 中，组员经 `Membership` 交给组长：

```
capable: bool                   // 见 §6.4
acked: Option<u16>              // 本纪元内该接收方确认过的最新 LTR
held: Vec<u16>                  // 最近一次 VideoFeedback.ltr
pending: Option<{decoded, request, at}>
last_served: Option<{request, frame_id, at}>
```

**每帧的决策**，替换 `capture_worker.rs:877-878` 的单一 `force`：

1. 有关键帧需求（本人或组员的 PLI/FIR、新成员、重配、错误等，见 §1.2）：
   - 出 IDR，并标记为 LTR 槽 0（`frame-meta` 置 KEY|MARK_LTR）；
   - 清空两个槽和所有接收方的 `acked`/`pending`。
2. 否则，如果有待处理的恢复请求（集合 R）：
   - 先去重：`request` 不大于已服务的序号，或 `decoded` 早于上次为它出的恢复帧且距今不到 `max(100 ms, 1.5×RTT)`，就丢弃该请求。
   - 选 LTR：槽中纪元相同、被 R 中每个成员的 `held` 都包含、`now − at ≤ LTR_MAX_AGE`（建议 5 s）的最新一个。
   - 选到了：本帧“只参考该 LTR”。支持失效的后端同时让 `min(decoded) + 1` 起的帧失效。`frame-meta` 置 RECOVERY|REF_LTR，`ref_id = ltr.id`。
   - 没选到，但后端支持失效，且 `next_id − (min(decoded) + 1) < dpb − 1`：只做失效，`ref_id = min(decoded)`。
   - 都不行：出 IDR。
3. 否则，到了标记时间（`now − 上次标记 ≥ T_mark`，建议 `clamp(2×最大成员 RTT, 100 ms, 1 s)`），并且槽 A 已有全员确认的 LTR（或者槽为空）：把本帧标记到候选槽 B，`frame-meta` 置 MARK_LTR。
4. 确认处理：B 被所有 `capable` 且活跃的成员确认后，A、B 角色互换，原来的 A 成为下一个候选槽。成员 2 s 不确认就不再计入“全员”；这个成员以后请求恢复时，只能用它自己 `held` 里的 LTR。
5. 恢复帧与关键帧一样处理：FEC 用严格目标，RTX 不受限速。实现方式是给 `transport.frame(...)` 传 `keyframe || recovery`（`media.rs:285-290`）。

**节流与兜底**：

- 恢复请求不经过 `keyframe::Feedback` 的间隔门（那个门只管 PLI/FIR）；用上面的 `request` 去重。
- PLI/FIR 路径保持不变。浏览器、没有声明 `video.recovery` 的主控和兜底请求照旧生效。
- 自动关闭：同一编码器上，如果恢复帧发出后 `T_fallback` 内又收到同一成员的 PLI，计为一次“恢复失败”。连续 3 次失败，就在本编码器的生命期内关闭 LTR，只出 IDR，并记 warning。这可以保护驱动有缺陷的机器。

**组员**：

- 组员把成员的确认和请求经 `Membership` 交给组长（`encode_group.rs` 的 `Member` 加字段，参照 `request_keyframe`/`take_keyframe`，`:362-379`）。
- 组员转发的 `Shared` 帧带上 `frame-meta`。
- 组员自己的接收队列缺口（`capture_worker.rs:482-494`）第一阶段仍走关键帧。

### 6.7 各后端的落地参数

| 后端 | 初始化 | 标记 LTR | 恢复帧 | 失效（可选） | 输出核对 |
|---|---|---|---|---|---|
| NVENC H.264/HEVC | `enableLTR=1`、`ltrTrustMode=0`、`ltrNumFrames=2`（不超过 `NV_ENC_CAPS_NUM_MAX_LTR_FRAMES`）；`maxNumRefFrames`/`maxNumRefFramesInDPB = ltrNumFrames + 1`（失效模式要更大）；`numRefL0 = 1`。是否必须如此以 SDK 文档与实测为准 | 本帧 `ltrMarkFrame=1`、`ltrMarkFrameIdx=slot` | `ltrUseFrames=1`、`ltrUseFrameBitmap=1<<slot` | `nvEncInvalidateRefFrames(ts_100ns(min(decoded)+1))`：编码器会把由它派生的帧一并失效；也可以对区间内每个时间戳各调一次 | `ltrFrameIdx`/`ltrFrameBitmap`/`ltrFrame`、`pictureType` |
| NVENC AV1 | 需要 13.x 绑定（§2.1） | 13.x 字段 | 13.x 字段 | 看能力位 | — |
| AMF H.264/HEVC/AV1 | Init 之前设 `MaxOfLTRFrames`（或 `Hevc*`/`Av1*`）= 2、`LTRMode = KEEP_UNUSED`；`MaxNumRefFrames = 1` | surface 上设 `MarkCurrentWithLTRIndex = slot` | `ForceLTRReferenceBitfield = 1<<slot` | 不支持 | `MarkedLTRIndex`、`ReferencedLTRIndexBitfield` |
| QSV H.264/HEVC | `NumRefFrame = 3`（失效模式要更大）；读回 `GetVideoParam` | `mfxEncodeCtrl.ExtParam` 挂 `mfxExtAVCRefListCtrl{LongTermRefList[0] = 本帧 FrameOrder}` | `PreferredRefList[0] = LTR 的 FrameOrder`、`NumRefIdxL0Active = 1`，`RejectedRefList` = 丢失区间 | `RejectedRefList` | `mfxExtAVCEncodedFrameInfo.LongTermIdx/UsedRefListL0` |
| QSV AV1 | 先 `Query` 探测；不支持就关闭 | 同左 | 同左 | — | — |
| 软件 H.264 | SPS `max_num_ref_frames = 3`；保存 2 份 LTR 平面 | MMCO 4 + MMCO 6，或 IDR 的 `long_term_reference_flag` | 参考平面换成 LTR；写 `ref_pic_list_modification`；清空其余短期参考 | 不做 | 自产，不用核对 |

### 6.8 编码器接口改动

- `platform::windows::encoder::Encoder`（`encoder.rs:257-363`）：
  - `encode_cancellable(texture, ts, keyframe: bool, cancel)` 改为 `encode_with(texture, ts, control: FrameControl, cancel)`。
  - `FrameControl{keyframe, mark_ltr: Option<u8>, use_ltr: Option<u8>, invalidate_from_ts: Option<i64>}`。
  - 新增 `reference_caps() -> RefCaps{ltr_slots, invalidation, dpb}`。
  - 软件编码器原来的 `request_keyframe` 语义保留（`:349-351`）。
- `Encoded`（`src/media/encoding/mod.rs:201-209`）新增：
  - `meta: Option<FrameMeta>`：由采集线程填写，不由后端填写；
  - `ltr_marked: Option<u8>`、`ltr_used: u8`：由后端按实际输出回报。
- 能力探测（`encoder.rs:8-199`）可以顺带探测 LTR 与失效能力，存进 `Capability`。这些能力不上线，只影响被控的决策。
- 编码器重建（`capture_worker.rs` 中所有 `encoder = None` 的位置）时，纪元加 1。

### 6.9 备选：RTCP 反馈（本文不选）

- **可行性**：
  - vendored 的 RTCP 解析器把未知的 PSFB/RTPFB 格式和 APP（PT 204）解析为 `RawPacket`（`vendor/rtcp/src/packet.rs:97-118`），所以被控能读到 RPSI（FMT 3）或 APP。
  - 但 **PSFB FMT 15（AFB）会被当作 REMB 解析**（`:112`）。非 REMB 的负载会解析失败，`media.rs:155-158` 会丢弃整个复合包，连同其中的 NACK。
- **好处**：路由简单。主控 `receive.rs` 手里就有 `connection`；被控反馈任务本来就按轨道读 RTCP（`media.rs:146-203`）。
- **坏处**：
  - 没有 protobuf，扩展字段要自定二进制格式；
  - RPSI 的“native bit string”对 H.264/HEVC/AV1 都没有标准定义，自定义等于滥用标准；
  - 不可靠，确认要靠重复发送；
  - 浏览器同样发不出来，没有额外收益。
- **结论**：用 FPP（§6.3）。如果 control 通道的路由改动在实现时证明代价过大，可以退回 RTCP APP（名字 `FWRC`，负载与 §6.3 的 protobuf 相同），协议语义不变。

### 6.10 单机度量

**确定性丢包与延迟注入器**（只在 `dev` 命令或调试构建中启用）：

- **位置**：主控 `receive.rs` 中，所有恢复手段（RTX 解包、RS-FEC）完成之后、交给 `NackRequester` 与 `VideoStreamReceiver` 之前，即处理 `pending_media` 处（`receive.rs:599` 附近）。
  - 按**原始序号**过滤，被 RTX 或 FEC 恢复出来的同一个包也一起丢弃，保证丢失不可恢复。
- **规则**（环境变量 `FARWING_DEV_VIDEO_LOSS` 或 `dev video --loss`）：
  - `frame:N[,every=M]`：丢弃第 N 帧（按 RTP 时间戳计）的全部包，之后每 M 帧丢一次；
  - `burst:start_ms+dur_ms`：丢弃这段时间内的全部视频包，模拟 Wi-Fi 突发；
  - `ge:p,r,seed=S`：Gilbert–Elliott 模型，固定种子；
  - `delay:D`：所有视频包延迟 D ms 再处理，用来模拟单向时延。本机回环的 RTT 约为 0，60 ms 的判定下限会掩盖差异，必须加延迟。
- **headless 主控**：新增 `farwing dev video --device-id … --seconds 60 --loss … --delay … --json`，与现有的 `dev input`、`dev clipboard` 同类（`src/main.rs:248-340`）。它解码但不呈现（软件或硬件解码可选），结束时输出 §6.5 的指标。
- **A/B**：
  - 同一个注入种子，分别在“主控不声明 `video.recovery`”（环境开关）和“声明”两种条件下各跑 N 次（建议 100 次丢包事件）；
  - 被控在同一台机器上运行；
  - 编码方式用“软件”（被控设置，`encoding_settings.rs:29-45`）或本机显卡。
- **指标**：

  | 指标 | 说明 |
  |---|---|
  | 恢复时长 p50/p95 | 丢包事件的首个缺失包本应到达的时刻 → 第一张恢复后画面解码完成 |
  | 冻结时长 p50/p95 | 呈现上相邻两张新画面的最大间隔，扣掉一个正常帧间隔 |
  | 恢复字节 | 恢复帧（或 IDR）的字节数；丢包后 1 s 内的总接收字节减去基线 |
  | 关键帧数 / 分钟 | `total_key_frames_decoded`（`performance.rs:82`） |
  | 恢复失败率 | 恢复请求之后仍发了 PLI 的比例 |
  | 被控侧 | 按方式统计的恢复次数、使用时 LTR 的年龄、恢复帧大小与平均 P 帧大小之比（日志 target `farwing::recovery`） |

- **单元与回归测试**（都不需要 GPU）：
  1. `frame-meta` 的编码与解析；
  2. `VideoStreamReceiver` 合成包测试：丢第 N 帧 → 恢复帧放行、搁置帧被清理、不发 PLI；不满足条件时退回 PLI；
  3. 软件 H.264 编码器标记 LTR，跳过 K 帧后编恢复帧，用软件解码器解码剩余帧，**与编码器的重建结果逐位一致**；
  4. 被控决策模块：槽轮换、全员确认、去重、退回 IDR、自动关闭；
  5. 在厂商机器上录制一次带 LTR 和恢复帧的码流，放进 `src/media/decoder/fixtures/`。之后在任何有对应 DXVA 解码能力的机器上回归解码路径。

---

## 7. 风险

| 风险 | 说明 | 对策 |
|---|---|---|
| 误放行 → 持续花屏 | 主控错误地放行了依赖缺失参考的帧。H.264/HEVC 通常报错退回关键帧，但 AV1 的过期槽不会报错，花屏会持续到下一个关键帧 | 放行条件严格（§3.2）；AV1 打开 frame id 并在 `rust_dxva/av1.rs` 中核对；被控按实际输出填写 `frame-meta`；首版可以先只对 H.264/HEVC 开启 |
| H.264 `frame_num` 跳号 | 原生 DXVA 路径现在直接报错（`avc.rs:170-175`）；第三方解码器（浏览器）对跳号的处理不一 | 只对带 RECOVERY 的帧放行；浏览器丢包时本来就不走恢复（§4.1） |
| 多参考导致后续帧引用丢失的短期帧 | 恢复后编码器可能仍把丢失区间内的短期帧当作有效参考 | `numRefL0 = 1` 或 `NumRefIdxL0Active = 1` / AMF `MaxNumRefFrames = 1`；支持失效的后端同时让丢失区间失效 |
| 驱动对 LTR 的实现缺陷或差异 | AMF 的跳帧、QSV 忽略 LTR（活动参考数不足时）、NVENC 版本差异 | 用输出属性逐帧核对；连续失败自动关闭（§6.6）；每个后端一个总开关；需要三家实机验证 |
| NVENC AV1 | 需要 SDK 13.x 绑定和更新的驱动，结构体版本与 API 级别都要改 | 放在最后；先做 NVENC H.264/HEVC 与 AMF AV1 |
| 时间戳唯一性 | NVENC 失效按 `inputTimeStamp` 定位帧；共享组里组员改写的是 RTP 时间戳，组长的编码输入时间戳不变（`capture_worker.rs:887`） | 失效只在组长侧，用组长的 `recent` 环表查找 |
| 共享组中个别成员拖累 | 某成员长期不确认，候选槽一直不能晋升 | 2 s 后把它排除出“全员”；它请求恢复时只用自己持有的 LTR，没有就出 IDR |
| LTR 太旧 | 画面大幅变化后，参考旧 LTR 的恢复帧可能接近 IDR 大小 | `LTR_MAX_AGE`；`T_mark` 随 RTT 调整；超龄就出 IDR。即使接近 IDR 大小，也免去了组内全员 IDR 和解码器重置 |
| 码控尖峰 | 恢复帧会消耗 VBV | 沿用现有 VBV；恢复帧按关键帧级别做 FEC/RTX 保护 |
| 反馈延迟 | 没有 FCT 时 control 通道走 SCTP，有队头阻塞 | 兜底计时器到期发 PLI；原生两端都声明 `fct` |
| DPB 变大 | 解码表面和内存增加（DXVA 表面数由 SPS 推出，`avc.rs:185-200`） | LTR 数取 2，DPB 只增加 2 张 |
| 解码器输出延迟 | DPB 大于 1 时，个别解码器可能推断出重排延迟 | NVENC 已写 `bitstreamRestrictionFlag`（`nvenc.rs:395-397`）；用主控的统计面板确认延迟不变 |
| 收益被高估 | 冻结时长的主要部分是判定门限 `2.5×RTT` | 提前判定（§6.5 第 4 点）单独做 A/B；以实测数据决定默认值 |
| 单机测试失真 | 本机回环的 RTT≈0；同一台机器上的 CPU 竞争（`round2-status.md` §4） | 用注入器加延迟；最终结论在双机上复核（`manual-test-checklist.md` 中环境为双机的项目） |

---

## 8. 文档与代码的出入（以代码为准）

| 文档说法 | 代码事实 |
|---|---|
| `improvement-proposals.md` §2.4 F2“现状”：被控 PLI 节流 600 ms；NACK 放弃时不触发 PLI；要等解码超时（初始 3 s，稳态 1 s） | 这是 P5 之前的状态。现在节流为 `max(100 ms, 1.5×RTT)`（`keyframe.rs:65`）；NACK 重试用尽或缺包超过 `max(2.5×RTT, 60 ms)` 就立即发 PLI（`feedback.rs:305-306`、`:324-337`）；解码超时 1–3 s 仍然保留，作为兜底（`video_receiver/mod.rs:592-643`） |
| 同上：“编码器无限 GOP、单参考” | 只对 NVENC 完全成立（`nvenc.rs:345`、`:364`、`:388`、`:402`）。AMF 没有设置参考帧数（`amf.rs:286-302`、`:515-545`）。QSV 的 `GopPicSize`、`IdrInterval`、`NumRefFrame` 都没设，由驱动默认（`qsv.rs:622-628`）。软件编码器在画面大变化时自己插 IDR（`encoder.rs:255`） |
| `improvement-proposals.md` F4 / F2：`urn:farwing:frame-meta`、FPP `RecoveryRequest` | 都没有实现，也没有字段定义。视频头扩展是 7 项（`negotiation.rs:304-312`） |
| `farwing-protocol.md` §6.5：缺包超时或 NACK 放弃就发 PLI | 正确，但没有写出：同一批次里有 PLI 时，NACK 不发（`feedback.rs:79-92`）；接收端自己请求关键帧时清空 NACK 表（`receive.rs:801-808`） |
| `improvement-proposals.md` F2“收益：冻结从约 1 s 降到约 1 个 RTT” | 快速关键帧已经把冻结压到约“2.5×RTT + IDR 发送”。LTR 省掉的是 IDR 的发送和解码器重置，判定门限仍是 2.5×RTT；要接近 1 个 RTT，必须提前判定（§0.5、§6.5） |
| `v2-design.md` §5.4：HELLO 在 session 通道；初始特性包含 `video.10bit` | HELLO 在 control 通道交换（`channels.rs:429`，`ingress.rs:72-79`）；代码中没有 `video.10bit`（`crates/protocol/src/peer/mod.rs:43-83`） |
| `farwing-protocol.md` §5.3：control 通道承载 HELLO、INPUT、POINTER、CURSOR、POINTER_POLICY 与 H→C `Screens` | 本方案新增 C→H 的 SESSION `VideoFeedback`。实现后，§5.3、§5.4、§5.5、§6.3、§6.5 都要同步更新 |
| `manual-test-checklist.md` 16.6：一位观看者丢包时“所有人都收到关键帧” | 这是现状下的预期。实现后改为：原生观看者丢包时整组收到恢复帧，浏览器观看者丢包时仍收到关键帧 |

---

## 9. 实现清单（按顺序）

标注：**[单机]** 一台 Windows 机器、无特殊显卡即可验证；**[DXVA]** 需要本机显卡有对应格式的硬件解码（多数机器的 H.264/HEVC 满足）；**[NVIDIA] / [AMD] / [Intel]** 需要对应厂商的显卡。

1. **基线与工具 [单机]**
   1. 主控注入器：丢包与延迟，按原始序号过滤（§6.10）；
   2. `dev video` headless 主控与 JSON 指标；
   3. 在现状下跑一轮基线：冻结、字节、关键帧数。
2. **`frame-meta` 扩展 [单机]**
   1. 两端注册扩展；
   2. 采集线程维护纪元和帧号；`Encoded`、`Shared` 带 `meta`；首包写入；
   3. 主控解析进 `EncodedFrame`；
   4. 单元测试。
3. **协议 [单机]**
   1. 特性 `video.recovery`；
   2. SESSION tag 8 `VideoFeedback`，包括 `wire.rs` 的 oneof 与 `name()`；
   3. 主控经 `fct.send_control` 发送；
   4. 被控在 `ingress.rs` 中识别，按 SSRC 路由到 media worker。
4. **主控接收逻辑 [单机]**
   1. 维护 `D`、`L`，发确认；
   2. 不可恢复时发恢复请求，带兜底计时器，并保留 NACK；
   3. `SeqNumOnlyRefFinder` 放行恢复帧，清理搁置帧与 NACK；
   4. 指标；
   5. 合成包单元测试。
5. **解码端 [单机]+[DXVA]**
   1. 透传 `recovery`；
   2. DXVA H.264 对恢复帧放行跳号 [DXVA]；
   3. AV1 frame id 核对（可选）[DXVA]；
   4. 软件 H.264 不用改，加测试确认 [单机]。
6. **被控决策模块 [单机]**：纯逻辑，包括槽、确认、去重、选 LTR/失效/IDR、自动关闭；配单元测试。
7. **编码器接口 [单机]**：`FrameControl`、`RefCaps`、`Encoded.ltr_*`；各后端先实现为“只支持 keyframe”，行为与现状一致。
8. **软件 H.264 LTR [单机]**
   1. 按 §2.4 实现；
   2. 编码 → 丢帧 → 解码，逐位一致的测试；
   3. **第一次端到端单机验证**：被控用软件编码，主控分别用软件解码 [单机] 和 DXVA 解码 [DXVA]，配合注入器做 A/B（§6.10）。
9. **共享组 [单机]**
   1. `Membership` 递交确认和请求，组员转发 `meta`；
   2. 两个原生主控（多 profile）加一个 Chrome 网页观看者同屏，验证 §4.3 的每一行。
10. **恢复帧的保护 [单机]**：`transport.frame(keyframe || recovery)`。
11. **NVENC H.264/HEVC LTR [NVIDIA]**
    1. 初始化参数、逐帧标记与使用、输出核对；补函数表检查；
    2. 可选：`nvEncInvalidateRefFrames` 失效模式；
    3. 录制码流夹具。
12. **AMF H.264/HEVC/AV1 LTR [AMD]**：初始化属性、surface 属性、输出属性核对；验证与跳帧的相互作用；录制夹具。
13. **QSV H.264/HEVC LTR + `RejectedRefList` [Intel]**
    1. `NumRefFrame`；`mfxEncodeCtrl.ExtParam`；`EncodedFrameInfo` 核对；
    2. AV1 探测；
    3. 录制夹具。
14. **NVENC AV1 [NVIDIA，新驱动]**：重新生成 SDK 13.x 绑定，调整 `nvenc.rs:39` 与各结构体版本，实现 AV1 LTR。
15. **可选优化**
    1. 提前判定门限的 A/B [单机]；
    2. 组员接收队列缺口改为恢复请求 [单机]；
    3. 被控在 RTX 被预算拒绝时，主动给原生接收方发恢复帧 [单机]；
    4. 帧内刷新作为重度丢包模式 [三家各自验证]；
    5. 浏览器的 Dependency Descriptor（需要浏览器实测）。
16. **文档**
    1. `farwing-protocol.md` §5.3–§5.5、§6.3、§6.5；
    2. `peer-messages.md` SESSION；
    3. `v2-design.md` §4.2 与 §6.3.1；
    4. 手动测试清单 16.6，`improvement-proposals.md` F2/F4 的状态；
    5. 本文开头补充实施结果。
17. **双机与厂商矩阵验收**：真实链路的 2% 丢包与突发丢包（`manual-test-checklist.md` 5.5）；三家显卡 × 三种编码格式的恢复成功率与冻结分布；据此决定每个后端默认开还是关。
