# Farwing Server 管理接口

Farwing Server 的设置与管理都在网页端完成；同样的操作也可以不用浏览器，用 HTTP 接口（例如 curl）完成。本文是这些接口的说明。设计背景见 [protocol/deployment-design.md](protocol/deployment-design.md) §2–3，网页端见 [web-console.md](web-console.md)，部署见 `server/README.md`。

所有 `/api/...` 请求都要带协议头 `X-Farwing-Protocol: 2`（`GET /api/v2/server` 除外）。下面的例子假设：

```sh
S=https://farwing.example.com:8443      # 公开地址
T=fwa_...                                # API 令牌（见第 2 节）
H=(-H "X-Farwing-Protocol: 2" -H "Authorization: Bearer $T")
# 自签证书：加 -k，或 --cacert data/tls/cert.pem
```

## 1. 命令行

服务端程序只有启动与一条应急命令：

| 用法 | 说明 |
| --- | --- |
| `farwing-server [--data-dir DIR] [--listen ADDR]` | 运行服务。数据目录默认是程序旁边的 `data/`（环境变量 `FARWING_DATA_DIR`）；`--listen`（`FARWING_LISTEN`）指定网页与接口的监听地址，指定后不能在线修改（容器里用） |
| `farwing-server edge [--data-dir DIR] [--join LINK]` | 以边缘中转节点运行（第 5 节） |
| `farwing-server setup-code [--data-dir DIR]` | 打印新的一次性初始化码（24 小时有效，取代之前的码）：服务器未初始化时用于 `/setup`，之后用于恢复管理员 |

日志级别是一项设置；环境变量 `RUST_LOG` 优先于它。

## 2. 认证

管理接口接受两种身份：

- **网页登录**：网页端的管理员登录（签名请求，见 [protocol/farwing-protocol.md](protocol/farwing-protocol.md) §1.2）。开启二次验证的管理员须完成第二步。
- **API 令牌**：`Authorization: Bearer fwa_…`，不需要签名。由管理员在网页端“API 令牌”页创建，只显示一次；服务端只保存哈希。
  - 权限 `read`：只能 `GET`；`admin`：全部管理接口。
  - 有效期（天，可不设）、允许的网络（CIDR，可不设）。
  - 令牌属于创建它的管理员：该账号被禁用、删除或不再是管理员时令牌失效。吊销、过期或失效返回 `401 session_revoked`，网络不允许返回 `403 admin_network_denied`，只读令牌写操作返回 `403 forbidden`。
  - 令牌不能创建或吊销令牌（`403 admin_login_required`），也不能访问账号接口（`/api/v2/me` 等）。
  - 审计日志记下令牌名称；“API 令牌”页显示最近使用的时间与地址。

设置 `web.admin_allow`（管理网络白名单）对两种身份都生效。

```sh
# 网页端登录后创建（只能由登录的管理员创建）
POST /api/v2/admin/tokens {"name": "备份脚本", "scope": "read", "expires_days": 90, "allow": ["10.0.0.0/8"]}
→ {"token": "fwa_…", "id": "…", "name": "备份脚本", "scope": "read", …}
GET    /api/v2/admin/tokens          # 列表（不含令牌本身）
DELETE /api/v2/admin/tokens/{id}     # 吊销
```

## 3. 首次启动与恢复

新服务器（还没有管理员）启动时在控制台打印初始化码，此时只有 `GET /api/v2/server` 与 `/api/v2/setup*` 应答，其他接口返回 `503 setup_required`。

```sh
curl -k -H "X-Farwing-Protocol: 2" $S/api/v2/setup
# {"state": "setup", "current": {…设置视图，见第 4 节…}}

curl -k -H "X-Farwing-Protocol: 2" -H "content-type: application/json" $S/api/v2/setup -d '{
  "code": "ABCD-EFGH-JKLM",
  "admin": {"username": "admin", "password": "…"},
  "settings": {"public_url": "https://farwing.example.com:8443", "turn": {"public_ip": "203.0.113.10"}}
}'
```

- `settings` 与 `PUT /admin/settings` 相同（部分设置，叠加在当前设置上）；可选 `certificate: {cert_pem, key_pem}` 同时上传证书（`tls.mode` 设为 `provided`）。初始化中改动监听地址或反向代理模式时直接生效，不需要确认。
- 可选 `token: {name, scope, expires_days?, allow?}`：同时为新管理员签发一个 API 令牌，在返回的 `token` 中（只显示一次），不用浏览器也能继续管理。恢复管理员（下文）同样可以带 `token`。
- 码错误或过期：`403 setup_code_invalid`（每个地址 15 分钟内最多 10 次）；已初始化：`409 setup_done`。
- 之后 `GET /api/v2/setup` 返回 `{"state": "done"}`；运行 `setup-code` 之后返回 `{"state": "recovery"}`。

