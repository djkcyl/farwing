//! The unattended host service, as far as Linux has one: a per-user systemd
//! unit that follows the graphical session.
//!
//! Windows installs a system service that keeps the account online and owns
//! controlled sessions without a signed-in desktop, with its credentials in a
//! machine vault. Linux has no equivalent and cannot get one cheaply: capture,
//! clipboard and audio all come from the compositor session, so a host with
//! nobody signed in has no picture to send. What Linux does have is
//! [`startup`]: a systemd user unit, `WantedBy=graphical-session.target`, that
//! runs the control center with `--background` the way Sunshine runs (or, on
//! desktops that never reach that target, an XDG autostart entry), so the
//! desktop client still owns its own account and presence, and every answer
//! below is still the portable mode's. A machine-wide service would only be
//! worth it together with a capture path that does not need the session (KMS),
//! or a virtual display, and would replace these answers.
use anyhow::{Context, Result, bail};
use std::path::PathBuf;

const NO_SERVICE: &str = "Linux 尚未提供后台被控服务";

/// Whether an install, update or uninstall is replacing the service now.
pub(crate) fn maintaining() -> bool {
    false
}

pub(crate) mod install {
    use super::*;

    /// No service is ever installed, so the portable client does its own
    /// background upkeep.
    pub(crate) fn running() -> Result<bool> {
        Ok(false)
    }
}

pub(crate) mod vault {
    use super::*;

    /// Credentials stay in the user's Secret Service keyring.
    pub(crate) fn applies() -> Result<bool> {
        Ok(false)
    }
    pub(crate) fn root() -> Result<PathBuf> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn owner() -> Result<Option<String>> {
        Ok(None)
    }
    pub(crate) fn sid(_pid: u32) -> Result<String> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn key(service: &str, account: &str) -> String {
        use sha2::{Digest, Sha256};
        format!(
            "{:x}.secret",
            Sha256::digest(format!("{service}\0{account}"))
        )
    }
    pub(crate) fn read(_key: &str) -> Result<Option<Vec<u8>>> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn write(_key: &str, _value: Option<&[u8]>) -> Result<()> {
        bail!(NO_SERVICE)
    }
}

pub(crate) mod resident {
    use super::*;
    pub(crate) use crate::session::resident::{Reply, Request, Snapshot};

    /// This process is never the service's resident account owner.
    pub(crate) fn is_owner() -> bool {
        false
    }
    /// The desktop client always owns the account itself.
    pub(crate) fn managed() -> bool {
        false
    }
    pub(crate) async fn request(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    pub(crate) fn call(_request: Request) -> Result<Reply> {
        bail!(NO_SERVICE)
    }
    /// Only asked of a managed client; with no service there is nothing to retire.
    pub(crate) async fn pause_for_exit() -> Result<()> {
        Ok(())
    }
}

/// Work the resident owner waits for before it reports itself paused. With no
/// service there is no owner waiting, so nothing is counted.
pub(crate) mod activity {
    pub(crate) struct Work;
    impl Work {
        pub fn new() -> Self {
            Self
        }
    }
}

/// 登录自启，等价于 Windows 的每用户 Run 键。按桌面选两种机制之一：
///
/// - 桌面接进了 `graphical-session.target`（GNOME、KDE、uwsm、niri 等）时，是一个
///   systemd 用户单元。被控要有画面可采，所以它跟着图形会话走：会话结束服务就停，
///   崩了自动回来，日志进 journal。
/// - XFCE、MATE、Cinnamon 与普通 X 会话到不了这个 target，单元 enable 了也没人拉它，
///   这些桌面上写 XDG 自启动条目 `~/.config/autostart/openuuyc.desktop`，与改用单元
///   之前写的是同一份，由桌面的会话管理器启动。
///
/// 开启时只留其中一种：两份同时在会让登录时起两次（第二份被实例锁挡掉，白跑一趟）。
/// 关闭时两种都删。直接启动、既不接 target 也不处理 XDG 自启动的合成器（没有 uwsm
/// 或 dex 的 sway、Hyprland）两种都不会拉起程序，这在程序里分辨不出来，见 README。
pub(crate) mod startup {
    use super::*;
    use std::path::Path;
    use std::sync::OnceLock;

