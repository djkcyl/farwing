#!/bin/sh
# Farwing Server 安装 / 升级脚本（Linux，systemd）。
#
#   sudo sh install.sh [--port 端口] [--no-firewall]
#
# 安装或升级 /usr/local/bin/farwing-server 与 systemd 服务，网页与接口监听
# 端口（默认 443，直接对外提供 HTTPS，证书在网页初始化时选“自动申请”），
# 在运行中的 ufw / firewalld 中放行所需端口，然后启动服务。数据保存在
# /var/lib/farwing-server，升级时保留。程序取自本脚本旁边的
# <架构>/farwing-server（x86_64 或 aarch64）或 farwing-server。
set -eu

PORT=443
PORT_GIVEN=0
FIREWALL=1
while [ $# -gt 0 ]; do
  case "$1" in
    --port)
      [ $# -ge 2 ] || { echo "--port 需要一个端口号" >&2; exit 2; }
      PORT=$2; PORT_GIVEN=1; shift 2 ;;
    --no-firewall) FIREWALL=0; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "未知选项：$1" >&2; exit 2 ;;
  esac
done
case "$PORT" in
  ''|*[!0-9]*) echo "端口无效：$PORT" >&2; exit 2 ;;
esac
if [ "$PORT" -lt 1 ] || [ "$PORT" -gt 65535 ]; then
  echo "端口无效：$PORT" >&2; exit 2
fi

[ "$(id -u)" -eq 0 ] || { echo "请用 root 运行：sudo sh $0" >&2; exit 1; }
command -v systemctl >/dev/null 2>&1 || { echo "需要 systemd" >&2; exit 1; }

here=$(cd "$(dirname "$0")" && pwd)
case "$(uname -m)" in
  x86_64|amd64) arch=x86_64 ;;
  aarch64|arm64) arch=aarch64 ;;
  *) echo "不支持的架构：$(uname -m)（支持 x86_64 与 aarch64）" >&2; exit 1 ;;
esac
binary="$here/$arch/farwing-server"
[ -f "$binary" ] || binary="$here/farwing-server"
[ -f "$binary" ] || { echo "找不到 $here/$arch/farwing-server" >&2; exit 1; }

unit=/etc/systemd/system/farwing-server.service
dropin=/etc/systemd/system/farwing-server.service.d/listen.conf
data=/var/lib/farwing-server
upgrade=0
[ -f "$unit" ] && upgrade=1

# 升级且没有指定端口时沿用原来的端口。
if [ "$upgrade" = 1 ] && [ "$PORT_GIVEN" = 0 ] && [ -f "$dropin" ]; then
  kept=$(sed -n 's/^Environment=FARWING_LISTEN=.*:\([0-9][0-9]*\)$/\1/p' "$dropin")
  [ -n "$kept" ] && PORT=$kept
fi

# 新安装时端口必须空闲。
if [ "$upgrade" = 0 ] && command -v ss >/dev/null 2>&1; then
  if ss -Htln "sport = :$PORT" 2>/dev/null | grep -q .; then
    echo "TCP $PORT 已被其他程序占用。换一个端口：sudo sh $0 --port 8443" >&2
    exit 1
  fi
fi

if ! id farwing >/dev/null 2>&1; then
  nologin=$(command -v nologin || echo /usr/sbin/nologin)
  useradd --system --home-dir "$data" --shell "$nologin" farwing
fi

if [ "$upgrade" = 1 ]; then
  systemctl stop farwing-server 2>/dev/null || true
fi
install -m 0755 "$binary" /usr/local/bin/farwing-server
install -m 0644 "$here/farwing-server.service" "$unit"
mkdir -p "$(dirname "$dropin")"
{
  echo "# 由 install.sh 生成：网页与接口的监听地址。"
  echo "[Service]"
  echo "Environment=FARWING_LISTEN=0.0.0.0:$PORT"
  if [ "$PORT" -lt 1024 ]; then
    echo "AmbientCapabilities=CAP_NET_BIND_SERVICE"
    echo "CapabilityBoundingSet=CAP_NET_BIND_SERVICE"
  fi
} > "$dropin"

# 防火墙：网页 / 接口，中转（UDP、TCP、TLS），NAT 探测与多人转发，中转端口范围。
if [ "$FIREWALL" = 1 ]; then
  if command -v ufw >/dev/null 2>&1 && ufw status 2>/dev/null | grep -q "Status: active"; then
    for rule in "$PORT/tcp" 3478/tcp 5349/tcp 3478:3481/udp 49160:49200/udp; do
      ufw allow "$rule" >/dev/null
    done
    echo "已在 ufw 中放行端口"
  elif command -v firewall-cmd >/dev/null 2>&1 && firewall-cmd --state >/dev/null 2>&1; then
    for rule in "$PORT/tcp" 3478/tcp 5349/tcp 3478-3481/udp 49160-49200/udp; do
      firewall-cmd --permanent --add-port="$rule" >/dev/null
    done
    firewall-cmd --reload >/dev/null
    echo "已在 firewalld 中放行端口"
  else
    echo "没有运行中的 ufw 或 firewalld，未修改本机防火墙"
  fi
fi

systemctl daemon-reload
systemctl enable farwing-server >/dev/null 2>&1
systemctl restart farwing-server
# 等到开始监听（初始化码要在服务写入自己的之后再生成）。
tries=0
while [ $tries -lt 20 ]; do
  sleep 1
  tries=$((tries + 1))
  systemctl is-active --quiet farwing-server || continue
  command -v ss >/dev/null 2>&1 || { sleep 2; break; }
  ss -Htln "sport = :$PORT" 2>/dev/null | grep -q . && break
done
if ! systemctl is-active --quiet farwing-server; then
  echo "服务没有启动。查看日志：journalctl -u farwing-server -n 50 --no-pager" >&2
  exit 1
fi

if [ "$PORT" = 443 ]; then
  address="https://<服务器的域名或 IP>"
else
  address="https://<服务器的域名或 IP>:$PORT"
fi
echo
if [ "$upgrade" = 1 ]; then
  echo "Farwing Server 已升级并重新启动（TCP $PORT），数据与设置保留。"
  echo "网页端：$address"
else
  # 新的初始化码（取代服务启动时打印的那个），以服务的用户运行（数据库权限）。
  if command -v runuser >/dev/null 2>&1; then
    code=$(runuser -u farwing -- /usr/local/bin/farwing-server --data-dir "$data" setup-code | sed -n 's/^Setup code: //p')
  else
    code=$(su -s /bin/sh farwing -c "/usr/local/bin/farwing-server --data-dir $data setup-code" | sed -n 's/^Setup code: //p')
  fi
  echo "Farwing Server 已安装并运行（TCP $PORT）。"
  echo "初始化码：$code（24 小时有效）"
  echo "用浏览器打开 $address/setup 完成初始化。"
fi
echo
echo "云服务器的安全组（或路由器）也要放行："
echo "  TCP $PORT、3478、5349"
echo "  UDP 3478–3481、49160–49200"
