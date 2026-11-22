# Farwing 自建分支进度

更新于 2026 年 11 月 22 日。分支基于上游 `v1.0.2-beta.8`。Windows 主控和被控、iOS 主控、网页主控可使用同一个自建服务地址。

| 模块 | 当前实现 | 验证情况 |
| --- | --- | --- |
| Windows 接口 | 设备注册、资料、状态推送、设备管理、会话接管、协助、收藏和最近记录、电源和 WoL、壁纸、软件功能权限 | Node.js 23.3.0 和 24.22.3 均通过 11 项服务端测试，含启动和 CLI 配对。另用临时受信任证书复跑 HTTPS/WSS 的 Windows 接口、重连和接管测试 |
| Windows 媒体与数据 | 两个独立 Windows 原生客户端进程通过自建信令建立 WebRTC 会话 | 在同一台 Windows 上用 Node.js 23.3.0 执行 `node server/index.js`，分别通过 UDP 直连和强制 TURN 中继验证了 H.264 首帧解码、Opus 音轨建立、TCP 数据往返以及 512 KiB 文件的上传和下载。文件内容逐字节核对一致 |
| Windows 远程协助 | 设备验证码和被控端确认 | 同一台 Windows 上，测试进程完成实际验证码校验，通过被控端确认处理程序批准，并分别在直连和 TURN 中继下解码首帧 |
| 网页主控 | 同源页面、设备列表及 WebRTC 远程会话 | 有一场与 Windows 实机建立 WebRTC 直连并发送视频首帧的记录 |
| iOS 主控 | SwiftUI 界面、按地址保存凭据、启动加载主机列表、WebRTC 视频和远程输入 | Xcode 26.3 `iphoneos` 构建通过，编译器报告两处 Swift 并发警告。iPad 真机的视频首帧及输入待验证 |

## 复测

```powershell
npm --prefix server ci --include=dev
npm --prefix server test
cargo test --locked --lib two_windows_peers_decode_video_through_self_hosted_signaling -- --ignored --nocapture
```

原生测试需要在可采集桌面的 Windows 会话内运行。它启动临时服务，使用两个独立进程和设备身份，并在临时目录中存储配置和测试文件。在 PowerShell 中设置 `$env:FARWING_TEST_FORCE_RELAY = '1'` 并重跑原生命令即可强制使用本机 TURN。

双机界面操作和官方模式对照待验证。在两台 Windows 上运行同一版本的客户端并保存同一自建 HTTPS 地址。在被控端开启「允许被控」，在主控端检查设备自动上线。连接后验证画面渲染、声音播放、键鼠、剪贴板、文件传输、协助确认和断开重连，再交换主控与被控。跨网络时使用部署环境的 TURN 验证中继，在相同设备和网络上复测官方模式。

地址模式的准入范围和跨 NAT 配置见[服务端部署说明](README.md)。
