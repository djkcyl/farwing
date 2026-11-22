# Farwing 自建服务器

Windows 主控和被控各保存一次相同的服务地址。客户端自动注册设备，主控从原生设备列表或远程协助发起连接。Node.js 服务提供 Windows 使用的 `/api/v1`、`/api/v2` 和 Socket.IO 接口，SQLite 保存设备身份、设置及收藏。iOS 和网页仍可使用同一个地址。

## 启动服务

在项目根目录执行。需要 Node.js 22.13 或更新版本。

```sh
npm --prefix server ci --omit=dev
node server/index.js
```

默认地址为 `http://127.0.0.1:8787`，可用于单机检查。服务入口和配对 CLI 会自动处理 Node.js `v23.3.0` 所需的 SQLite 启动标志。

供多台设备使用时，复制 `server/.env.example` 为 `server/.env`。在其中配置实际的 HTTPS 证书、`PUBLIC_ORIGIN` 和 `DESKTOP_CLIENT_NETWORKS`。服务启动时自动读取该文件。

```sh
node server/index.js
```

也可通过系统环境变量或服务管理器提供同样的配置。客户端系统需要信任服务证书。`PUBLIC_ORIGIN` 填用户实际访问的 `https://域名[:端口]`。直连 TLS 时将 `HOST` 设为服务端的 VPN 或局域网地址。使用反向代理时将 Node 服务保持在回环地址，并转发 `/socket.io/` 和 `/v1/session` 的 WebSocket 升级请求。

`DESKTOP_CLIENT_NETWORKS` 接受逗号分隔的来源 IP 或 CIDR。地址模式下，获准网络中的客户端可以登记并连接已开启「允许被控」的设备。应填写自己的 VPN、受信任局域网或明确的客户端 IP。代理部署时由代理对实际客户端执行同样的访问限制，Node 端仅放行代理地址。后端按 TCP 对端地址判定准入。留空时，仅使用回环地址的本地实例允许地址注册。

局域网或路由互通的 VPN 通常可以直接建立媒体连接。需要跨 NAT 或仅中继连接时，在 `SELF_HOST_ICE_SERVERS` 配置自有 STUN/TURN。每项填写一个 `urls` 字符串，TURN 项还需要 `username` 和 `credential`。

服务启动后，在主控设备的浏览器打开服务地址下的 `/healthz`，确认 HTTPS 请求返回 `{"status":"ok"}`。

## 构建 Windows 客户端

在包含自建接口的分支上，按[项目构建说明](../README.md#构建)安装 Windows 工具链，并在项目根目录执行 `cargo dist --native`。Windows x64 可执行文件位于 `target/dist/native/`。

## 连接设备

1. 将自建分支的 Windows 可执行文件复制到主控和被控电脑并运行。只在运行服务器的设备上安装 Node.js。在启动页的「自建服务器」，或控制中心的「连接设置」中填同一 HTTPS 地址，点击「连接自建服务器」。
2. 在被控电脑上开启「允许被控」。在主控的「我的设备」列表中，找到在线的被控电脑并点击「连接」。新电脑未显示时，点击「刷新设备」。
3. 在 iPad 或 iPhone 上，在 Farwing 控制器的「连接自建服务器」填写同一个地址，点击「连接服务器」。网页主控直接用浏览器打开该地址，连接自建服务器后从主机列表发起会话。

地址模式的日常连接无需配对码。设备令牌按服务器地址保存在客户端。同一自建服务的准入网络对应一个设备目录。客户端的「全部设备」支持详情、改名和移除。也可以用 `node server/cli.js list` 查看注册设备，用 `node server/cli.js revoke DEVICE_ID` 撤销设备。

使用远程协助时，在被控端开启「允许被控」和「接受远程协助」，按需选择验证码、本机确认或两者。主控端通过「开始协助」输入九位设备 ID，使用被控端选择的验证方式。收藏和最近连接按发起端设备身份保存，验证码由 Windows 客户端按既有设置保存在本机。

电源操作沿用 Windows 被控端设置。「关机和重启」需要被控端允许。远程开机需要目标的有线网卡和 BIOS 支持 WoL，并且同网段有一台在线 Windows 客户端开启「局域网唤醒协助」。

显示、声音、输入、剪贴板、文件传输和端口转发沿用 Windows 客户端的原生媒体与数据通道，功能按双方硬件能力和被控端权限开放。自建模式使用本地设备身份和本地功能权限。网易扫码、短信登录、会员购买、UU 路由器和官方下发更新由各自的服务提供。

## 已有配对码部署

旧版前台 Windows host 和现有配对码仍可使用原有接口。服务端签发 host 码后，可用 `list` 查看设备 ID，再签发仅可访问该 host 的 controller 码。

```sh
node server/cli.js pair-code --role host
node server/cli.js list
node server/cli.js pair-code --role controller --host DEVICE_ID
```

服务默认使用 `server/data/devices.sqlite`，可以用 `OPENUUYC_DB_PATH` 指定存放位置。升级时沿用原数据库可保留设备和撤销记录。

各客户端的验收记录见[自建分支进度](PROGRESS.md)。