恢复管理员（忘记密码、丢失二次验证、管理员被删光）：服务器上运行 `farwing-server setup-code`，然后

```sh
curl -k -H "X-Farwing-Protocol: 2" -H "content-type: application/json" $S/api/v2/setup/recover \
  -d '{"code": "…", "username": "admin", "password": "新密码"}'
```

账号存在时重设密码、启用账号、关闭二次验证并吊销它的全部登录；不存在时新建。账号随后是管理员。初始化码用过即失效。网页端的 `/setup` 页面做同样的事。

## 4. 设置

设置是一份 JSON 文档，保存在数据库中（没有配置文件）。

```sh
curl "${H[@]}" $S/api/v2/admin/settings
```

返回设置视图：

| 字段 | 内容 |
| --- | --- |
| `settings` | 设置文档（密钥显示为空字符串） |
| `turn_secret_set` | 中转共享密钥已设置 |
| `listen_locked` | 监听地址由 `--listen` 指定 |
| `data_dir` | 数据目录 |
| `tls` | `fingerprint`（当前证书 SHA-256 指纹，反向代理模式为空）、`acme`（自动证书状态）、`provided`（已上传证书） |
| `relay` | 内置中转实际绑定的地址：`turn_udp`、`turn_tcp`、`turn_tls`、`stun`；`nat_discovery`（发给客户端的 NAT 探测地址）；`local_ipv4`（建议的本机地址） |
| `setup` | 尚未初始化 |
| `listen_pending` | 待确认的监听地址变更：`{url, confirm_by}`，否则为空 |

### 4.1 修改

`PUT /api/v2/admin/settings` 的请求体是部分设置：对象逐键合并，其他值（包括数组）整体替换；密钥留空表示不变。整份文档校验通过后才生效并保存：

```sh
curl "${H[@]}" -X PUT -H "content-type: application/json" $S/api/v2/admin/settings \
  -d '{"instance_name": "家里", "limits": {"login_failures_per_ip": 10}}'
```

返回设置视图并多一个 `outcome`：

| 字段 | 内容 |
| --- | --- |
| `listen_url`、`confirm_by` | 监听地址或反向代理模式变了：新地址与确认期限（Unix 秒），见 4.2 |
| `fingerprint_changed` | 证书换了（使用自签或上传证书的客户端需要重新确认指纹） |
| `relay_restarted` | 内置中转重启了（经中转的连接会自动重连） |

被拒绝时返回 `400 bad_request`，`details`：`{"reason": "setting_invalid", "field": "turn.public_ip", "problem": "required"}`。`problem`：`required`、`invalid`、`out_of_range`、`conflict`、`unavailable`（端口被占用、文件不可用）、`locked`。被拒绝时设置不变。

各项设置何时生效：

| 设置 | 生效方式 |
| --- | --- |
| `instance_name`、`registration`、`guest`、`bitrate_limit_mbps`、`web`、`trusted_proxies` | 立即 |
| `limits` | 立即（已建立的实时连接保持原来的限额，新连接用新值） |
| `log` | 立即 |
| `tls`（自签名称、上传证书、自动申请） | 新连接使用新证书，已有连接不受影响 |
| `turn`（中转与 NAT 探测端点） | 内置中转按新设置重启；新会话拿到新的服务器列表与凭据 |
| `listen`、`tls.mode` 切换到 / 离开 `proxy` | 新监听立即启动，须在 60 秒内确认（4.2） |

### 4.2 监听地址

监听地址（或反向代理模式）只能单独修改（同一请求里还可以改 `public_url`）。生效后新旧地址同时应答，须在 `confirm_by` 之前确认，否则自动恢复原来的地址：

```sh
curl "${H[@]}" -X PUT -H "content-type: application/json" $S/api/v2/admin/settings -d '{"listen": "0.0.0.0:9443"}'
curl "${H[@]}" -X POST https://farwing.example.com:9443/api/v2/admin/settings/listen/confirm   # 保留
curl "${H[@]}" -X POST $S/api/v2/admin/settings/listen/revert                                 # 立即恢复
```

没有待确认的变更时两者都返回 `409 nothing_to_confirm`。同一地址上切换反向代理模式时，原来的监听立即关闭，只能从新地址（`http://` 或 `https://`）确认。

### 4.3 证书、导出导入、中转密钥

```sh
# 上传证书链与私钥（PEM，二者须匹配），并改用它
jq -n --rawfile c fullchain.pem --rawfile k privkey.pem '{cert_pem: $c, key_pem: $k}' |
  curl "${H[@]}" -X PUT -H "content-type: application/json" $S/api/v2/admin/settings/certificate -d @-

# 导出（默认不含密钥；?secrets=true 含密钥）
curl "${H[@]}" "$S/api/v2/admin/settings/export?secrets=true" > farwing-settings.json

# 导入整份设置（密钥为空时保留当前的；--listen 指定的监听地址不变）
curl "${H[@]}" -X POST -H "content-type: application/json" $S/api/v2/admin/settings/import -d @farwing-settings.json

# 生成新的中转共享密钥（内置中转重启）
curl "${H[@]}" -X POST $S/api/v2/admin/settings/relay-secret
```

