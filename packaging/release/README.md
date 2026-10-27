# Farwing 0.1.0（{date}）

自己部署服务器的远程桌面。客户端与服务端构建自 main 分支提交 `{commit}`，必须配套使用。

| 路径 | 内容 |
| --- | --- |
| `GUIDE.md` | 部署与使用教程（先看这个） |
| `双机测试清单.md` | 两台电脑实网测试的步骤与预期（第二十版），结果填在表里 |
| `client/Farwing-v0.1.0-windows-x86_64.exe` | Windows 客户端，主控与被控同一个程序，免安装 |
| `client/收集日志.ps1` | 出问题时打包本机日志 |
| `server/farwing-server-0.1.0-linux.tar.gz` | Linux 服务端（x86_64、aarch64）与一键安装脚本 |
| `server/windows/farwing-server.exe` | Windows 服务端（测试或小规模使用） |
| `docs/client-usage.md` | 客户端功能详解 |
| `SHA256SUMS.txt` | 校验值 |

## 快速开始（Linux 服务器 + 域名）

1. 域名的 A 记录指向服务器的公网 IPv4。
2. 上传 `server/farwing-server-0.1.0-linux.tar.gz` 到服务器并安装，记下打印的初始化码：

   ```sh
   tar xzf farwing-server-0.1.0-linux.tar.gz
   cd farwing-server-0.1.0-linux
   sudo sh install.sh
   ```

3. 云服务器安全组放行：TCP 443、3478、5349，UDP 3478–3481、49160–49200。
4. 浏览器打开 `https://你的域名/setup`（先选择继续访问），填初始化码与管理员密码；公开地址 `https://你的域名`；证书选“自动申请”并填域名与邮箱；中转选“内置中转”，公网 IP 填服务器的公网 IPv4。点“完成”。
5. 用管理员登录 `https://你的域名`，在“账号”中创建用户。
6. 每台 Windows 电脑运行客户端，服务器地址填 `你的域名`，登录。被控电脑在“设置 → 后台服务与驱动”中安装服务。
7. 在“设备”页选择电脑连接。

没有域名、443 被占用、用 Windows 当服务器、排错、升级与卸载见 `GUIDE.md`。
