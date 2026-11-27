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

`DESKTOP_CLIENT_NETWORKS` 接受逗号分隔的来源 IP 或 CIDR。新客户端从准入网络提交设备审批请求，经管理员批准后才能登记和连接。应填写自己的 VPN、受信任局域网或明确的客户端 IP。代理部署时由代理对实际客户端执行同样的访问限制，Node 端仅放行代理地址。后端按 TCP 对端地址判定准入。留空时，仅使用回环地址的本地实例允许地址注册。

### 自建账号与 Passkey

在浏览器打开部署地址的 `/account`。用户可以用用户名和密码注册、登录，也可以用 Passkey 注册、登录或为已有账号添加 Passkey。

Passkey 需要一个可解析的 HTTPS 域名以及浏览器信任的证书。账号页默认使用 `PUBLIC_ORIGIN`。远控使用 IP 地址时，可配置 `ACCOUNT_PUBLIC_ORIGIN=https://login.example.com`，并让该域名的 HTTPS 代理把 `/account`、`/account/*` 和 `/v1/account/*` 转发给同一个 Node 实例。账号域名同时用作 Passkey 的 RP ID，应保持稳定。直接通过局域网 IP 打开的账号页提供密码注册和登录。浏览器独立验证 HTTPS 证书。

### 自有设备扫码登录

先在 `/account` 创建所有者账号，再将 `DEVICE_ACCOUNT_OWNER` 设置为其用户名，并配置 `ADMIN_PORT=8789`。启用后，设备目录和远控仅对该账号下获准的设备开放。已有准入的设备扫码登录时沿用设备 ID。账号模式需要支持扫码登录的客户端。

Windows 输入自建服务器地址后点击扫码登录。用手机系统相机扫描二维码，在自有账号网页确认。新设备随后在部署机的 `http://127.0.0.1:8789/` 手动批准。二维码有效期为 5 分钟。Windows 在系统凭据存储保存登录态。设备令牌默认有效 30 天，可通过 `DEVICE_ACCOUNT_SESSION_MS` 调整。到期后重新扫码，已有的白名单授权可以继续使用。

部署端还提供 `POST /api/v1/qrcode/scanned` 和 `POST /api/v1/qrcode/login/action`。当前实现接收 `qrcode_jump_url`，后者另需 `action` 为 `confirm` 或 `cancel`，均需要同一所有者账号的已登录设备凭据。现有 iPad 安装版继续使用原地址准入模式。切换到账号模式前需验证并更新 iPad 原生登录接入。

### 原地址准入模式

在 iPad 的「自建设置 → 自建账号」打开账号页。登录后生成设备关联码，在 App 中粘贴。关联码有效期为 5 分钟且只能成功使用一次。账号关联可以在 iPad 上解除，远控准入仍由设备凭据和 `DESKTOP_CLIENT_NETWORKS` 管理。

新版 iOS 从原有登记记录继承设备 ID，并把它存在本机钥匙串。Windows「全部设备 → 手机 / 平板」会显示已登记的 iOS 设备。改版 App 使用相同签名身份和钥匙串访问组时，更新、重启、切换账号或服务器地址会保留这个 ID。抹掉设备或丢失钥匙串数据后会重新生成。

局域网或路由互通的 VPN 通常可以直接建立媒体连接。需要跨 NAT 或仅中继连接时，在 `SELF_HOST_ICE_SERVERS` 配置自有 STUN/TURN。每项填写一个 `urls` 字符串，TURN 项还需要 `username` 和 `credential`。

服务启动后，在主控设备的浏览器打开服务地址下的 `/healthz`，确认 HTTPS 请求返回 `{"status":"ok"}`。

## 构建 Windows 客户端