上传的证书保存在 `<数据目录>/tls/provided-cert.pem`、`provided-key.pem`。

### 4.4 设置文档

| 键 | 默认 | 说明 |
| --- | --- | --- |
| `listen` | `0.0.0.0:8443` | 网页与接口的监听地址 |
| `public_url` | `https://<本机地址>:8443` | 客户端与浏览器访问的地址；实时端点由它生成。带路径前缀时，反向代理可剥掉前缀 |
| `instance_name` | `farwing` | 实例名称 |
| `trusted_proxies` | `[]` | 受信任的反向代理（CIDR），只对它们采用 `X-Forwarded-For` |
| `bitrate_limit_mbps` | 空 | 下发给客户端的会话码率上限（1–10000） |
| `log` | `info` | 日志过滤（`tracing` 语法） |
| `registration` | `false` | 邀请码注册 |
| `guest.enabled` | `true` | 游客与网页访客 |
| `tls.mode` | `self_signed` | `self_signed`、`provided`（上传的证书）、`acme`（自动申请）、`proxy`（明文 HTTP，由反向代理提供 TLS） |
| `tls.hostnames` | 本机名称与地址 | 自签证书包含的名称（另加 `public_url` 的主机名）；名称变化时重新生成证书 |
| `tls.acme` | | `domains`（空 = `public_url` 的主机名）、`contact`、`directory`（`letsencrypt`、`letsencrypt-staging` 或目录 URL）、`challenge`（`auto`、`tls-alpn-01`、`http-01`）、`http_listen`（HTTP-01 与跳转）、`ca_root`（私有 CA 根证书路径）、`agree_tos`（必须为 `true`） |
| `turn.mode` | `builtin` | `builtin`（内置 STUN/TURN）、`external`（coturn 等，`use-auth-secret`） |
| `turn.relay` | `true` | 内置模式提供中转；关闭时只运行 NAT 探测端点 |
| `turn.urls` | `[]` | 发给客户端的 `stun:`/`turn:` 地址；内置模式留空时自动生成 |
| `turn.shared_secret` | 随机 | TURN REST 凭据的共享密钥（读取时为空） |
| `turn.credential_ttl` | `43200` | 中转凭据有效期（秒） |
| `turn.force_relay` | `false` | 要求客户端只用中转 |
| `turn.listen_udp`、`turn.listen_tcp` | `0.0.0.0:3478` | 内置 TURN 的监听地址（可为空） |
| `turn.listen_tls` | `0.0.0.0:5349` | 内置 TLS 中转的监听地址（可为空；使用网页端的证书，反向代理模式下不启动） |
| `turn.public_ip` | 空 | 内置中转写入候选的公网 IP（内置模式必填） |
| `turn.relay_bind`、`turn.relay_ip` | `0.0.0.0`、空 | 中转套接字绑定地址、对外地址（默认等于 `public_ip`） |
| `turn.relay_port_min`、`turn.relay_port_max` | `49160`、`49200` | 中转端口范围 |
| `turn.realm` | `farwing` | TURN realm |
| `turn.stun` | 3479、3480 | NAT 探测端点 `[{"listen": "0.0.0.0:3479", "public": null}]`，说明见 `server/README.md`“NAT 类型探测” |
| `sfu.enabled` | `true` | 多人转发：观看人数达到阈值时被控只发一份，由转发器分发 |
| `sfu.threshold` | 3 | 开始转发的观看人数（2–16） |
| `sfu.allow_compatible` | `true` | 允许被控选择兼容模式（转发器能看到画面） |
| `sfu.port` | 3481 | 转发器的 UDP 端口（所有转发连接共用；绑定 `turn.relay_bind`，候选地址为 `turn.public_ip`）；不能与中转端口相同 |
| `web.enabled` | `true` | 网页端（关闭后页面返回 404，接口不受影响） |
| `web.admin_allow` | `[]` | 允许管理的网络（CIDR），空 = 不限 |
| `web.session_idle_minutes`、`web.session_max_hours` | 30、12 | 网页登录的空闲与最长期限 |
| `web.audit_days` | 180 | 审计日志保留天数 |
| `web.caps` | 浏览器能用的全部 | 网页主控的能力上限（须包含 `view`） |
| `limits.*` | | 心跳、超时、帧与 SDP 大小、信令速率、登录与设备登记限流、协助等待、游客凭据期限（键名见网页端“限额与超时”或导出的文档） |

