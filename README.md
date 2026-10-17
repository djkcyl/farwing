# OpenUUYC

![OpenUUYC](assets/banner.png)

OpenUUYC 是用 Rust 编写的 UU 远程第三方客户端，支持 Windows 和 Linux，支持主控、被控和远程协助。

后续计划支持 Linux 等其他平台，移植工作请先通过 [Issue](https://github.com/djkcyl/openuuyc/issues) 沟通。

## 下载与使用

从 [Releases](https://github.com/djkcyl/openuuyc/releases) 下载 Windows x64 客户端，扫码或短信登录后连接设备，支持 UU 官方客户端；无需登录也可在登录页开启“接受远程协助”。

最新稳定版为 [v1.0.1](https://github.com/djkcyl/farwing/releases/tag/v1.0.1)。

最新测试版为 [v1.0.2-beta.5](https://github.com/djkcyl/farwing/releases/tag/v1.0.2-beta.5)，包含协助邀请链接、被控托盘提示、观看窗口剪贴板开关，以及网络参数、单轨切屏和文件时间修正。

接收测试版更新提醒：在“关于”中开启“允许测试版”（默认关闭）。

**从 alpha.6 或更早版本升级需重新登录，设备 ID 会改变，旧设备条目需手动删除。**

## 能力表

以下对应 **v1.0.1** 在 **Windows x64** 上的能力，功能取决于双方能力与权限；Linux 的能力见下面的[功能矩阵](#windows--linux-功能矩阵)。

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
| 被控终端 | 不提供 | 支持（需开启许可） | 手机端 UU 的「终端」连接：shell 以当前用户身份跑在 pty / ConPTY 上，断线保留、重连能接回；侧边栏「终端会话」页可查看和结束会话，Linux 上还能在本机新建、用系统终端接上，本机开的会话默认不对主控可见 |
| 批注与指示工具 | 支持发送 | 支持接收 | 画笔、形状、撤销重做、白板、激光笔和鼠标指示；被控端需登录 Windows |
| 远程电源操作 | 开机、关机和重启 | 关机、重启、远程开机设置与局域网唤醒协助 | 提供有线网卡检查/配置；WoL 需 BIOS 开启及同网协助设备或已配置的 UU 路由器 |

被控通知可选程序浮窗或 Windows 通知，支持连接确认、断开及文件收发提醒。

向官方被控端拖入文件，需开启键鼠控制、剪贴板同步和文件复制。

另支持设备管理、自定义快捷键、本机诊断、性能监控、异常图标及[插件和节点图](plugins/README.md)。

全屏观看时，悬停顶部横条可展开操作栏并退出全屏，移开后自动收起；游戏鼠标锁定时需先释放键鼠控制。

支持本机导出及远端获取脱敏诊断包（ZIP / zstd，最多 32 MiB）；远端获取需双方 beta.3+，并开启被控端文件访问权限。

## Windows / Linux 功能矩阵

对照本分支（v1.0.2-beta.5 加 Linux 移植）在两个平台、两种角色下的能力；功能有增减时同步更新这张表。✅ 支持，⚠️ 部分支持或有前提，❌ 不支持，— 该角色不涉及。Linux 的安装、权限与运行前提见[构建 → Linux](#linux)。

| 功能 | Windows 主控 | Windows 被控 | Linux 主控 | Linux 被控（Xorg） | Linux 被控（Wayland） |
| --- | --- | --- | --- | --- | --- |
| 登录方式 | ✅ 扫码 / 短信 | ✅ 未登录可游客协助 | ✅ 扫码 / 短信 | ✅ 未登录可游客协助 | ✅ 未登录可游客协助 |
| 同账号连接 | ✅ 可同时连多台 | ✅ 需开“允许被控” | ✅ 可同时连多台 | ✅ 需开“允许被控” | ✅ 需开“允许被控” |
| 设备 ID / 验证码协助 | ✅ 含最近与收藏 | ✅ 账号及游客 | ✅ 含最近与收藏 | ✅ 账号及游客 | ✅ 账号及游客 |
| 协助邀请链接 `uuremote://` | ✅ 可选接管协议 | — | ✅ 经 .desktop 登记 | — | — |
| 画面采集 | — | ✅ DXGI 桌面复制 | — | ✅ NvFBC → MIT-SHM → 门户 | ⚠️ wlr-screencopy → 门户，需 XWayland；缩放桌面需 XWayland ≥ 23.1 |
| 硬件编码 | — | ✅ NVENC / AMF / QSV | — | ✅ NVENC / VA-API H.264 | ✅ NVENC / VA-API H.264 |
| 软件编码 | — | ✅ H.264 4:2:0 | — | ✅ H.264 4:2:0 | ✅ H.264 4:2:0 |
| 硬件解码 | ✅ DXVA H.264/H.265/AV1 | — | ⚠️ 仅 VA-API H.264 | — | — |
| 软件解码 | ✅ H.264 / AV1 | — | ⚠️ H.264；AV1 仅 8 位 4:2:0 | — | — |
| 指定显卡 / GPU 调度 | ✅ | ✅ | ⚠️ 自动（桌面所在显卡） | ⚠️ 自动，无 GPU 调度 | ⚠️ 自动，无 GPU 调度 |
| H.265 | ✅ 硬解 | ✅ 硬编 | ❌ | ⚠️ 仅 NVENC | ⚠️ 仅 NVENC |
| AV1 | ✅ 硬解 / 软解 | ✅ 硬编（视显卡） | ⚠️ 软解 8 位 4:2:0 | ⚠️ NVENC 4:2:0，需 RTX 40 系及以上 | ⚠️ NVENC 4:2:0，需 RTX 40 系及以上 |
| 10 位 / HDR | ✅ | ✅ | ❌ | ❌ | ❌ |
| 4:4:4 真彩 | ✅ | ✅ 硬编 | ⚠️ H.264 软解，≤1080p | ⚠️ 仅 NVENC | ⚠️ 仅 NVENC |
| 键盘鼠标 | ✅ 含组合键、光标同步 | ✅ | ✅ | ✅ XTest，免设置 | ⚠️ uinput 需权限，否则仅 XWayland |
| 绝对 / 相对鼠标 | ✅ 游戏锁定、可选节流 | ✅ | ✅ 游戏锁定、可选节流 | ✅ | ⚠️ 绝对定位需 xdg-output |
| 触摸（多点） | ❌ 只发键鼠 | ✅ 移动端原生多点 | ❌ 只发键鼠 | ⚠️ 单指模拟 | ⚠️ 单屏未旋转时多点 |
| 文字输入（移动端输入法） | — | ✅ | — | ✅ XTest | ⚠️ 虚拟键盘协议，GNOME 退回 XTest |
| 拦截本机快捷键 | ✅ | — | ⚠️ Xorg 抓键盘 / Wayland 需合成器协议 | — | — |
| 多显示器切屏 | ✅ | ✅ | ✅ 顶栏屏标签，跟随远端拓扑 | ✅ | ✅ 门户需共享多屏 |
| 播放中切换设备 | ✅ | — | ⚠️ 顶栏与全屏浮动栏，未实测 | — | — |
| 多屏独立窗口 | ✅ 标签拖出 | — | ⚠️ 标签拖出，独立窗口各一块屏；仅硬解，Wayland 摆位由合成器决定 | — | — |
| 分辨率 / DPI 调整 | ✅ 远端设置、本地拉伸 | ✅ | ✅ 远端设置、本地拉伸 | ⚠️ RandR 分辨率，无 DPI | ⚠️ 物理屏保持原分辨率（XWayland 不能改，按拒绝处理），虚拟屏经 niri；无 DPI |
| 虚拟屏 / 超级屏 / 无屏兜底 | ✅ 操作远端 | ✅ 需显示驱动 | ✅ 操作远端 | ❌ | ⚠️ 仅虚拟屏：niri，内核 ≥ 6.19，需启用 openuuyc-vkms |
| 桌面声音 | ✅ 播放、调音质 | ✅ | ✅ Pulse / PipeWire / ALSA | ✅ 录监听源 | ✅ 录监听源 |
| 仅音频连接 | ✅ | ✅ | ✅ | ✅ | ✅ |
| 麦克风 / 虚拟声卡 | ✅ 发送麦克风 | ⚠️ 选装测试签名驱动 | ✅ 发送麦克风 | ❌ 无虚拟声卡 | ❌ 无虚拟声卡 |
| 剪贴板文字 / 图片 / 文件 | ✅ 含富文本 | ✅ 含富文本 | ⚠️ 无富文本，文件经 FUSE | ⚠️ 无富文本，文件经 FUSE | ⚠️ 同左，图片可能到不了 XWayland |
| 剪贴板同步开关（标题栏） | ✅ | — | ✅ | — | — |
| 文件拖放（拖入 / 拖出 / 拖回） | ✅ 三者均可 | ✅ 三者均可 | ⚠️ 拖入/拖出，无拖回 | ⚠️ 拖入/拖出，无拖回 | ⚠️ 仅拖出（需 layer-shell） |
| 向官方主控发文件（队列浮窗） | — | ✅ | — | ✅ | ❌ |
| 独立文件传输 | ✅ | ✅ 需已登录桌面 | ✅ | ✅ 根为 `\`，路径对外用反斜杠 | ✅ 同左 |
| 端口转发 | ✅ TCP / UDP | ✅ 需开许可 | ✅ TCP / UDP | ✅ 需开许可 | ✅ 需开许可 |
| 被控终端（UU 手机端的「终端」） | ❌ 不提供 | ⚠️ 需开许可，经 ConPTY；便携运行以当前用户身份（以管理员运行时就是管理员 shell），无人值守（SYSTEM）不提供；「终端会话」页只能查看、结束，本机新建与接入暂不支持；未实测 | ❌ 不提供 | ✅ 需开许可，以桌面用户身份运行；「终端会话」页可本机新建、用系统终端接上；本机开的会话另需「允许主控接入本机终端会话」才对主控可见 | ✅ 同左 |
| 批注 / 白板 | ✅ 发送 | ✅ 接收 | ❌ | ❌ 拒绝请求 | ❌ 拒绝请求 |
| 远程关机 / 重启 | ✅ | ✅ | ✅ | ✅ logind，受 polkit 约束 | ✅ logind，受 polkit 约束 |
| 远程开机（WoL） | ✅ | ✅ 含网卡配置 | ✅ | ⚠️ 网卡需手动 ethtool | ⚠️ 网卡需手动 ethtool |
| 息屏点亮与防休眠 | ✅ 观看时防休眠 | ✅ | ✅ 观看时防休眠 | ✅ DPMS 点亮 | ⚠️ 视桌面，Hyprland 等不能点亮 |
| 锁屏 / 登录界面被控 | — | ✅ 需安装服务 | — | ❌ 无常驻服务 | ❌ 无常驻服务 |
| 开机自启 | ✅ 登录后进托盘 | ✅ 服务开机即可接入 | ✅ 登录后进托盘 | ⚠️ 登录桌面后才能被控 | ⚠️ 登录桌面后才能被控 |
| 托盘与被控红角标 | ✅ | ✅ | ✅ SNI，GNOME 需扩展 | ✅ | ✅ |
| 访问通知 | — | ✅ 浮窗 / 系统通知 | — | ✅ 浮窗 / 桌面通知 | ✅ 浮窗位置由合成器定 |
| 性能监控 / 异常图标 / 全屏操作栏 | ✅ | — | ✅ | — | — |
| 设备资料与壁纸 | — | ✅ | — | ⚠️ 壁纸仅部分桌面 | ⚠️ 壁纸仅部分桌面 |
| 诊断包（本机导出 / 远端获取） | ✅ 含解码检查 | ✅ | ⚠️ 无解码检查页 | ✅ | ✅ |
| 插件与节点图 | ✅ | — | ⚠️ 可管理插件，播放器无节点图 | — | — |
| 1:1 实际像素 | ✅ | — | ❌ 按钮置灰 | — | — |
| 自定义 / 全局快捷键 | ✅ | — | ⚠️ 仅播放窗口有焦点时 | — | — |
| 凭据存储 | ✅ 凭据管理器 | ✅ 装服务后由服务保管 | ✅ Secret Service，无明文回退 | ✅ 同左 | ✅ 同左 |
| 安装服务 / 驱动 | ✅ | ✅ 服务、输入与显示驱动 | ❌ 用 deb / AUR 包 | ❌ | ❌ |
| 更新 | ✅ 提醒、更新并打开、命令行 | ✅ 同左 | ⚠️ 仅提醒，指向上游 Release | ⚠️ 同左 | ⚠️ 同左 |

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

## 构建

### Windows

需要 Rust stable（MSVC）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 NASM，命令行工具需加入 PATH。H.264 软件编解码和 AV1 软件解码使用项目 Rust 核心及 SIMD 汇编。

```powershell
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo dist
```

产物为 `target/dist/` 下的单文件 EXE。仅构建原生 EXE 可用 `cargo dist --native`；命令行参数见 `--help`。

源码已包含签名驱动包和公钥证书，构建主程序不需要签名私钥。修改驱动及发布前验证见 [驱动构建说明](drivers/README.md)。

### Linux

需要 Rust stable（edition 2024）与 C/C++ 工具链。

#### Arch / Omarchy

[`packaging/aur/`](packaging/aur/README.md) 里有两份 AUR 配方：`openuuyc-bin` 安装 Release 里的压缩包，`openuuyc-git` 跟踪 `linux-port` 分支源码构建。它们还没有发布到 AUR，可以直接在本地构建：

```bash
cd packaging/aur/openuuyc-bin && makepkg -si
```

Omarchy 用户可以把 [`packaging/omarchy/omarchy-menu.jsonc`](packaging/omarchy/omarchy-menu.jsonc) 里的条目加进 `~/.config/omarchy/extensions/omarchy-menu.jsonc`，从 `Super + Space → Install → Service` 安装。不用 AUR 时，在源码目录构建后（或解压 Release 里的 `OpenUUYC-<版本>-x86_64-linux.tar.zst` 后）运行 `./install-omarchy.sh`，它会安装程序、桌面入口、uinput 规则和 Omarchy 菜单项（`--uninstall` 卸载）。

自建：

```bash
sudo pacman -S --needed base-devel cmake clang nasm pkg-config rust \
    libva libx11 libxcb libxrandr libxi libxcursor libxfixes \
    libxkbcommon libxkbcommon-x11 wayland libglvnd vulkan-icd-loader \
    dbus alsa-lib openssl fuse3 noto-fonts-cjk
cargo build --release --bin OpenUUYC
```

#### Debian / Ubuntu

Ubuntu 22.04 及以上：

```bash
sudo apt install build-essential cmake clang nasm pkg-config libva-dev \
    libasound2-dev libdbus-1-dev libxkbcommon-dev libxkbcommon-x11-dev \
    libwayland-dev libx11-dev libxcb1-dev libxrandr-dev libxi-dev libxcursor-dev \
    libgl1-mesa-dev libvulkan-dev libudev-dev libssl-dev fonts-noto-cjk fuse3
git clone https://github.com/djkcyl/openuuyc.git
cd openuuyc
cargo build --release
./target/release/OpenUUYC gui
```

#### 被控端虚拟显示器（可选）

主控可以要求被控端凭空多出一块显示器。写内核的 configfs 只有 root 能做，所以包里另带一个小程序 `openuuyc-vkms` 和一份套接字激活的系统服务：平时不跑，第一次用到时由套接字拉起。条件：

- **Linux 6.19 或更新的内核**，且编译了 `CONFIG_DRM_VKMS`。更老的内核上 VKMS 没有 configfs 接口（`modinfo -F parm vkms` 里没有 `create_default_dev` 就是），程序不会去碰它，诊断页会写「内核不支持」；
- systemd（套接字激活）；
- 当前合成器能把新输出接进桌面布局，**目前只有 niri**。其他桌面（GNOME、KDE、sway、Xorg……）上主控不会出现「添加虚拟屏」这个入口。

deb 与 AUR 包都会装上它，但**不会替你启用**：不启用一切照旧，只是诊断页里「虚拟显示器」那一行显示「已安装，服务未启用」。启用：

```bash
sudo systemctl enable --now openuuyc-vkms.socket
```

从源码构建或手动安装时，把 `crates/vkms` 编出来的 `openuuyc-vkms` 装到 `/usr/bin/`、两份单元装到 `/usr/lib/systemd/system/`，再执行上面那条命令：

```bash
(cd crates/vkms && CARGO_TARGET_DIR=../../target cargo build --release --locked)
sudo install -Dm755 target/release/openuuyc-vkms /usr/bin/openuuyc-vkms
sudo install -Dm644 packaging/linux/openuuyc-vkms.socket \
    packaging/linux/openuuyc-vkms.service /usr/lib/systemd/system/
sudo systemctl daemon-reload && sudo systemctl enable --now openuuyc-vkms.socket
```

#### 被控端输入权限（Wayland 必需）

Wayland 会话下键鼠经 `/dev/uinput` 注入。安装包会带上 udev 规则，重载后当前登录用户即可使用：

```bash
sudo udevadm control --reload-rules && sudo modprobe uinput
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
```

规则使用 `TAG+="uaccess"`，由 systemd-logind 给本机活动会话授权，**不需要把用户加进 `input` 组，也不需要重新登录**。从源码构建或手动安装时，把
[`packaging/linux/60-openuuyc-uinput.rules`](packaging/linux/60-openuuyc-uinput.rules)
放到 `/etc/udev/rules.d/` 即可。没有权限时程序会记录一条警告并回退到 XTest（只影响 XWayland 客户端）。Xorg 会话不使用 uinput，无需这一步。

Wayland 下被控先用合成器自己的 wlroots screencopy（Sway、Hyprland、niri 等都提供），不需要门户，也不弹授权。合成器不提供或无法复制画面时（GNOME、KDE 等）改用 XDG 屏幕共享门户：

```bash
# GNOME / KDE
sudo pacman -S --needed xdg-desktop-portal-gnome   # KDE 为 xdg-desktop-portal-kde
# wlroots 系合成器也走门户时（如 OPENUUYC_CAPTURE=portal）：Hyprland 为 xdg-desktop-portal-hyprland，Sway 等为 xdg-desktop-portal-wlr
```

经门户采集时，首次被控会弹出一次共享授权，之后记在 `~/.local/share/OpenUUYC/screencast-restore-token`。

#### 硬件解码 / 编码

- **解码**：H.264 走 VA-API。AMD 用 `mesa`，Intel 用 `intel-media-driver`，NVIDIA 用 `libva-nvidia-driver`（Ubuntu 为 `nvidia-vaapi-driver`；并设 `NVD_BACKEND=direct`）；libva 2.15 以前认不出 NVIDIA 显卡对应的驱动，程序会自行指定，无需设置 `LIBVA_DRIVER_NAME`。
- **编码**（被控）：NVIDIA 用 NVENC；AMD 与 Intel 经 VA-API 编码 H.264（Intel 需 `intel-media-driver` 或 `libva-intel-driver`，AMD 用 `mesa`），都不可用时回退软件编码。
- **老 Intel GPU / 虚拟机**：Haswell/Broadwell 的 Vulkan 驱动未通过一致性认证，程序仍会使用它（设 `WGPU_ALLOW_UNDERLYING_NONCOMPLIANT_ADAPTER=0` 恢复 wgpu 默认的隐藏行为）；驱动无法向 Wayland surface 呈现时，可安装 `vulkan-swrast`（lavapipe）并用 `OPENUUYC_BACKEND=gl` 或 `OPENUUYC_BACKEND=vulkan` 指定界面后端。

#### 运行前提与开关

- **运行环境**：登录态存在系统密钥环（Secret Service，需要 gnome-keyring、KWallet 等，没有明文回退）。托盘用 StatusNotifierItem，GNOME 需要 AppIndicator 扩展（Ubuntu 默认启用），桌面没有托盘时关闭窗口即退出；面板比程序晚起来时每 2 秒重试，托盘一出现就改回关闭收进托盘。粘贴远端文件要 `fuse3`。Wayland 会话被控也要有 XWayland（显示器列表与输入注入器都先连 X 服务器）；缩放（含分数缩放）的桌面上，门户共享的屏幕按合成器 `xdg-output` 布局里的输出名与所选显示器配对，需要 XWayland 23.1 及以上（输出以连接器命名）且程序能拿到 `WAYLAND_DISPLAY`，配不上时报错，不会采错屏。拖出文件的预览图要 `gio` 与 `rsvg-convert`。
- **开机自启**：桌面接进 `graphical-session.target` 时是 systemd 用户单元 `openuuyc.service`（`journalctl --user -u openuuyc` 看日志），否则写 `~/.config/autostart/openuuyc.desktop`。两者都不处理的合成器（无 uwsm 的 sway、Hyprland 等）请自行启动 `OpenUUYC gui --background`。再次启动程序会经 `$XDG_RUNTIME_DIR/openuuyc/control.sock` 唤起已运行的控制中心。
- **协助邀请链接**：安装包的 `openuuyc.desktop` 登记了 `x-scheme-handler/uuremote`，没有 Windows 那样的接管开关，可用 `xdg-mime default <程序>.desktop x-scheme-handler/uuremote` 改。便携运行时把链接作为唯一参数传给程序。
- **被控终端**：默认关闭，在被控设置的访问权限里打开，打开时会确认一次；以 root 运行时不提供，本机也不能新建。shell 跑在 pty 上，断线后保留、重连可接回（重开同一会话即接管，旧连接不再收输出），接回时重画当前屏幕，不带回已滚出屏幕的输出；主控开的会话在退出或切换账号、关闭终端开关、禁止被控或在本机断开连接时结束，两端都没有附着 30 分钟后也会结束。设置页显示运行中的会话数，可一键结束全部。
  - **终端会话页**：侧边栏常驻，列出全部会话，可结束；也能在本机新建会话，或用系统终端接上任一会话（`$TERMINAL` 优先，再找常见的终端模拟器）。命令行是 `OpenUUYC terminal [编号或名字]`、`--list`、`--new [名字]`，控制中心没在跑时新建和接入会以 `gui --background` 把它拉起来。本机经 `$XDG_RUNTIME_DIR/openuuyc/terminal.sock` 接入，只有同一用户能连。本机与主控同时附着时输入输出同步，终端按两边较小的尺寸显示；关掉本地终端只断开这一端。本机开的会话不会因空闲被结束，shell 退出后从列表消失。
  - **本机会话对主控的可见性**：本机开的会话默认不对主控可见，主控看不到、也接不上。要让主控接入，在「终端」下再打开「允许主控接入本机终端会话」，打开时会再确认一次，关闭终端开关时它一并关闭。主控接入本机会话时访问通知会写明；关掉这一项后主控立即断开这些会话，shell 留给本机。
- **远程开机**：配置网卡需要 root，程序内不提供，请用 `sudo ethtool -s <网卡> wol g`。远程关机与重启经 systemd-logind，受 polkit 约束。
- **更新**：「关于」页的更新提醒指向上游的 Windows Release；Linux 版请从本 fork 的 Release 或 deb / AUR 包更新。下面的「命令行更新」只适用于 Windows。
- **环境变量**：`OPENUUYC_CAPTURE` 指定采集后端顺序（如 `nvfbc`、`wlr`、`x11,portal`）；`OPENUUYC_ZEROCOPY=1` 让 VA-API 解码画面以 dmabuf 直接交给渲染器（只在 Intel 上验证过）；`OPENUUYC_CAPTURE_DMABUF=1` 与 `OPENUUYC_GPU_CONVERT=1` 是门户采集的试验开关；`OPENUUYC_BACKEND=gl|vulkan` 指定界面后端。

`cargo dist` 只用于 Windows 打包，Linux 直接用 `cargo build`。

[`devtools/probes/`](devtools/probes/README.md) 里是验证 Linux 零拷贝与 GPU 颜色转换可行性的开发用探测程序（C 与一个独立的 Rust 小工程），不参与构建，用于在新驱动或新显卡上先确认 dmabuf 导出、Vulkan 导入和 VA-API 编码是否可用。

Arch / EndeavourOS 的构建依赖：

```bash
sudo pacman -S --needed base-devel rust nasm clang pkgconf libva alsa-lib dbus \
    libxkbcommon libxkbcommon-x11 wayland libx11 libxcb libxrandr libxi \
    libxcursor libglvnd vulkan-icd-loader openssl fuse3 noto-fonts-cjk
```

打包为 deb（GitHub Actions 的 “Linux deb” 工作流在 Ubuntu 22.04 上分别为 amd64 与 arm64 原生构建同样的包，产物见运行页面的 Artifacts，打 `v*` 标签时附到 Release，Release 说明末尾的「下载地址」按附上的文件自动生成。arm64 包只做了编译与安装检查，没有在 ARM 机器上实际运行过；Release 里的压缩包与单文件程序只有 x86_64）：

```bash
cargo install cargo-deb
cargo build --release --bin OpenUUYC
cargo deb --no-build
sudo apt install ./target/debian/openuuyc_*.deb
```

安装后从应用菜单启动，或运行 `OpenUUYC`。

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
