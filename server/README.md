# Farwing 自建服务器

Windows 主控和被控各保存一次相同的服务地址。Windows 客户端自动注册设备，主控从原生设备列表连接。iOS 和网页也使用同一个地址。服务端使用 SQLite 保存设备身份，并提供设备发现、在线状态及 WebRTC 信令。

## 启动服务

在项目根目录执行。需要 Node.js 22.13 或更新版本。

```sh
npm --prefix server ci --omit=dev
node server/index.js
```

默认地址为 `http://127.0.0.1:8787`，可用于单机检查。已有的 Node.js `v23.3.0` 可通过 `node --experimental-sqlite server/index.js` 启动。

供多台设备使用时，复制 `server/.env.example` 为 `server/.env`。在其中配置实际的 HTTPS 证书、`PUBLIC_ORIGIN` 和 `DESKTOP_CLIENT_NETWORKS`，然后执行以下命令。Node.js `v23.3.0` 在 `--env-file` 前加 `--experimental-sqlite`。

```sh
node --env-file=server/.env server/index.js
```

也可通过系统环境变量或服务管理器提供同样的配置。客户端系统需要信任服务证书。`PUBLIC_ORIGIN` 填用户实际访问的 `https://域名[:端口]`。直连 TLS 时将 `HOST` 设为服务端的 VPN 或局域网地址。使用反向代理时将 Node 服务保持在回环地址，并转发 `/socket.io/` 和 `/v1/session` 的 WebSocket 升级请求。

`DESKTOP_CLIENT_NETWORKS` 接受逗号分隔的来源 IP 或 CIDR。地址模式下，获准网络中的客户端可以登记并连接已开启「允许被控」的设备。应填写自己的 VPN、受信任局域网或明确的客户端 IP。代理部署时由代理对实际客户端执行同样的访问限制，Node 端仅放行代理地址。后端按 TCP 对端地址判定准入。留空时，仅使用回环地址的本地实例允许地址注册。

局域网或路由互通的 VPN 通常可以直接建立媒体连接。需要跨 NAT 或仅中继连接时，在 `SELF_HOST_ICE_SERVERS` 配置自有 STUN/TURN。每项填写一个 `urls` 字符串，TURN 项还需要 `username` 和 `credential`。

## 连接设备

1. 将自建分支的 Windows 可执行文件复制到主控和被控电脑并运行。只在运行服务器的设备上安装 Node.js。在启动页的「自建服务器」，或控制中心的「连接设置」中填同一 HTTPS 地址，点击「连接自建服务器」。
2. 在被控电脑上开启「允许被控」。在主控的「设备」列表中，选择在线的被控电脑并连接。
3. 在 iPad 或 iPhone 上，在 Farwing 控制器的「服务端」保存同一个地址。网页主控直接用浏览器打开该地址，按「连接自建服务器」。

地址模式的日常连接无需配对码。设备令牌按服务器地址保存在客户端。使用 `node server/cli.js list` 查看注册设备，使用 `node server/cli.js revoke DEVICE_ID` 撤销设备。使用 Node.js `v23.3.0` 时同样为 CLI 加 `--experimental-sqlite`。

## 已有配对码部署

旧版前台 Windows host 和现有配对码仍可使用原有接口。服务端签发 host 码后，可用 `list` 查看设备 ID，再签发仅可访问该 host 的 controller 码。

```sh
node server/cli.js pair-code --role host
node server/cli.js list
node server/cli.js pair-code --role controller --host DEVICE_ID
```

服务默认使用 `server/data/devices.sqlite`，可以用 `OPENUUYC_DB_PATH` 指定存放位置。升级时沿用原数据库可保留设备和撤销记录。地址模式目前覆盖连接所需的设备注册、目录、在线状态、控制开关、会话接管和媒体信令。远程协助码、远程唤醒、电源操作、官方会员和升级服务尚未接入。