    const UNIT: &str = "openuuyc.service";
    /// systemd 的用户单元搜索路径里由发行版安装的那几处。包装的单元是权威版本，
    /// 便携运行时系统里没有，才按当前可执行文件另写一份到用户配置目录。
    const PACKAGED_UNITS: [&str; 3] = [
        "/usr/lib/systemd/user/openuuyc.service",
        "/usr/local/lib/systemd/user/openuuyc.service",
        "/etc/systemd/user/openuuyc.service",
    ];
    /// 两种机制都要用户配置目录：XDG 条目写在那里，便携运行时的单元也写在那里。
    const NO_CONFIG_DIR: &str = "无法确定用户配置目录（HOME 与 XDG_CONFIG_HOME 都没有设置），写不了 systemd 用户单元，也写不了 XDG 自启动条目";
    /// 本进程就在这个图形会话里跑，答案在运行期间不会变，所以只查一次。
    static REACHABLE: OnceLock<bool> = OnceLock::new();

    fn config_dir() -> Result<PathBuf> {
        std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
            .context("无法确定用户配置目录")
    }

    /// 桌面有没有把自己接进 `graphical-session.target`：接了用 systemd 单元，没接
    /// 用 XDG 条目。
    fn session_reachable() -> bool {
        *REACHABLE.get_or_init(|| {
            std::process::Command::new("systemctl")
                .arg("--user")
                .args(["is-active", "graphical-session.target"])
                .output()
                .is_ok_and(|output| output.status.success())
        })
    }

    fn checked(args: &[&str], output: &std::process::Output) -> Result<()> {
        // 有的子命令（is-enabled）把状态写在标准输出上、错误时标准错误是空的。
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let reason = if stderr.is_empty() {
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        } else {
            stderr
        };
        anyhow::ensure!(
            output.status.success(),
            "systemctl --user {} 失败：{reason}",
            args.join(" ")
        );
        Ok(())
    }

    /// 两种机制都用不了时的原因，界面据此把开关置灰并说明。桌面没接
    /// `graphical-session.target` 不算：那里改用 XDG 条目。
    pub(crate) fn unavailable() -> Option<String> {
        config_dir().err().map(|_| NO_CONFIG_DIR.to_owned())
    }

    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub(crate) enum State {
        Disabled,
        Enabled,
        /// Windows keeps a separate Startup Apps approval the program cannot
        /// override. A desktop's disable switch is the unit or entry itself,
        /// which the settings switch rewrites, so it reads as plain `Disabled`.
        #[allow(dead_code)]
        DisabledByWindows,
    }

    /// Whether login starts OpenUUYC on this desktop: the unit is enabled and
    /// the desktop reaches its target, or the XDG entry is present and not
    /// skipped. The settings switch and `systemctl --user enable/disable` are
    /// the same thing where the unit is the mechanism.
    pub(crate) fn state() -> Result<State> {
        Ok(if enabled()? {
            State::Enabled
        } else {
            State::Disabled
        })
    }

    /// The settings switch. There is no installed copy apart from the running
    /// one, so login starts this program, as enabling remote wake does.
    pub(crate) fn configure(enabled: bool) -> Result<()> {
        if !enabled {
            return set_image(false, Path::new(""));
        }
        set_image(true, &std::env::current_exe()?)
    }

    pub(crate) fn set_image(enabled: bool, path: &Path) -> Result<()> {
        let places = Places::current()?;
        if !enabled {
            return places.disable();
        }
        anyhow::ensure!(path.is_file(), "尚未部署自启动程序");
        let image = path.to_str().context("程序路径不是 UTF-8")?;
        anyhow::ensure!(!image.contains(['\n', '\r']), "程序路径含换行");
        places.enable(image, session_reachable())
    }

    pub(crate) fn enabled() -> Result<bool> {
        Places::current()?.enabled(session_reachable())
    }

