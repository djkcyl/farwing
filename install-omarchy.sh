#!/bin/bash
# Install OpenUUYC on Omarchy (Arch + Hyprland), including host (被控) support.
#
# This is the manual path for people who build from source or unpack the
# release archive (it has the same layout as this repository). AUR users do
# not need it: `yay -S openuuyc-bin` already installs every file below.
#
# Usage:
#   ./install-omarchy.sh              # install the binary in this tree
#   ./install-omarchy.sh --uninstall  # remove it (keeps user data)
set -euo pipefail
cd "$(dirname "$0")"

if [[ -n ${OPENUUYC_BINARY:-} ]]; then
    BINARY=$OPENUUYC_BINARY
elif [[ -x target/release/OpenUUYC ]]; then
    BINARY=target/release/OpenUUYC
else
    BINARY=OpenUUYC # the release archive
fi
# 虚拟显示器辅助程序（可选）。发布归档里有，自己构建时要用 crates/vkms 单独编一次。
if [[ -x target/release/openuuyc-vkms ]]; then
    HELPER=target/release/openuuyc-vkms
else
    HELPER=openuuyc-vkms # the release archive
fi
PREFIX="${PREFIX:-/usr/local}"
MENU="$HOME/.config/omarchy/extensions/omarchy-menu.jsonc"

say() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m警告:\033[0m %s\n' "$*" >&2; }

if [[ ${EUID:-$(id -u)} -eq 0 ]]; then
    SUDO=()
else
    SUDO=(sudo)
fi

# Earlier versions of this script installed and enabled a unit of their own
# (openuuyc-gui.service). Login startup is now the settings page's switch, which
# enables openuuyc.service; leaving both would start two copies.
remove_legacy_service() {
    if [[ -e /usr/lib/systemd/user/openuuyc-gui.service ]]; then
        systemctl --user disable --now openuuyc-gui.service 2>/dev/null || true
        "${SUDO[@]}" rm -f /usr/lib/systemd/user/openuuyc-gui.service
        systemctl --user daemon-reload 2>/dev/null || true
    fi
}

uninstall() {
    say "移除 OpenUUYC"
    # 先关掉自启：单元文件删掉之后 systemctl 找不到它，disable 会剩下悬空的
    # graphical-session.target.wants 符号链接。
    systemctl --user disable openuuyc.service 2>/dev/null || true
    "${SUDO[@]}" systemctl disable --now openuuyc-vkms.socket 2>/dev/null || true
    "${SUDO[@]}" rm -f \
        "$PREFIX/bin/OpenUUYC" \
        "$PREFIX/lib/systemd/user/openuuyc.service" \
        "$PREFIX/share/applications/openuuyc.desktop" \
        "$PREFIX/share/icons/hicolor/256x256/apps/openuuyc.png" \
        /usr/bin/openuuyc-vkms \
        /usr/lib/systemd/system/openuuyc-vkms.socket \
        /usr/lib/systemd/system/openuuyc-vkms.service \
        /usr/lib/udev/rules.d/60-openuuyc-uinput.rules
    "${SUDO[@]}" systemctl daemon-reload 2>/dev/null || true
    # 便携运行（没走这个脚本）时程序会自己写一份到用户配置目录，一并清掉。
    # 不接 graphical-session.target 的桌面上，开关写的是 XDG 自启动条目。
    rm -f "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user/openuuyc.service" \
        "${XDG_CONFIG_HOME:-$HOME/.config}/autostart/openuuyc.desktop"
    systemctl --user daemon-reload 2>/dev/null || true
    # Only remove the menu extension if it is exactly the one we wrote; an
    # extension the user merged our entry into stays.
    if [[ -e $MENU ]] && cmp -s packaging/omarchy/omarchy-menu.jsonc "$MENU"; then
        rm -f "$MENU"
    elif [[ -e $MENU ]] && grep -q 'install.service.openuuyc' "$MENU"; then
        warn "$MENU 含有 OpenUUYC 菜单项，请手动删除该条目"
    fi
    remove_legacy_service
    say "已移除。用户数据保留在 ~/.local/share/OpenUUYC/，登录自启（systemd 用户单元与 XDG 自启动条目）已关闭并删除"
    exit 0
}

[[ ${1:-} == --uninstall || ${1:-} == -u ]] && uninstall

if [[ ! -x $BINARY ]]; then
    cat >&2 <<EOF
找不到可执行文件：$BINARY

先构建：
    cargo build --release --bin OpenUUYC

或从 Release 下载 OpenUUYC-<版本>-x86_64-linux.tar.zst，解压后运行其中的
    ./install-omarchy.sh
EOF
    exit 1
fi

# 自启单元里要写 $PREFIX/bin/OpenUUYC。systemd 只认绝对路径，路径里的引号、反斜杠
# 与换行在单元里表达不了（Executable path contains special characters），所以在动
# 系统之前就拒绝。空格与 % 没问题：前者靠引号，后者写成 %%。
case $PREFIX in
/*) ;;
*)
    echo "PREFIX 必须是绝对路径：$PREFIX" >&2
    exit 1
    ;;
esac
case $PREFIX in
*[\"\\]* | *$'\n'*)
    echo "PREFIX 含引号、反斜杠或换行，无法写进 systemd 单元：$PREFIX" >&2
    exit 1
    ;;
esac

say "安装依赖"
"${SUDO[@]}" pacman -S --needed --noconfirm \
    libva libx11 libxcb libxrandr libxi libxcursor libxfixes \
    libxkbcommon libxkbcommon-x11 wayland libglvnd vulkan-icd-loader \
    dbus alsa-lib openssl fuse3 noto-fonts-cjk

say "确认屏幕共享门户"
if ! pacman -Qq xdg-desktop-portal-hyprland &>/dev/null; then
    warn "未安装 xdg-desktop-portal-hyprland，Wayland 下被控采集需要它"
    warn "安装：sudo pacman -S xdg-desktop-portal-hyprland"
fi

say "安装 uinput 输入权限（被控键鼠必需）"
"${SUDO[@]}" install -Dm644 packaging/linux/60-openuuyc-uinput.rules \
    /usr/lib/udev/rules.d/60-openuuyc-uinput.rules
"${SUDO[@]}" udevadm control --reload-rules
"${SUDO[@]}" modprobe uinput
echo uinput | "${SUDO[@]}" tee /etc/modules-load.d/uinput.conf >/dev/null
# uaccess 授权在规则重载后对当前会话不一定立即生效；补一条 ACL 免得重新登录。
if [[ -e /dev/uinput ]]; then
    "${SUDO[@]}" setfacl -m "u:${USER}:rw" /dev/uinput 2>/dev/null || true
fi

say "安装程序文件"
"${SUDO[@]}" install -Dm755 "$BINARY" "$PREFIX/bin/OpenUUYC"
"${SUDO[@]}" install -Dm644 packaging/linux/openuuyc.desktop \
    "$PREFIX/share/applications/openuuyc.desktop"
"${SUDO[@]}" install -Dm644 assets/icon-256.png \
    "$PREFIX/share/icons/hicolor/256x256/apps/openuuyc.png"
# 桌面条目里登记了 uuremote:// 协助链接；刷新 MIME 缓存，xdg-open 才找得到它。
if command -v update-desktop-database >/dev/null; then
    "${SUDO[@]}" update-desktop-database -q "$PREFIX/share/applications" || true
fi

say "安装登录自启单元"
# 打包的那份单元把程序路径写死成 /usr/bin/OpenUUYC；这个脚本装到 $PREFIX，
# 按实际位置改掉，否则单元启动的是另一个（或不存在）的程序。逐行替换而不用 sed，
# 路径里的 # & 之类不会被当成 sed 的语法；% 是 systemd 的说明符，写成 %%。
unit_image="$PREFIX/bin/OpenUUYC"
unit_image=${unit_image//\%/%%}
unit=$(mktemp)
trap 'rm -f "$unit"' EXIT
while IFS= read -r line; do
    case $line in
    ExecStart=*) printf 'ExecStart="%s" gui --background\n' "$unit_image" ;;
    ConditionFileIsExecutable=*) printf 'ConditionFileIsExecutable=%s\n' "$unit_image" ;;
    *) printf '%s\n' "$line" ;;
    esac
done <packaging/linux/openuuyc.service >"$unit"
"${SUDO[@]}" install -Dm644 "$unit" "$PREFIX/lib/systemd/user/openuuyc.service"
systemctl --user daemon-reload 2>/dev/null || true
remove_legacy_service

say "安装虚拟显示器辅助程序"
if [[ -x $HELPER ]]; then
    # 这三样的路径在程序里是写死的（客户端的 HELPER 与 HELPER_SOCKET_UNITS，
    # 见 crates/vkms/src/client.rs），所以和 udev 规则一样装到 /usr，不跟着
    # $PREFIX 走。服务是套接字激活的，默认不启用：装完不启用一切照旧。
    "${SUDO[@]}" install -Dm755 "$HELPER" /usr/bin/openuuyc-vkms
    "${SUDO[@]}" install -Dm644 packaging/linux/openuuyc-vkms.socket \
        /usr/lib/systemd/system/openuuyc-vkms.socket
    "${SUDO[@]}" install -Dm644 packaging/linux/openuuyc-vkms.service \
        /usr/lib/systemd/system/openuuyc-vkms.service
    "${SUDO[@]}" systemctl daemon-reload 2>/dev/null || true
else
    warn "找不到 $HELPER，跳过虚拟显示器辅助程序（被控端给主控加虚拟屏要用它）"
    warn "需要时先在源码目录构建：(cd crates/vkms && cargo build --release --locked)"
fi

# Omarchy only: add an Install → Service row so the app is discoverable there.
if [[ -d $HOME/.config/omarchy ]]; then
    say "添加 Omarchy 菜单项"
    mkdir -p "$HOME/.config/omarchy/extensions"
    if [[ -e $MENU ]] && ! cmp -s packaging/omarchy/omarchy-menu.jsonc "$MENU"; then
        warn "$MENU 已存在，未自动修改"
        warn "请手动加入 packaging/omarchy/omarchy-menu.jsonc 里的条目"
    else
        cp packaging/omarchy/omarchy-menu.jsonc "$MENU"
        say "菜单项已写入 $MENU（Quickshell 会自动重新加载）"
    fi
fi

cat <<EOF

完成。

  启动      应用菜单「OpenUUYC」，或运行 OpenUUYC
  日志      ~/.local/share/OpenUUYC/logs/
  菜单      Super + Space → Install → Service → UU Remote (OpenUUYC)

下一步：扫码登录 → 连接设置 → 打开「允许被控」；需要登录后自动启动时，
打开「连接设置 → 启动 → 开机自启」。

被控端给主控添加虚拟屏需要内核 6.19+ 并带 CONFIG_DRM_VKMS，且当前合成器是 niri；
辅助程序已装好但按需启用，需要时执行：

  sudo systemctl enable --now openuuyc-vkms.socket

首次被控时会在本机屏幕弹出一次屏幕共享授权，点「共享」即可，之后不再询问。
若弹窗被 Hyprland 插件遮挡（例如 overview 常驻表面），可临时提层：

  hyprctl eval 'hl.layer_rule({ match = { namespace = "ch.wysbd.hyprland-preview-share-picker" }, order = 1 })'

卸载：$0 --uninstall
EOF
