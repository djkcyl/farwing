//! 让合成器把刚接上的那块虚拟输出摆成我们要的样子。
//!
//! 这一步没有通用做法。VKMS 的连接器只有一份固定的无 EDID 模式表（最高 4096x2160，
//! 而且首选是 1024x768），分辨率要多少得由合成器现场定，而每个合成器定这件事的接口
//! 都不一样：命令行、IPC、配置文件，没有一层能覆盖全部的抽象。所以这里是一张后端
//! 表，谁在跑就问谁；一个都问不到就不去惊动内核，直接带着原因失败（见
//! [`ensure_available`]）——先插一块屏再拔掉，用户会看到屏幕布局跳两下。
//!
//! 目前只有 niri 一个后端，因为它就在开发机上、能实测。加别的合成器时按同样的形状
//! 加一条即可——**没有实测过的不要写进来**，那只会让人以为支持。
use anyhow::{Context, Result, bail, ensure};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// `niri msg` 是几毫秒的事，但它要先连上合成器的套接字。
const TIMEOUT: Duration = Duration::from_secs(5);
/// 输出刚接上时合成器可能还没处理完热插拔，给它一点时间出现。
const APPEAR: Duration = Duration::from_secs(5);

/// 当前会话里有没有我们能指挥的输出配置接口。
///
/// 只有 niri 这一个后端，所以「能不能接管新输出」就等于「在不在 niri 里」。
pub(crate) fn available() -> bool {
    std::env::var_os("NIRI_SOCKET").is_some()
}

/// 接管不了就不要去惊动内核：辅助程序接上连接器会让合成器收到一次热插拔，本机用户
/// 的屏幕布局要跳一下，发现不行再拔掉又跳一下（GNOME 的 mutter 还可能把这台新卡当成
/// 第二块 GPU 去初始化）。原因要说清楚，它会被原样转给主控。
pub(crate) fn ensure_available() -> Result<()> {
    ensure!(available(), "{}", unavailable_reason());
    Ok(())
}

/// [`ensure_available`] 里那句话，诊断页也用它。
pub(crate) fn unavailable_reason() -> String {
    format!(
        "当前桌面还不支持创建虚拟显示器（{}），现在只有 niri 能接管新输出",
        session_kind()
    )
}

/// 从环境变量里认出来的桌面名，只用来把上面的原因说得具体一点。
fn session_kind() -> String {
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let kind = match std::env::var("XDG_SESSION_TYPE").as_deref() {
        Ok("wayland") => "Wayland",
        Ok("x11") => "X11",
        _ => "图形会话",
    };
    if desktop.is_empty() {
        kind.to_owned()
    } else {
        format!("{desktop} / {kind}")
    }
}

/// 把一块已经在用的显示器切到另一个分辨率。
///
/// `custom_hz` 给的是我们自己造的虚拟屏的刷新率：那种屏的模式不在 VKMS 的模式表里，
/// 只能靠 `custom-mode` 现造。物理显示器反过来——它的模式表是真的，用 `mode` 从表里
/// 挑，刷新率跟着那条模式走。
pub(crate) fn set_mode(
    connector: &str,
    width: u32,
    height: u32,
    custom_hz: Option<u32>,
) -> Result<()> {
    ensure!(
        !connector.is_empty() && connector.len() <= 64,
        "连接器名无效：{connector}"
    );
    ensure_available()?;
    match custom_hz {
        Some(hz) => niri_msg(&[
            "output",
            connector,
            "custom-mode",
            &format!("{width}x{height}@{hz}"),
        ]),
        None => niri_msg(&["output", connector, "mode", &format!("{width}x{height}")]),
    }
    .with_context(|| format!("无法把 {connector} 切到 {width}x{height}"))
}

/// 让合成器以指定分辨率接管 `connector`。
pub(crate) fn adopt(connector: &str, width: u32, height: u32, hz: u32) -> Result<()> {
    ensure!(
        !connector.is_empty() && connector.len() <= 64,
        "连接器名无效：{connector}"
    );
    // 建屏之前已经确认过合成器能接管（见 [`ensure_available`]），走到这里还没有接口，
    // 只可能是它在接管之前退出了。
    ensure!(
        available(),
        "合成器在接管虚拟屏之前退出了，这块屏只能用 VKMS 自己的默认模式"
    );
    niri(connector, width, height, hz)
}

fn niri(connector: &str, width: u32, height: u32, hz: u32) -> Result<()> {
    let mode = format!("{width}x{height}@{hz}");
    let deadline = Instant::now() + APPEAR;
    loop {
        // 输出还没被合成器认领时它会报错，那只是还没到，不是失败。
        match niri_msg(&["output", connector, "custom-mode", &mode]) {
            Ok(()) => break,
            Err(error) if Instant::now() < deadline => {
                tracing::debug!(%error, connector, "虚拟输出尚未被合成器接管，重试");
                std::thread::sleep(Duration::from_millis(100));
            }
            Err(error) => {
                return Err(error).with_context(|| format!("无法把 {connector} 设成 {mode}"));
            }
        }
    }
    // 缩放固定 1：主控按像素发绝对坐标，逻辑尺寸必须和物理尺寸一样，否则指针会偏。
    // 拿不到就算了，niri 对陌生输出本来就默认 1。
    if let Err(error) = niri_msg(&["output", connector, "scale", "1"]) {
        tracing::warn!(%error, connector, "虚拟输出的缩放没能固定成 1");
    }
    tracing::info!(connector, mode, "虚拟显示器已被合成器接管");
    Ok(())
}

fn niri_msg(args: &[&str]) -> Result<()> {
    let program = niri_binary();
    let mut child = Command::new(&program)
        .arg("msg")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("无法运行 {program}"))?;
    let deadline = Instant::now() + TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) => {
                let mut stderr = String::new();
                if let Some(mut pipe) = child.stderr.take() {
                    use std::io::Read;
                    let _ = pipe.read_to_string(&mut stderr);
                }
                ensure!(
                    status.success(),
                    "{program} msg {} 失败：{}",
                    args.join(" "),
                    stderr.trim()
                );
                return Ok(());
            }
            None if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            None => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{program} msg {} 超时", args.join(" "));
            }
        }
    }
}

/// `niri` 未必在服务进程的 PATH 里，而 `$NIRI_SOCKET` 里就写着正在跑的那个合成器的
/// pid（`niri.wayland-1.<pid>.sock`），顺着它取出可执行文件最保险：拿到的必定是
/// 当前这个合成器自己的命令行工具。
fn niri_binary() -> String {
    let pid = std::env::var("NIRI_SOCKET")
        .ok()
        .and_then(|path| niri_pid(&path));
    if let Some(pid) = pid
        && let Ok(image) = std::fs::read_link(format!("/proc/{pid}/exe"))
        && image.is_file()
    {
        return image.to_string_lossy().into_owned();
    }
    "niri".to_owned()
}

/// `$NIRI_SOCKET` 形如 `/run/user/1000/niri.wayland-1.<pid>.sock`，pid 就在文件名里。
fn niri_pid(socket: &str) -> Option<u32> {
    socket
        .rsplit('/')
        .next()?
        .strip_suffix(".sock")?
        .rsplit('.')
        .next()?
        .parse()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_compositor_pid_out_of_the_socket_path() {
        assert_eq!(
            niri_pid("/run/user/1000/niri.wayland-1.14261.sock"),
            Some(14261)
        );
        // 没有 pid 的文件名不是我们能认的形式。
        assert_eq!(niri_pid("/run/user/1000/niri.sock"), None);
        assert_eq!(niri_pid(""), None);
    }
}