    /// 自启读写的位置。测试换成临时目录，碰不到真实的用户配置与系统里的单元。
    struct Places<'a> {
        config: PathBuf,
        packaged: &'a [&'a str],
        /// 测试指向一个不存在的程序，当作没有 systemd 的系统，免得读到、甚至停用
        /// 跑测试的人自己启用的单元。
        systemctl: &'a str,
    }

    impl Places<'static> {
        fn current() -> Result<Self> {
            Ok(Self {
                config: config_dir()?,
                packaged: &PACKAGED_UNITS,
                systemctl: "systemctl",
            })
        }
    }

    impl Places<'_> {
        /// 跑一次 `systemctl --user`。没有 systemd 的发行版上根本没有 systemctl，那时
        /// 返回 `None`，由调用方决定这算不算错。
        fn run_systemctl(&self, args: &[&str]) -> Result<Option<std::process::Output>> {
            match std::process::Command::new(self.systemctl)
                .arg("--user")
                .args(args)
                .output()
            {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
                result => result.context("无法执行 systemctl").map(Some),
            }
        }

        fn systemctl(&self, args: &[&str]) -> Result<()> {
            let output = self.run_systemctl(args)?.context("找不到 systemctl")?;
            checked(args, &output)
        }

        /// 我们自己写的那份单元，与包装的同名单元不同路径。
        fn generated_unit(&self) -> PathBuf {
            self.config.join("systemd/user").join(UNIT)
        }

        /// systemd 现在能不能看到这个单元，包装的或我们自己写的都算。
        fn unit_installed(&self) -> bool {
            self.generated_unit().exists()
                || self.packaged.iter().any(|unit| Path::new(unit).exists())
        }

        fn entry(&self) -> PathBuf {
            self.config.join("autostart/openuuyc.desktop")
        }

        /// 自启在这个桌面上是否开着。`systemd` 是桌面接没接 `graphical-session.target`：
        /// 没接的桌面上 enable 了的单元登录时不会被拉起，不算。XDG 条目在接了的桌面
        /// （GNOME、KDE 的会话管理器或 systemd 的 xdg-autostart 生成器）上照样会启动
        /// 程序，所以哪里都算——升级前写入的条目也因此显示为开。
        fn enabled(&self, systemd: bool) -> Result<bool> {
            Ok(self.entry_runs()? || (systemd && self.unit_enabled()?))
        }

        fn enable(&self, image: &str, systemd: bool) -> Result<()> {
            if systemd {
                self.enable_unit(image)?;
                if let Err(error) = self.remove_entry() {
                    tracing::warn!(error = %format!("{error:#}"), "XDG 自启动条目没能删除");
                }
            } else {
                self.write_entry(image)?;
                // 先前在接了 target 的桌面上开过单元：留着它，回到那个桌面时会起两次。
                if let Err(error) = self.disable_unit() {
                    tracing::warn!(error = %format!("{error:#}"), "自启动单元没能停用");
                }
            }
            Ok(())
        }

        fn disable(&self) -> Result<()> {
            self.disable_unit()?;
            self.remove_entry()
        }

        /// XDG 条目会不会在登录时启动程序：桌面被明确告知跳过它就不算。
        fn entry_runs(&self) -> Result<bool> {
            let entry = self.entry();
            let text = match std::fs::read_to_string(&entry) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
                result => {
                    result.with_context(|| format!("无法读取自启动设置 {}", entry.display()))?
                }
            };
            Ok(!text.lines().map(str::trim).any(|line| {
                line.eq_ignore_ascii_case("Hidden=true")
                    || line.eq_ignore_ascii_case("X-GNOME-Autostart-enabled=false")
            }))
        }

        fn write_entry(&self, image: &str) -> Result<()> {
            let entry = self.entry();
            if let Some(directory) = entry.parent() {
                std::fs::create_dir_all(directory)?;
            }
            std::fs::write(&entry, entry_body(image))
                .with_context(|| format!("无法写入自启动设置 {}", entry.display()))
        }

        fn remove_entry(&self) -> Result<()> {
            match std::fs::remove_file(self.entry()) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
                    Err(error).context("无法删除本程序自启动设置")
                }
                _ => Ok(()),
            }
        }

        /// 单元现在是否已启用，查的是 systemd 的实际状态。
        fn unit_enabled(&self) -> Result<bool> {
            // 单元没装时不必问 systemctl：v253 起 `is-enabled` 才输出 `not-found`，
            // systemd 249（Ubuntu 22.04，deb 只投这个）对不存在的单元标准输出什么都不
            // 打、只往标准错误写一行就非零退出。
            if !self.unit_installed() {
                return Ok(false);
            }
            // 包里的单元文件在、系统却没有 systemd（systemctl 不存在）时，单元不会被
            // 任何人启动，就是没启用；别让开关读成「无法读取」，旧条目也还得能关掉。
            let Some(output) = self.run_systemctl(&["is-enabled", UNIT])? else {
                return Ok(false);
            };
            // 未启用、被 mask 都是非零退出，状态只写在标准输出上。
            let state = String::from_utf8_lossy(&output.stdout).trim().to_owned();
            match state.as_str() {
                "enabled" | "enabled-runtime" | "alias" | "static" | "indirect" | "generated" => {
                    Ok(true)
                }
                "disabled" | "linked" | "linked-runtime" | "masked" | "not-found" => Ok(false),
                // 状态查不到时标准输出是空的，退出码非零同样只是「没启用」。
                "" if !output.status.success() => Ok(false),
                "" => bail!(
                    "systemctl --user is-enabled 没有给出状态：{}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
                other => bail!("无法识别的单元状态：{other}"),
            }
        }

        fn enable_unit(&self, image: &str) -> Result<()> {
            check_image(image)?;
            let generated = self.generated_unit();
            // 包装的单元已经把可执行文件位置写死成安装路径；只有在没有包装单元、或者
            // 之前那份本来就是我们自己写的时候，才按当前可执行文件重写一份，免得便携
            // 运行时换了个位置后单元指向一个已经不存在的文件。
            if generated.exists() || !self.unit_installed() {
                if let Some(directory) = generated.parent() {
                    std::fs::create_dir_all(directory)?;
                }
                std::fs::write(&generated, unit_body(image))
                    .with_context(|| format!("无法写入自启动单元 {}", generated.display()))?;
                self.systemctl(&["daemon-reload"])?;
            }
            // 这里只 enable 不 --now：正在跑的就是本进程，再起一份会被实例锁挡住。
            self.systemctl(&["enable", UNIT])?;
            Ok(())
        }

        fn disable_unit(&self) -> Result<()> {
            // 没启用（包括单元不在）时不必让 systemctl 再报一次错，那就是我们要的结果。
            if self.unit_enabled()? {
                self.systemctl(&["disable", UNIT])?;
            }
            // 只删我们自己写的那份；包装的单元归包管。
            let generated = self.generated_unit();
            if generated.exists() {
                std::fs::remove_file(&generated).context("删除自启动单元失败")?;
                // 没有 systemd 时也就没有要重新加载的用户管理器。
                if let Some(output) = self.run_systemctl(&["daemon-reload"])? {
                    checked(&["daemon-reload"], &output)?;
                }
            }
            Ok(())
        }
    }

    /// XDG 自启动条目，与改用 systemd 单元之前写的一模一样。
    fn entry_body(image: &str) -> String {
        // Desktop Entry quoting: reserved characters get a backslash inside the
        // quoted argument, and the file's string escaping doubles every backslash.
        let mut quoted = String::new();
        for c in image.chars() {
            match c {
                '\\' => quoted.push_str(r"\\\\"),
                '"' | '`' | '$' => {
                    quoted.push_str(r"\\");
                    quoted.push(c);
                }
                '%' => quoted.push_str("%%"),
                _ => quoted.push(c),
            }
        }
        format!(
            "[Desktop Entry]\nType=Application\nName=OpenUUYC\n\
             Exec=\"{quoted}\" gui --background\nX-GNOME-Autostart-enabled=true\n"
        )
    }

    /// 单元内容。包里的 `packaging/linux/openuuyc.service` 是同一份定义，改这里时
    /// 要一起改。
    fn unit_body(image: &str) -> String {
        let specifier = image.replace('%', "%%");
        format!(
            "[Unit]\n\
             Description=OpenUUYC 远程被控\n\
             # 被控要有画面可采，所以跟着图形会话走：会话结束就停，也不在没人登录时硬撑。\n\
             PartOf=graphical-session.target\n\
             After=graphical-session.target xdg-desktop-portal.service\n\
             # 程序不在时（例如已卸载，只剩这份写在 ~/.config 里的单元）跳过这次启动：\n\
             # 条件不满足是 skipped 而不是 failed，Restart= 不会再试。\n\
             ConditionFileIsExecutable={}\n\
             # 与 Sunshine 的用户单元相同：偶尔崩溃照样拉起，一启动就崩的约 25 秒后停下，\n\
             # 不会无休止地重启、刷日志。\n\
             StartLimitIntervalSec=500\n\
             StartLimitBurst=5\n\
             \n\
             [Service]\n\
             Type=simple\n\
             ExecStart={} gui --background\n\
             # 崩了自动回来；从托盘退出是正常退出，不会又被拉起来。\n\
             Restart=on-failure\n\
             RestartSec=5\n\
             # 默认的 control-group 会把子进程一起杀掉：显示恢复守护（`process_group(0)`）\n\
             # 本该跨崩溃活着，`xdg-open` 打开的浏览器也不该跟着退。\n\
             KillMode=process\n\
             \n\
             [Install]\n\
             WantedBy=graphical-session.target\n",
            specifier,
            exec_quote(image)
        )
    }

    /// 单元里表达不了的可执行路径：换行会把一行写成两行，`"` 与 `\` 在 systemd 的
    /// 命令行解析里是特殊字符（`Executable path contains special characters`）。
    fn check_image(image: &str) -> Result<()> {
        anyhow::ensure!(!image.contains(['\n', '\r']), "程序路径含换行");
        anyhow::ensure!(
            !image.contains(['"', '\\']),
            "程序路径含引号或反斜杠，无法写入自启动单元"
        );
        Ok(())
    }

    /// `ExecStart` 里 `%` 是说明符，字面量要写两遍；`$` 在可执行路径上既不展开也不
    /// 折叠（`ExecStart="/x/a$$b"` 找的就是 `a$$b`），写两遍反而找不到程序。`"` 与
    /// `\` 由 [`check_image`] 拒绝。
    fn exec_quote(image: &str) -> String {
        format!("\"{}\"", image.replace('%', "%%"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn quotes_and_escapes_the_executable_path() {
            assert_eq!(exec_quote("/usr/bin/OpenUUYC"), r#""/usr/bin/OpenUUYC""#);
            // 说明符要写成字面量；变量展开在可执行路径上不存在，`$` 不能双写
            assert_eq!(exec_quote("/opt/a%b/c$d"), r#""/opt/a%%b/c$d""#);
            // 引号与反斜杠在单元里表达不了，只能拒绝
            assert!(check_image("/usr/bin/OpenUUYC").is_ok());
            assert!(check_image(r#"/opt/a"b\c"#).is_err());
            assert!(check_image("/opt/a\nb").is_err());
        }

        /// 包里的单元与程序自己写的必须说同一件事：包装时用前者，便携运行时用后者。
        #[test]
        fn the_shipped_unit_agrees_with_the_generated_one() {
            let shipped = include_str!("../../../packaging/linux/openuuyc.service");
            let generated = unit_body("/usr/bin/OpenUUYC");
            for line in [
                "PartOf=graphical-session.target",
                "After=graphical-session.target",
                "ConditionFileIsExecutable=/usr/bin/OpenUUYC",
                "StartLimitIntervalSec=500",
                "StartLimitBurst=5",
                "Type=simple",
                "Restart=on-failure",
                "RestartSec=5",
                "KillMode=process",
                "WantedBy=graphical-session.target",
            ] {
                assert!(shipped.contains(line), "单元文件缺少 {line}");
                assert!(generated.contains(line), "生成的单元缺少 {line}");
            }
            assert!(shipped.contains("ExecStart=/usr/bin/OpenUUYC gui --background"));
        }

        #[test]
        fn unit_follows_the_graphical_session() {
            let body = unit_body("/usr/bin/OpenUUYC");
            assert!(body.contains("ExecStart=\"/usr/bin/OpenUUYC\" gui --background"));
            assert!(body.contains("WantedBy=graphical-session.target"));
            assert!(body.contains("PartOf=graphical-session.target"));
            assert!(body.contains("Restart=on-failure"));
        }

        /// 临时配置目录、没有包装单元：碰不到真实的用户配置，也不会去问 systemctl。
        fn scratch<'a>(name: &str) -> Places<'a> {
            let config = std::env::temp_dir()
                .join(format!("openuuyc-startup-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&config);
            Places {
                config,
                packaged: &[],
                systemctl: "/nonexistent/openuuyc-test/systemctl",
            }
        }

        /// 到不了 `graphical-session.target` 的桌面（XFCE、MATE、Cinnamon）上开启写的
        /// 是 XDG 条目，状态读回为开，关闭时删掉。
        #[test]
        fn falls_back_to_the_xdg_entry_without_the_session_target() {
            let places = scratch("xdg");
            assert!(!places.enabled(false).unwrap());
            places.enable("/opt/Open UUYC/a%b$c", false).unwrap();
            let text = std::fs::read_to_string(places.entry()).unwrap();
            assert!(text.contains(r#"Exec="/opt/Open UUYC/a%%b\\$c" gui --background"#));
            assert!(!places.generated_unit().exists());
            assert!(places.enabled(false).unwrap());
            // 接了 target 的桌面上会话管理器同样会运行这个条目。
            assert!(places.enabled(true).unwrap());
            places.disable().unwrap();
            assert!(!places.entry().exists());
            assert!(!places.enabled(false).unwrap());
            // 已经关着时再关一次不是错误。
            places.disable().unwrap();
            let _ = std::fs::remove_dir_all(&places.config);
        }

        /// 包里的单元文件在而 systemd 不在（或这个用户的管理器里没有它）时，读作关而
        /// 不是「无法读取」，XDG 条目照样能开能关。只读地问一次 `is-enabled`。
        #[test]
        fn a_packaged_unit_without_systemd_reads_as_off() {
            let mut places = scratch("packaged");
            std::fs::create_dir_all(&places.config).unwrap();
            let packaged = places.config.join("openuuyc.service");
            std::fs::write(&packaged, "").unwrap();
            let packaged = [packaged.to_str().unwrap()];
            places.packaged = &packaged;
            assert!(places.unit_installed());
            assert!(!places.enabled(true).unwrap());
            places.enable("/usr/bin/OpenUUYC", false).unwrap();
            assert!(places.enabled(false).unwrap());
            places.disable().unwrap();
            assert!(!places.entry().exists());
            let _ = std::fs::remove_dir_all(&places.config);
        }

        /// 桌面被告知跳过的条目登录时不会运行，读作关；关闭开关时照样删掉。
        #[test]
        fn a_skipped_entry_reads_as_off() {
            let places = scratch("skipped");
            for skip in ["Hidden=true", "X-GNOME-Autostart-enabled=false"] {
                places.enable("/usr/bin/OpenUUYC", false).unwrap();
                let text = std::fs::read_to_string(places.entry()).unwrap();
                std::fs::write(
                    places.entry(),
                    text.replace("X-GNOME-Autostart-enabled=true", skip),
                )
                .unwrap();
                assert!(!places.enabled(false).unwrap(), "{skip}");
                places.disable().unwrap();
                assert!(!places.entry().exists());
            }
            let _ = std::fs::remove_dir_all(&places.config);
        }
    }
}

/// Holds the per-user lock that lets only one process keep a device's
/// presence room; two owners would keep kicking each other off the server.
pub(crate) struct PresenceReservation(#[allow(dead_code)] std::fs::File);

/// Reserve the device's presence room, or `None` while another process of
/// this user holds it.
pub(crate) fn reserve_presence(device: &str) -> Result<Option<PresenceReservation>> {
    anyhow::ensure!(
        !device.is_empty()
            && device
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
        "设备标识无效"
    );
    let base = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map_or_else(crate::platform::paths::require_local_app_data, Ok)?;
    let directory = base.join("openuuyc");
    std::fs::create_dir_all(&directory)?;
    let path = directory.join(format!("presence.{device}.lock"));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("无法打开在线状态锁 {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(Some(PresenceReservation(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
    }
}

/// Refuse a path that is a symbolic link, the Linux counterpart of the
/// Windows reparse-point check on service-owned directories.
pub(crate) fn reject_reparse(path: &std::path::Path) -> Result<()> {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        anyhow::ensure!(!metadata.file_type().is_symlink(), "组件目录不可为符号链接");
    }
    Ok(())
}
