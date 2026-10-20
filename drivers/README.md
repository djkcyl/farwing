# Windows 驱动

Farwing 包含输入驱动 `ROOT\FARWING_INPUT`、虚拟显示驱动 `ROOT\FARWING_DISPLAY`，以及可选的内核虚拟音频驱动 `ROOT\FARWING_AUDIO`。三者的硬件 ID、设备名、接口 GUID 与 OpenUUYC 不同（见 [docs/dev/identity.md](../docs/dev/identity.md)），两者可以共存。

**仓库不附带预编译驱动包。** 驱动是可选组件：

- 未嵌入驱动包的构建可以正常构建和运行，后台服务照常安装。输入使用系统输入注入，显示使用现有显示器。驱动安装入口提示“此构建未包含驱动包”。
- 需要虚拟显示、HID 输入或虚拟声卡时，用你自己的代码签名证书构建并签名驱动，放入 `assets/drivers/` 后重新构建客户端。`build.rs` 发现某类驱动的 INF、DLL/SYS、CAT 三个文件齐全时才嵌入；只放了一部分会直接构建失败。

安装驱动时，程序请求管理员授权，把内嵌的公钥证书 `assets/drivers/FarwingDrivers.cer` 加入本机 Root 和 TrustedPublisher，安装确认对话框会显示证书主题和 SHA-1 指纹。这是自签名方案，不是 Microsoft/WHQL 签名；受组织策略限制的设备可能拒绝安装。如果驱动由 Windows 已信任的证书签名，可以不放证书文件，此时安装器不修改证书信任。卸载时会移除驱动与设备，但不会撤销这项证书信任（证书可能与其他驱动共用）；需要撤销时在 `certlm.msc` 的“受信任的根证书颁发机构”和“受信任的发布者”中删除 “Farwing Drivers”。

## 构建与签名

需要 Visual Studio C++ 工具、Windows SDK 与 WDK，以及当前 Windows 用户证书库中的代码签名证书。打包脚本要求证书主题为 `CN=Farwing Drivers`。

SDK 与 WDK 可以是不同版本：`kits.ps1` 分别选用最新的、文件齐全的 SDK（um / shared / ucrt 头文件与库、SignTool）和 WDK（km、VHF、IddCx、ACX、Inf2Cat），WDF 头文件与库不分版本。需要指定时设置 `FARWING_SDK_VERSION` / `FARWING_WDK_VERSION`。已验证的组合：SDK 10.0.26100.0 + WDK 10.0.28000.0。

请用 PowerShell 7（`pwsh`）运行这些脚本。Windows PowerShell 5.1 若继承了 PowerShell 7 的 `PSModulePath`，会加载不兼容的安全模块，`Cert:` 驱动器不可用。

仅用于开发测试时，可以这样创建自签名证书：

```powershell
New-SelfSignedCertificate -Type CodeSigningCert -Subject 'CN=Farwing Drivers' `
    -CertStoreLocation Cert:\CurrentUser\My -KeyExportPolicy NonExportable
```

然后构建、签名并放入 `assets/drivers/`（`-Stage` 同时导出 DER 公钥证书 `FarwingDrivers.cer`）：

```powershell
./drivers/display/build.ps1 -SigningThumbprint <证书指纹> -Stage
./drivers/input/build.ps1 -SigningThumbprint <同一证书指纹> -Stage
./drivers/audio/build.ps1 -TestSigningThumbprint <同一证书指纹> -Analyze -Stage
./drivers/verify.ps1          # 在已信任该证书的机器上
./drivers/verify.ps1 -AllowUntrustedRoot   # 构建机未信任自签名证书时
cargo build --release
```

`-AllowUntrustedRoot` 只接受“签名者是该证书、证书链唯一的问题是根证书未受信任”的签名，并改用编录 API（`CryptCATAdminCalcHashFromFileHandle2` + `CryptCATGetMemberInfo`）检查 INF 与二进制是否在编录中；签名完整性、签名者与成员关系仍然全部检查。

也可以用环境变量 `FARWING_DISPLAY_DRIVER_DIR`、`FARWING_INPUT_DRIVER_DIR`、`FARWING_AUDIO_DRIVER_DIR` 和 `FARWING_DRIVER_CERTIFICATE` 指定其他位置的包和证书。构建时会检查包内 INF 与本目录源码 INF 一致：音频 INF 按 UTF-16LE 打包，另外两个按原样复制。安装器固定的证书指纹由 `build.rs` 根据证书文件计算。

修改驱动内容时，增加对应 INF 的 `DriverVer`。私钥、PFX/P12、密码及本机证书库内容不要提交到仓库。

## 虚拟音频驱动的加载条件

**自签名的内核驱动不能在默认安全策略下加载。** 使用虚拟扬声器或接收远程麦克风时，被控端需要：

- 临时测试：Shift + 重启 → 疑难解答 → 高级选项 → 启动设置 → 重启 → **7 / F7 禁用驱动程序强制签名**，仅本次启动有效；或
- 持续测试：管理员执行 `bcdedit /set testsigning on` 后重启，恢复时执行 `bcdedit /set testsigning off`。Secure Boot、BitLocker 或组织策略可能限制该操作。

程序不会自动修改启动安全策略；信任项目证书不能替代内核驱动加载许可。本次启动不允许测试驱动时，Windows 以代码 52（无法验证数字签名）拒绝启动设备，安装程序会撤回这次新装的设备和驱动包，并提示上面两种做法。开着“内存完整性”（核心隔离）时，按 7 / F7 不一定生效，需要先关闭它；可以用启动后桌面右下角的“测试模式”字样，或注册表 `HKLM\SYSTEM\CurrentControlSet\Control\SystemStartOptions` 中的 `TESTSIGNING` / `DISABLE_INTEGRITY_CHECKS` 确认设置已经生效。输入驱动（VHF/UMDF）和显示驱动（IddCx/UMDF）是用户态驱动，信任证书后即可安装。

## 验证状态

- 2026-10-15：在开发机上用 SDK 10.0.26100.0 + WDK 10.0.28000.0 构建了三个驱动包（音频驱动带 `/analyze`、`/W4 /WX` 无告警），以自签名证书 `CN=Farwing Drivers`（指纹 `6CC0CF35FED805B5652161468A6D313B873F6E91`，有效期至 2031-10）签名并生成编录，放入 `assets/drivers/`；`verify.ps1 -AllowUntrustedRoot` 全部通过（篡改后的 INF 能被识别为非编录成员）；客户端 `build.rs` 已嵌入三个驱动包与证书。签名没有时间戳。
- 2026-10-20：在开发机上实测（“禁用驱动程序强制签名”启动）：三个驱动都已安装，虚拟声卡的虚拟麦克风（单人、两人混音）与虚拟扬声器工作正常，见 [selftest-results.md](../docs/selftest-results.md) §32；未允许测试驱动时安装会以代码 52 失败并自动撤回。
- 2026-10-15 时的记录：**尚未在任何机器上安装和实测。** 安装会把证书加入本机根证书与受信任发布者，并安装设备，属于系统级更改；内核音频驱动还需要测试签名启动。驱动源码只改动了身份标识（硬件 ID、设备名、接口 GUID、EDID 厂商、HID PID、IOCTL 魔数），驱动逻辑未变。

## 来源

显示驱动基于 SudoVDA，原始来源及声明见 [UPSTREAM.md](display/UPSTREAM.md)，Microsoft 示例代码许可见 [LICENSE](display/LICENSE)。

虚拟音频参考 Microsoft ACX 示例，来源见 [UPSTREAM.md](audio/UPSTREAM.md)，许可见 [LICENSE](audio/LICENSE)。
