# OpenUUYC iOS 控制器

原生 SwiftUI 控制器适用于 iPhone 和 iPad。它只连接用户填写的服务端，不含 NetEase、UU 或其他第三方服务地址。

## 打开和构建

需要安装完整 Xcode（含 iOS SDK）。项目使用社区维护的 WebRTC SPM 二进制发行版 `stasel/WebRTC` **精确版本 158.0.0**，避免每次解析时自动升级：

1. 用 Xcode 打开本目录下已生成并签入的 `OPENUUYCController.xcodeproj`。
2. 等待 Xcode 解析 WebRTC 包，再选择已安装的 iPhone/iPad 模拟器或连接的设备运行。真机运行需在 Xcode 中配置 Apple 开发团队和签名。

`project.yml` 是项目生成配置。需要从配置重新生成工程时，可使用 XcodeGen 2.x 并在本目录运行 `xcodegen generate`。

可先通过命令行进行未签名的 iOS app 编译：

```sh
xcodebuild -project OPENUUYCController.xcodeproj \
  -target OPENUUYCController \
  -configuration Debug \
  -sdk iphoneos \
  CODE_SIGNING_ALLOWED=NO build
```

在 Xcode 中运行时，选择 `OPENUUYCController` scheme 和当前已安装的模拟器或设备平台。实际安装到设备需要配置有效的 Apple 签名。

上游在 [158.0.0 标签的 Package.swift](https://github.com/stasel/WebRTC/blob/158.0.0/Package.swift) 中发布了 `WebRTC-M158.xcframework.zip`，SHA-256 为 `dd26ba28fb9c8dbc82f4be3085e8d112a4e2a4e654e1786a9977a748c24c4912`。M158 头文件包含本项目用到的 PeerConnection、ICE、数据通道、接收端编解码偏好和 Metal 视频渲染 API。控制器将接收编解码偏好限制为 Opus、H.264 和对应 H.264 RTX。项目直接使用该 SPM 包；TLS 证书必须受 iOS 系统信任，组织内部 CA 可以安装到设备后使用。项目保留系统 ATS 和证书校验设置，不会关闭 TLS 验证或放宽 ATS。

## 使用

在「服务端」填写与 Windows 被控端相同的 HTTPS 地址，点击「连接自建服务器」。选择该服务，再从在线且已开启「允许被控」的设备中连接。地址模式无需配对码。仍在使用旧配对流程时，可以展开「使用配对码」。身份凭据按服务器地址保存于 iOS Keychain。

应用进入后台时会先释放当前输入并结束会话。连接界面会显示信令、媒体连接状态、错误和首个视频帧；只有首帧到达后才显示「已连接」。

服务端使用 HTTPS/WSS。iOS 本地网络访问会显示系统权限提示；请为所配置的服务端允许访问。TLS 证书必须受 iOS 信任；组织内部 CA 可以安装到设备后使用。项目没有开放任意 HTTP、关闭 TLS 校验或添加 ATS 放宽项。

## 验证范围

已用 Xcode 26.3 完成 `iphoneos` 构建。服务端集成测试已覆盖 iOS 设备地址登记、获取设备列表和发起会话。iPad 上的视频和输入需在真机验证。
