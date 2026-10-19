# 上游同步记录

Farwing 的客户端派生自 OpenUUYC。本文件记录每次跟进上游所做的取舍。

## OpenUUYC v1.0.0（2026-10-14）

- **范围：** `v1.0.0-rc.3`（506df56，Farwing 第一轮的基线）→ `v1.0.0`（9d436cb）。
  - 包含 rc.4、rc.5 和 1.0.0 发布准备，共 8 个提交、72 个文件。
  - 上游合入的两个外部 PR（#6 Mac 键盘、#8 CVO 旋转）也在此范围内。
- **方法：** 在 `temp/upstream/` 中另建克隆，未改动参考目录。
  - 以 rc.3 为基准，对 Farwing 的每个文件做三方合并。
  - 合并后逐行核对：上游新增的每一行要么已进入 Farwing，要么属于下表“未采用”或“改为 Farwing 实现”的内容（`.tools/scripts/upstream_coverage.py`）。
- **上游项目更名：** 上游仓库已更名为 `djkcyl/farwing`，`next` 分支开始介绍 Farwing。`v1.0.0` 本身仍是兼容 UU 的 OpenUUYC，许可声明未变。

### 已采用

| 改动 | 说明 |
| --- | --- |
| 显示恢复与首选分辨率 | 兜底屏和虚拟屏恢复更稳健；读取显示器驱动报告的首选模式（按旋转方向调整） |
| 本地比例拉伸 | 观看菜单可按被控首选分辨率或固定比例在本地拉伸，不改远端分辨率；设置按设备保存 |
| 全屏操作栏 | 全屏时悬停顶部横条展开操作栏并可退出全屏 |
| 批注 | 改由会话内的被控常驻进程直接绘制，带 UIAccess，可覆盖在全屏游戏之上；移除单独的批注用户进程和管道 |
| 剪贴板 | 用户代理改为事件唤醒，替代 10 ms 轮询；拖放输出就绪时立即唤醒 |
| GPU 调度 | 被控默认“流畅优先”（Realtime）。新增“兼容优先”选项（被控设置 → GPU 调度），用于会冻结的显卡驱动 |
| 编码 / 解码 | QSV、AMF、编码器选择、GPU 转换与读回、DXVA 解码的修复；新增 GPU 诊断计时 |
| 输入 | 失焦和检查点修复时，先释放普通键、后释放修饰键 |
| DPI | 程序清单声明 PerMonitorV2，后台线程从启动起就使用物理坐标 |
| 更新检查 | 失败时写日志 |

### 改为 Farwing 实现

| 上游 | Farwing |
| --- | --- |
| UU 会话 JSON 中的 `openuuyc_display_geometry: 1` | 会话扩展 `extensions.display_geometry`（`farwing-protocol`）。`Extensions` 改为保留未知键，旧服务器和旧客户端会原样转发新扩展 |
| 屏幕消息字段 `openuuyc_geometry`（tag 100） | `Screen.display_geometry`（tag 100），见 [peer-messages.md](protocol/peer-messages.md) |
| `Global\OpenUUYC.Clipboard.Wake.*` | `product::global_object("Clipboard.Wake.")`，按 profile 区分 |
| 环境变量 `OPENUUYC_GPU_TIMING` | `FARWING_GPU_TIMING` |
| 批注、用户后端管道 `\\.\pipe\OpenUUYC.*` | 保留 Farwing 管道命名；批注管道随上游一起移除 |

### 未采用

| 改动 | 原因 |
| --- | --- |
| Mac 被控键盘映射（`protocol/keyboard.rs`，PR #6） | 只适用于官方 UU Mac 被控：它直接把收到的按键码交给 CGEvent。Farwing 的 `INPUT.key` 定义为 Windows VK，目前也只有 Windows 被控。将来做 macOS 被控时应在被控端映射，可复用上游映射表。上游同一提交中的按键释放顺序修复已采用 |
| CVO 角度取反（PR #8） | 针对 UU 发送端把 90°/270° 互换的私有约定。Farwing 被控不写 CVO，协议规定按 3GPP / WebRTC 标准解释，见 [farwing-protocol.md](protocol/farwing-protocol.md) §4.6 |
| GitHub 更新地址 | Farwing 的更新来自自建服务器的 `/api/v1/updates/windows` |
| README、版本号 1.0.0、驱动 README 中的发布说明 | 属于 OpenUUYC 的发布文档。Farwing 版本号仍为 `1.0.0-rc.3`，待验收后决定 |