在包含自建接口的分支上，按[项目构建说明](../README.md#构建)安装 Windows 工具链，并在项目根目录执行 `cargo dist --native`。Windows x64 可执行文件位于 `target/dist/native/`。

## 连接设备

1. 将自建分支的 Windows 可执行文件复制到主控和被控电脑并运行。只在运行服务器的设备上安装 Node.js。在启动页的「自建服务器」，或控制中心的「连接设置」中填同一 HTTPS 地址，点击「连接自建服务器」。
2. 在被控电脑上开启「允许被控」。在主控的「我的设备」列表中，找到在线的被控电脑并点击「连接」。新电脑未显示时，点击「刷新设备」。
3. 在 iPad 或 iPhone 上，在 Farwing 控制器的「连接自建服务器」填写同一个地址，点击「连接服务器」。网页主控直接用浏览器打开该地址，连接自建服务器后从主机列表发起会话。

新设备先连接服务器提交申请，现有已登记设备继续使用原凭据。管理员在部署端核对申请 ID 后执行以下命令。如果服务配置了 `OPENUUYC_DB_PATH`，CLI 也要指向同一个数据库。

```sh
node server/cli.js pending
node server/cli.js approve DEVICE_ID
```

配置 `ADMIN_PORT=8789` 后，可在部署机浏览器打开 `http://127.0.0.1:8789/` 管理设备。后台使用同一个数据库，在独立的本机回环端口提供服务。可以批准或删除申请、手动输入设备 ID 预准入，或撤销已准入设备。预准入有效期为 7 天，设备登记时消耗该授权。撤销会使现有凭据失效并结束相关会话。使用原登记凭据的已撤销设备重新连接时会提交申请，管理员批准后才能恢复准入。

iPad 在「自建设置」显示待审批的设备 ID，批准后点击「重新检查准入」。Windows 或网页客户端在批准后重新连接。设备令牌按服务器地址保存在客户端。同一部署下获准的设备共用设备目录。使用 `node server/cli.js list` 查看注册设备，使用 `node server/cli.js revoke DEVICE_ID` 撤销已登记设备。

使用远程协助时，在被控端开启「允许被控」和「接受远程协助」，按需选择验证码、本机确认或两者。主控端通过「开始协助」输入九位设备 ID，使用被控端选择的验证方式。收藏和最近连接按发起端设备身份保存，验证码由 Windows 客户端按既有设置保存在本机。

电源操作沿用 Windows 被控端设置。「关机和重启」需要被控端允许。远程开机需要目标的有线网卡和 BIOS 支持 WoL，并且同网段有一台在线 Windows 客户端开启「局域网唤醒协助」。

显示、声音、输入、剪贴板、文件传输和端口转发沿用 Windows 客户端的原生媒体与数据通道，功能按双方硬件能力和被控端权限开放。自建模式使用本地设备身份和本地功能权限。网易扫码、短信登录、会员购买、UU 路由器和官方下发更新由各自的服务提供。

接入原生 Socket.IO 信令的 iOS 主控以 `platform: "ios"` 登记后，可携带设备令牌调用 `POST /v1/sessions`，在请求体中提交 `deviceId` 和 `transport: "socket.io"`。HTTPS 部署返回同源的 `wss://…/socket.io/` 地址。会话票据用于 `X-NRD-AUTH`，会话仍受来源网络、设备授权和主机控制开关约束。省略 `transport` 时使用 `/v1/session` 信令。

官方 4.48.1 的自建内测模块用 `POST /api/v1/tool/configures_with_version` 和 `POST /api/v1/device/ios/init` 获取本地配置并登记为 iOS 主控。登记后的客户端使用本地设备令牌访问 `GET /api/v1/device/list` 和 `POST /api/v1/room/join/by_device/<设备 ID>`。这些接口沿用 `DESKTOP_CLIENT_NETWORKS`。会话接口要求配置 HTTPS 公共地址，签发同源 WSS 票据，并遵守被控端的控制开关和现有会话占用状态。

在官版内测 App 的「自建服务器」页面填写同一 HTTPS 地址。使用私有 CA 时，在该页面导入 PEM 或 DER 格式的证书。HTTPS 和原生 WSS 都按配置的主机名和端口使用该证书，并校验服务端的证书链与主机名。

## 已有配对码部署

旧版前台 Windows host 和现有配对码仍可使用原有接口。服务端签发 host 码后，可用 `list` 查看设备 ID，再签发仅可访问该 host 的 controller 码。

```sh
node server/cli.js pair-code --role host
node server/cli.js list
node server/cli.js pair-code --role controller --host DEVICE_ID
```

服务默认使用 `server/data/devices.sqlite`，可以用 `OPENUUYC_DB_PATH` 指定存放位置。升级时沿用原数据库可保留设备和撤销记录。

各客户端的验收记录见[自建分支进度](PROGRESS.md)。