未知的键被拒绝（`problem: invalid`，`field: settings`）。

## 5. 中转节点

```sh
# 添加：返回加入命令（1 小时内有效）
curl "${H[@]}" -X POST -H "content-type: application/json" $S/api/v2/admin/nodes -d '{"name": "上海", "region": "cn-east"}'
# {"node": {...}, "command": "farwing-server edge --join \"https://…/edge/fwj_…#AB:CD:…\"", "link": "…", "expires_at": …}

curl "${H[@]}" $S/api/v2/admin/nodes                       # 列表：连接、健康与统计
curl "${H[@]}" -X PATCH -H "content-type: application/json" $S/api/v2/admin/nodes/<id> \
  -d '{"enabled": false, "settings": {"public_ip": "203.0.113.20", "bandwidth_limit_mbps": 500}}'
curl "${H[@]}" -X POST $S/api/v2/admin/nodes/<id>/rejoin    # 新的加入命令（节点丢失了身份）
curl "${H[@]}" -X DELETE $S/api/v2/admin/nodes/<id>         # 移除，节点的身份失效
```

节点设置（`settings`，部分修改，对象逐键合并）：`public_ip`（空 = 节点连接本服务器的地址）、`listen_udp`、`listen_tcp`、`listen_tls`、`relay_port_min`、`relay_port_max`、`relay_bind`、`stun`（同 4.4 的 `turn.*`）、`bandwidth_limit_mbps`（超过后不再分配给新会话）、`sfu_port`（节点转发器的 UDP 端口，空 = 与本服务器的 `sfu.port` 相同）。列表中每个节点另有 `online`、`usable`（在线、启用、30 秒内报告过健康且中转在运行、未超带宽）、`address`、`health`（实际监听地址、`allocations`、有分配的会话 `sessions`、`relayed_bps`、`error`、`version`）。

节点的连接（`GET /api/v2/edge/ws`）：加入时带 `X-Farwing-Join: <令牌>`、`X-Farwing-Node-Key: <新公钥>`，之后带 `X-Farwing-Node: <节点 ID>`；都带 `X-Farwing-Timestamp`（Unix 毫秒）与 `X-Farwing-Signature`（Ed25519 对 `FWEDGE1\n<公钥或节点 ID>\n<时间戳>` 的签名）。本服务器下发 `{"type": "joined"}`、`{"type": "config", "turn": {...}}`（节点的中转设置，`shared_secret` 为节点密钥）、`{"type": "control_fingerprint"}`（证书更换），节点每 10 秒发 `{"type": "health", ...}`。

## 6. 多人转发

```sh
curl "${H[@]}" $S/api/v2/admin/sfu    # 转发房间：被控、所在节点、模式、可订阅的会话与统计
```

每个房间：`id`、`host_device_id`、`node`（边缘节点名称，空 = 本服务器）、`mode`（`e2ee` / `compatible`）、`sessions`、`created_at`、`stats`（`packets`、`bytes`、`uplink_repairs`（向被控补要的包）、`keyframe_requests`、`legs`：每位观看者的 `packets`、`bytes`、`repairs`、`dropped`、`loss`（%）、`rtt_ms`）。边缘节点上的房间统计随节点每 10 秒的健康报告更新。概览的 `forwarding_rooms`、`forwarding_viewers` 与会话列表的 `forwarded_by`（转发所在节点，空字符串 = 本服务器，`null` = 未转发）来自同一数据。房间由客户端经 `/api/v2/sfu/*` 打开（`farwing-protocol.md` §3.14）；关闭多人转发时所有房间立即关闭。

## 7. 其他管理接口

| 接口 | 说明 |
| --- | --- |
| `GET /api/v2/admin/overview` | 服务器概览 |
| `GET`/`POST /api/v2/admin/users`，`GET`/`PATCH`/`DELETE …/users/{id}`，`POST …/{id}/password`，`POST …/{id}/totp/reset` | 账号：创建、修改（`{"admin": bool, "disabled": bool, "nickname": …}`）、删除、重置密码、重置二次验证 |
| `GET`/`POST /api/v2/admin/invites`，`DELETE …/{id}`，`GET …/{id}/uses` | 邀请码 |
| `GET /api/v2/admin/devices`，`DELETE …/{id}`，`POST …/{id}/disconnect` | 设备：解绑、断开 |
| `GET /api/v2/admin/sessions`，`DELETE …/{id}` | 会话：结束 |
| `GET`/`POST /api/v2/admin/updates`，`PUT`/`DELETE …/{id}` | 客户端更新公告 |
| `GET /api/v2/admin/audit?kind=&actor=&before=&limit=` | 审计日志 |

请求与返回的字段与网页端使用的相同（类型见 `server/web/src/lib/types.ts`）。
