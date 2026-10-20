# 品牌素材

来自 Farwing 品牌设计素材包 1.0（2026-10-19）。主蓝 `#4B88EB`、深墨 `#161A21` 与客户端界面的 `ACCENT`、`BG` 相同。

| 文件 | 用途 |
|---|---|
| `assets/icon.ico`、`assets/icon-256.png` | 程序、安装程序与快捷方式图标；窗口与任务栏图标、标题栏和“关于”页的应用图标 |
| `assets/tray/{light,dark}-{idle,controlled,disabled}.ico` | 托盘图标（资源 2–7）：浅色任务栏用 `light-*`（深色图形），深色任务栏用 `dark-*`；`controlled` 为有人连接本机（红点），`disabled` 为不允许连接本机（灰色） |
| `assets/brand/mark.png`、`assets/brand/wordmark.png` | 侧栏、登录卡片与加载页的标志（图形与英文名称，白色，绘制时着色；按显示像素重新采样） |
| `assets/brand/horizontal-light.svg`、`horizontal-dark.svg` | README 页眉 |

网页端的图标在 `server/web/public`（favicon、PWA 图标与 `site.webmanifest`），页面中的应用图标为 `server/web/src/ui/Shell.tsx` 的 `BrandMark`。
