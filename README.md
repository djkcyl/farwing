# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方 Windows 客户端，支持主控、被控和远程协助。

后续计划支持 Linux 等其他平台，移植工作请先通过 [Issue](https://github.com/djkcyl/openuuyc/issues) 沟通。

## 自建服务器

在 Windows 主控和被控端的「自建服务器」填入同一个服务地址。被控端开启「允许被控」后，主控端从设备列表连接。网页和 iOS 可使用同一服务地址。

[服务端部署和连接步骤](server/README.md) · [iOS 主控工程](ios/README.md)

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，扫码或短信登录后连接设备，支持 UU 官方客户端；无需登录也可在登录页开启“接受远程协助”。

最新稳定版为 [v1.0.1](https://github.com/djkcyl/farwing/releases/tag/v1.0.1)。

最新测试版为 [v1.0.2-beta.7](https://github.com/djkcyl/farwing/releases/tag/v1.0.2-beta.7)。Windows 被控端支持同卡 NVENC 并行采集与编码，改善高刷屏采集节奏和带宽探测精度。发送拥塞恢复后，即使桌面没有再次重绘，主控也能收到最终画面。

当前源码面向下一版 v1.0.2-beta.8：跟进官方 4.48.1 的账号权益查询、连接内同步及到期处理，线上申报为 4.48.1.5606 / V4.14.2。

接收测试版更新提醒：在“关于”中开启“允许测试版”（默认关闭）。

**从 alpha.6 或更早版本升级需重新登录，设备 ID 会改变，旧设备条目需手动删除。**

## 能力表

功能取决于双方能力与权限。

| 功能 | 主控端 | 被控端 | 说明 |
| --- | --- | --- | --- |
| 同账号远程连接 | 支持同时连接多台设备 | 支持 | 本机在连接设置中开启“允许被控” |
| 设备 ID / 验证码协助 | 支持主动连接，也能直接点开邀请链接 | 支持账号及游客协助 | 随机 / 自定义密码、本机确认；主控支持最近连接和收藏，对方分享的 `uuremote://external/assistance?...` 链接会拉起本程序并填好设备 ID 与验证码 |
| 画面传输 | 接收与播放 | 采集与编码 | H.264 / H.265 / AV1、真彩及 HDR，可调画质、码率和帧率；AV1 与 SDR 自动 10 位需双方 OpenUUYC 支持 |
| 编解码 | DXVA11 硬解 / Rust 软解 | NVENC、AMF、QSV 硬编 / Rust H.264 软编 | 可选格式与显卡首选；软解支持 H.264 / AV1，整个客户端最多一个软解播放窗口 |
| 键鼠与触摸 | 键盘、鼠标控制 | 键鼠及移动端原生触摸接收 | 相对/绝对鼠标、组合键、光标同步；可选 125 / 500 / 1000 Hz 鼠标节流（默认关闭） |
| 多显示器与显示设置 | 切屏、多窗口、本地比例拉伸、分辨率及 DPI 调整 | 多屏采集与设置接收 | 屏幕标签可拖出独立窗口；本地比例拉伸不修改远端分辨率，物理设置按目标支持的配置应用 |
| 虚拟屏、超级屏与无屏接管 | 支持操作远端 | 支持 | 需安装显示驱动；自动兜底屏断线保留，其他可用屏幕接入后回收 |
| 桌面声音 | 播放与远程音质调整 | 采集与发送 | 默认或指定设备，设备变化后恢复；64 / 128 / 192 / 256 kbps，远程调音质需双方 alpha.6+ |
| 仅音频连接 | 支持 | 支持 | 默认 256 kbps，提供波形、电平、网络 RTT、精简模式，可恢复画面 |
| 麦克风与虚拟声卡 | 选择麦克风、调整音质并发送 | 虚拟扬声器及远程麦克风接收 | 被控端需选装虚拟音频驱动，可配置默认音频设备切换策略 |
| 剪贴板与文件拖放 | 支持 | 支持 | 文字、富文本、图片及文件复制；OpenUUYC 双端支持双向拖放，与官方端连接时使用其支持的发送方式 |
| 独立文件传输 | 支持 | 支持官方 Windows / Android / iOS 收发 | 目录浏览、收发、文件管理及暂停续传；被控端需登录 Windows |
| 端口转发 | 支持 | 支持 | TCP 与官方互通；UDP 需两端 OpenUUYC，复用可靠通道；被控需开启许可 |
| 批注与指示工具 | 支持发送 | 支持接收 | 画笔、形状、撤销重做、白板、激光笔和鼠标指示；被控端需登录 Windows |
| 远程电源操作 | 开机、关机和重启 | 关机、重启、远程开机设置与局域网唤醒协助 | 提供有线网卡检查/配置；WoL 需 BIOS 开启及同网协助设备或已配置的 UU 路由器 |

被控通知可选程序浮窗或 Windows 通知，支持连接确认、断开及文件收发提醒。

向官方被控端拖入文件，需开启键鼠控制、剪贴板同步和文件复制。

另支持设备管理、自定义快捷键、本机诊断、性能监控、异常图标及[插件和节点图](plugins/README.md)。

全屏观看时，悬停顶部横条可展开操作栏并退出全屏，移开后自动收起；游戏鼠标锁定时需先释放键鼠控制。

支持本机导出及远端获取脱敏诊断包（ZIP / zstd，最多 32 MiB）；远端获取需双方 beta.3+，并开启被控端文件访问权限。

## 安装与服务

主页“安装服务”会将程序安装到 `Program Files\OpenUUYC`，安装后台服务、输入和显示驱动，创建快捷方式并配置托盘自启动。安装需管理员授权及驱动证书信任；关闭主窗口进入托盘，托盘“退出”结束运行。

“连接设置 → 启动 → 开机自启”控制登录 Windows 后是否在托盘启动：安装服务后指向已安装的程序，未安装时指向当前运行的程序。后续更新保留关闭状态；若 Windows 已禁用自启动，页面会提示并提供系统启动设置入口。关闭它不影响后台服务，已授权并允许被控的常驻服务仍可在开机后接入。

安装服务并开启被控后，支持锁屏、PIN 界面及重启后未登录 Windows 时接入。

“安装服务”的确认对话框里可勾选「用 OpenUUYC 打开 UU 远程协助邀请链接」：勾上后把本用户的 `uuremote://` 协议登记给本程序，别人分享的邀请链接就会直接拉起 OpenUUYC 并填入设备 ID 和验证码。登记在安装成功后指向已安装程序，也可在连接设置中更改。只写本用户注册表，不动官方客户端的 HKLM 登记；接管前备份已有的 HKCU 登记，取消或卸载时恢复。若其他程序后来接管，则保留其设置。链接里的 `controlledLogin` 只描述被协助端当时的登录状态，不影响连接方式。

更新时运行新版 EXE，点击“更新并打开”即可更新已有安装。虚拟声卡在“连接设置 → 服务管理”中选装；卸载可选择保留虚拟显示驱动、虚拟声卡及用户数据。

### 虚拟音频驱动的使用条件

**虚拟音频驱动使用测试签名，尚未取得 Microsoft 正式签名。** 使用虚拟扬声器或接收远程麦克风，需要手动准备测试启动环境；普通画面、键鼠和桌面声音不需要此设置。

- 临时测试：Shift + 重启 → 疑难解答 → 高级选项 → 启动设置 → 重启 → **7 / F7 禁用驱动程序强制签名**，仅本次启动有效。[Windows 说明](https://support.microsoft.com/en-us/windows/experience/startup-boot/windows-startup-settings)
- 持续测试：管理员执行 `bcdedit /set testsigning on` 后重启；恢复时执行 `bcdedit /set testsigning off` 并重启。Secure Boot、BitLocker 或组织策略可能限制操作。[微软说明](https://learn.microsoft.com/en-us/windows-hardware/drivers/install/the-testsigning-boot-configuration-option)

测试启动会降低驱动加载保护，程序不会自动修改这些设置。

## 自托管远程连接

可运行自托管信令服务，让 Windows 主机与浏览器或 iOS 主控连接。先安装 Node.js 22.13 或更新版本，再启动服务：

```sh
npm --prefix server ci
npm --prefix server start
```

服务会在前台运行。另开终端生成一次性主机配对码：

```sh
node server/cli.js pair-code --role host
```

如果服务运行在 Windows 主机本机，可首次运行 `OpenUUYC.exe self-host --server-url http://127.0.0.1:8787 --pair-code 配对码 --name 主机名称`。跨设备访问时请将 `--server-url` 改为服务配置的 HTTPS 地址。配对信息会保存到 Windows Credential Manager，之后可省略配对码。主机配对后，用 `node server/cli.js list` 查找设备 ID，再生成绑定该主机的控制器配对码：

```sh
node server/cli.js pair-code --role controller --host DEVICE_ID
```

在 Windows 主机上打开服务地址，用浏览器输入控制器配对码即可绑定。跨设备连接时，主机、浏览器和 iOS 控制器都应使用同一已配置的 HTTPS/WSS 地址。iOS 控制器可在「服务端」中填写地址并配对。自托管服务不会访问网易 UU 或其他上游账号服务。详细配置见[服务端说明](server/README.md)和[iOS 控制器说明](ios/README.md)。

## 构建

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 NASM，命令行工具需加入 PATH。H.264 软件编解码和 AV1 软件解码使用项目 Rust 核心及 SIMD 汇编。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

产物为 `target/dist/` 下的单文件 EXE。仅构建原生 EXE 可用 `cargo dist --native`；命令行参数见 `--help`。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

## 命令行更新

从新下载的程序包运行 `OpenUUYC.exe update --silent`，静默更新已有安装。会中断当前连接，完成后不自动打开控制中心或重启 Windows；非管理员运行时仍需确认系统 UAC。

无人值守脚本使用 `update --silent --no-elevate`，由安装所属用户的管理员终端执行；权限不足返回 740，不弹 UAC。退出码：0 成功、3010 需重启、170 已有安装操作、1605 未安装、2404 驱动占用，其他失败为 1。不要同时保留其他更新窗口。

## 反馈与许可

问题和建议请提交到 [Issues](https://github.com/djkcyl/openuuyc/issues)。报告问题时附上双方版本、操作步骤、大致发生时间及导出的诊断包，分享前检查是否含私人信息。

OpenUUYC 非网易官方项目。源码公开，但项目整体未采用开源许可证，使用与分发条件见 [LICENSE](LICENSE)。第三方及派生代码保留各自的许可，详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。

## 鸣谢

感谢以下团队、项目和贡献者为 OpenUUYC 提供的参考、技术基础与开发支持：

- [网易 UU 远程](https://uuyc.163.com/)：本项目的协议兼容目标，也是远程连接功能与交互行为的参考。
- [SudoVDA](https://github.com/SudoMaker/SudoVDA) 与 [Microsoft Windows Driver Samples](https://github.com/microsoft/Windows-driver-samples)：虚拟显示驱动的源码基础，以及虚拟音频驱动的示例参考。
- [FFmpeg](https://ffmpeg.org/)：视频编解码基础算法及硬件解码适配的重要来源。
- [rav1d](https://github.com/memorysafety/rav1d) / dav1d：AV1 软件解码核心及 SIMD 实现。
- [Cisco OpenH264](licenses/openh264-algorithms.txt)：软件编码器的码率控制、屏幕变化检测与运动搜索等算法参考。
- [WebRTC](https://webrtc.googlesource.com/src/+/refs/branch-heads/5481/)、[WebRTC-rs](https://github.com/webrtc-rs/webrtc) 与 [goog_cc](https://github.com/kixelated/goog_cc)：实时传输、音频接收处理与拥塞控制基础。
- Opus / [SpeexDSP](src/media/audio/COPYING.SpeexDSP)：音频编解码与重采样算法。
- egui / [egui-directx11](https://github.com/NekomaruQwQ/egui-directx11)：图形界面与渲染基础。
- [OpenAI](https://openai.com/)：Codex 为本项目的开发提供 AI 编程辅助。
- [sisi0318](https://github.com/sisi0318)：提供两个 Codex x20 账号，支持本项目开发。
- ~~[Tibo（Thibault Sottiaux）](https://x.com/thsottiaux)：感谢他与 Codex 团队提供的额外额度重置。~~

也感谢[项目贡献者](https://github.com/djkcyl/openuuyc/graphs/contributors)及提供测试反馈和改进建议的用户。完整的第三方来源与许可说明见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
