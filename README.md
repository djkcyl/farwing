<p align="center">
  <picture>
    <source media="(prefers-color-scheme: dark)" srcset="assets/brand/horizontal-dark.svg">
    <img alt="Farwing" src="assets/brand/horizontal-light.svg" width="380">
  </picture>
</p>

# Farwing

Farwing 是可自建、可独立部署的远程控制与桌面串流软件，定位类似 Moonlight 与 RustDesk：原生客户端、自建的 **Farwing Server** 和双方共享的 **Farwing Protocol**。

| 组件 | 位置 | 说明 |
| --- | --- | --- |
| Farwing Client | 仓库根目录 | Windows 主控、被控及远程协助；Linux 主控预览 |
| Farwing Server | [`server/`](server/README.md) | Rust 服务端：HTTPS 管理面、WSS 在线与会话协调、SQLite、TURN 凭据与可选内置 STUN/TURN；自带网页端（用户中心、管理后台、浏览器里的远程桌面） |
| Farwing Protocol | [`docs/protocol/`](docs/protocol/farwing-protocol.md)、[`crates/protocol`](crates/protocol) | 客户端与服务端共享的协议规范与线上类型 |

媒体通过 WebRTC 端到端传输（DTLS-SRTP），中继使用标准 TURN，服务端不接触、不重编码音视频。

项目的目标与方向见 [docs/vision.md](docs/vision.md)。

当前分支使用 **Farwing Protocol 3**，客户端与服务端须使用同一版本。旧版数据库没有迁移路径。早期迁移记录见 [docs/migration-status.md](docs/migration-status.md)，会话设计见 [docs/protocol/v2-design.md](docs/protocol/v2-design.md)；具体协议版本以 [`crates/protocol`](crates/protocol) 的定义为准。

## 快速开始

1. 部署服务端（详见 [server/README.md](server/README.md)）：运行 `farwing-server`，用浏览器打开 `https://<服务器>:8443/setup`，输入控制台打印的初始化码，创建管理员并完成设置，然后在网页端创建账号或生成邀请码。

   浏览器打开 `https://farwing.example.com:8443/` 即为网页端：管理员可以在“管理”中创建账号、生成邀请码、查看设备与会话、发布客户端更新；所有账号可以管理设备与好友，并在浏览器中连接自己或好友共享的设备（按授权观看与控制）。

2. 在 Windows 上运行 `Farwing.exe`，填写服务器地址（自签名证书需核对并信任指纹），用管理员创建的账号登录。命令行方式：

   ```powershell
   Farwing.exe server set farwing.example.com:8443 --pin <证书 SHA-256 指纹>
   Farwing.exe login --username alice
   Farwing.exe devices
   Farwing.exe connect --device-id <设备ID>
   ```

完整用法（被控、远程协助、多实例 profile）见 [docs/client-usage.md](docs/client-usage.md)。

## 能力

以下是 Windows 客户端与网页端的功能。Linux 主控预览的范围见下方构建说明。

- 账号与设备管理、在线状态、设备改名/移除、壁纸、同账号设备互访、好友与按设备授权、多人同时连接
- 远程协助：9 位设备号 + 验证码 / 本机确认 / 验证码加确认，游客协助，最近与收藏
- 局域网发现、地址直连与配对（不经服务器）
- 音视频串流：H.264 / H.265 / AV1、硬件与软件编解码、HDR、多显示器、显示设置、虚拟屏、本地比例拉伸（不修改远端分辨率）；全屏时悬停顶部展开操作栏；桌面声音与仅音频连接
- 远程输入（键鼠）、麦克风与虚拟声卡、剪贴板、文件传输、拖放、批注
- 端口转发（TCP/UDP）、远程电源操作与网络唤醒、无人值守、后台服务、安装与更新
- 服务端网页端：用户中心、管理后台（账号、邀请码、设备、会话、更新公告、在线设置、审计日志、管理员二次验证），以及浏览器远程桌面（账号设备、好友共享设备、短号 + 访问码的访客；观看，按授权的键鼠、剪贴板、文件与麦克风），见 [docs/web-console.md](docs/web-console.md)

虚拟显示、输入和音频驱动是可选组件，仓库不附带预编译驱动包，见下文“驱动”。

## 构建客户端

### Windows

需要 Windows 10/11 x64、Rust stable（MSVC，开发使用 1.95）、Visual Studio C++ 构建工具、Windows SDK、CMake 和 NASM，命令行工具需加入 PATH。

```powershell
cargo build --release          # 产物 target\release\Farwing.exe
cargo dist                     # 打包单文件 EXE，产物在 target\dist\
cargo dist --native            # 仅构建原生 EXE
cargo fast                     # 本地迭代，产物 target\fast\Farwing.exe
cargo fast-test --lib          # 本地迭代的单元测试
```

