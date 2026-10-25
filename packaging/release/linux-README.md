# Farwing Server 0.1.0（Linux）

构建自 main 分支提交 `{commit}`。程序静态链接，不依赖发行版的系统库。

```sh
sudo sh install.sh            # 安装或升级，监听 TCP 443
sudo sh install.sh --port 8443
```

安装后按打印的提示，用浏览器打开 `https://<服务器的域名或 IP>/setup` 完成初始化。完整教程见完整包中的 `GUIDE.md`。

| 文件 | |
| --- | --- |
| `install.sh` | 安装 / 升级脚本（systemd） |
| `farwing-server.service` | systemd 服务 |
| `x86_64/farwing-server`、`aarch64/farwing-server` | 服务端程序 |
| `SHA256SUMS.txt` | 校验值 |

不用脚本时，也可以直接运行 `./x86_64/farwing-server`：监听 TCP 8443，数据保存在程序旁边的 `data` 目录，初始化码打印在终端里。