`release` 为了保持验证过的代码生成，主 crate 只用一个代码生成单元，改动一处也要重新优化整个 crate（约 9 分钟）。`cargo fast` 使用继承 `release` 的 `fast` 配置：并行代码生成、增量编译，并用 Rust 自带的 `rust-lld` 链接，改动一处后重编约 20 秒；测试与自查用它，发布包仍用 `cargo dist`。（mold 只支持 Linux；时间主要花在编译主 crate，链接只占 2 秒左右。）服务端同样提供 `cargo fast`（不做 LTO，改动一处约 5 秒）。

### Linux 主控预览

在 x86-64 Linux 上使用 Rust stable、C/C++ 工具链、CMake、NASM、`pkg-config` 和 ALSA 开发库构建：

```sh
cargo build --locked --release --bin Farwing
./target/release/Farwing
```

桌面界面需要 X11 或 Wayland、OpenGL；账号和设备身份使用系统的 Secret Service（如 GNOME Keyring 或启用 Secret Service 的 KWallet）。中文设备名需要系统安装 CJK 字体。账号连接使用同版本的 Farwing Server，自签证书需核对 SHA-256 指纹。也可以从未登录的设备中心使用短号协助、地址直连或局域网配对。

命令行用 `./target/release/Farwing --help` 查看。`server probe <地址>` 查看服务器证书，`server set <地址> --pin <指纹>` 保存自签服务器，`login --username <用户名>` 登录，`devices` 列出设备，`connect --device-id <设备ID>` 打开观看窗口。

Linux 主控支持桌面和声音、键鼠、串流设置及多屏观看。x86-64 提供 H.264 / AV1 的 8-bit 4:2:0 或 4:4:4 软件解码，最高协商到 1080p。Linux 被控、硬解、HDR、系统剪贴板和远程文件传输待移植。

服务端是独立的 Cargo 工作区，构建方法见 [server/README.md](server/README.md)。

## 驱动

虚拟显示、输入与虚拟声卡驱动的源码在 [`drivers/`](drivers/README.md)，其硬件 ID、设备名与接口 GUID 见 [docs/dev/identity.md](docs/dev/identity.md)。驱动需要 WDK 构建，并使用你自己的代码签名证书签名；把签名后的驱动包放入 `assets/drivers/` 后重新构建客户端即可嵌入。未嵌入驱动包的构建中，相关安装入口会提示该构建未包含驱动，其余功能不受影响。

## 文档

- [客户端使用说明](docs/client-usage.md)
- [服务端部署与管理](server/README.md)；[网页端](docs/web-console.md)
- [Farwing Protocol 规范](docs/protocol/farwing-protocol.md) 与 [对端消息定义](docs/protocol/peer-messages.md)；[v2 设计与决策记录](docs/protocol/v2-design.md)
- [迁移状态与验证结果](docs/migration-status.md)
- [系统组件身份](docs/dev/identity.md)

## 许可与来源

Farwing 以 [AGPL-3.0-only](LICENSE) 发布。

第三方代码及其派生部分保留各自的许可（FFmpeg 派生部分为 LGPL-2.1-or-later，rav1d/dav1d 为 BSD-2-Clause 等），详见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES) 与各目录下的许可文件。


## 鸣谢

- [SudoVDA](https://github.com/SudoMaker/SudoVDA) 与 [Microsoft Windows Driver Samples](https://github.com/microsoft/Windows-driver-samples)：虚拟显示驱动的源码基础，以及虚拟音频驱动的示例参考。
- [FFmpeg](https://ffmpeg.org/)：视频编解码基础算法及硬件解码适配。
- [rav1d](https://github.com/memorysafety/rav1d) / dav1d：AV1 软件解码核心及 SIMD 实现。
- [Cisco OpenH264](licenses/openh264-algorithms.txt)：软件编码器的码率控制、屏幕变化检测与运动搜索等算法参考。
- [WebRTC](https://webrtc.googlesource.com/src/+/refs/branch-heads/5481/)、[WebRTC-rs](https://github.com/webrtc-rs/webrtc) 与 [goog_cc](https://github.com/kixelated/goog_cc)：实时传输、音频接收处理与拥塞控制基础。
- Opus / [SpeexDSP](src/media/audio/COPYING.SpeexDSP)：音频编解码与重采样算法。
- egui / [egui-directx11](https://github.com/NekomaruQwQ/egui-directx11)：图形界面与渲染基础。
- [coturn](https://github.com/coturn/coturn) 与 [webrtc-rs turn](https://github.com/webrtc-rs/webrtc)：TURN 中继（外部部署或服务端内置）。

完整的第三方来源与许可说明见 [THIRD_PARTY_NOTICES](THIRD_PARTY_NOTICES)。
