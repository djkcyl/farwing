//! 谁能用这个辅助程序。
//!
//! 判定只有一条：对端 uid 有一个**正在使用中的本地会话**，而且那个会话在座位上。
//! 和 polkit 的 `allow_active` 是同一个方向，但我们不假装和它等价：polkit 看的是
//! 「发起请求的那个进程属于哪个会话」，我们只能看「这个用户在座位上有没有活动的
//! 图形会话」。同一个 uid 本来就互相信任（它能把任意代码塞进自己的会话），ssh、
//! cron、`sudo -u` 拉起来的进程都落在同一个 uid 里，所以这一层就够了；把注释写成
//! 「正是 allow_active」只会让人以为边界比实际更强。
//!
//! 为什么不校验对端的可执行文件是哪一个：本机上任何进程都能 exec 我们的程序，认
//! 了 exe 也挡不住「别人用我们的程序来戳套接字」。真正的边界就是「坐在机器前的
//! 那个人」，多这一层只会让人以为边界更强。
use anyhow::{Context, Result, bail, ensure};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::sync::OnceLock;
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

/// 内核报给我们的对端身份，客户端伪造不了。
#[derive(Clone, Copy, Debug)]
pub struct Credentials {
    pub pid: i32,
    pub uid: u32,
    pub gid: u32,
}

pub fn credentials(stream: &UnixStream) -> Result<Credentials> {
    let mut credentials = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: 长度与结构体一致，fd 是本进程持有的已连接套接字。
    let result = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            std::ptr::from_mut(&mut credentials).cast(),
            &mut length,
        )
    };
    ensure!(result == 0, "无法读取对端的身份");
    Ok(Credentials {
        pid: credentials.pid,
        uid: credentials.uid,
        gid: credentials.gid,
    })
}

/// 放行并返回会话号（日志用），或者说明为什么不行。原因要写清楚，它会原样转给主控端。
///
/// 这里**不写日志**：诊断页每 5 秒探一次，每次都会走到这里，写下来就是刷屏。
/// 真正动过内核操作的那条连接由调用方记一行。
pub fn authorize(credentials: &Credentials) -> Result<String> {
    ensure!(
        credentials.pid > 0,
        "对端进程身份无效（pid {}）",
        credentials.pid
    );
    // root 不走这条路：它本来就能直接写 configfs，让它经过这里只会让审计日志里
    // 多一堆分不清来源的记录。
    ensure!(credentials.uid != 0, "辅助程序不接受 root 自己的请求");
    match graphical_session(credentials.uid)? {
        Some(session) => Ok(session),
        None => bail!(
            "用户 {} 没有正在使用中的图形会话，不允许添加虚拟显示器",
            credentials.uid
        ),
    }
}

/// 一条系统总线连接，整个进程共用。
///
/// 每次授权都新建一条的话，一个本地用户开上几千条连接就能把 root 的
/// `max_connections_per_user` 配额用光——那一刻**所有** root 进程都连不上系统总线，
/// 不只是我们这个服务。
fn bus() -> Result<&'static Connection> {
    static BUS: OnceLock<Connection> = OnceLock::new();
    if let Some(bus) = BUS.get() {
        return Ok(bus);
    }
    let opened = Connection::system().context("连接系统总线失败")?;
    // 并发首次调用时可能有两三条连接被建出来，只有一条会被留下，多出来的当场析构。
    Ok(BUS.get_or_init(|| opened))
}

/// 这个用户有没有一个「人在用」的本地会话。有就返回会话号，用于日志。
///
/// 两种都算：类型是 x11/wayland 的活动本地会话，以及该用户当前的 `Display` 会话。
/// 后者是为了从 TTY 登录再起合成器（`niri-session`）的用法——logind 会把那种会话
/// 的 `Type` 记成 `tty`，只认 Type 就会把坐在机器前的本人拒之门外，而 polkit 的
/// `allow_active` 并不看 Type。
fn graphical_session(uid: u32) -> Result<Option<String>> {
    let connection = bus()?;
    let manager = Proxy::new(
        connection,
        "org.freedesktop.login1",
        "/org/freedesktop/login1",
        "org.freedesktop.login1.Manager",
    )
    .context("systemd-logind 不可用")?;
    // ListSessions 给的是 (会话号, uid, 用户名, 座位, 对象路径)。空座位的是远程
    // 登录，本机屏幕上没有它的画面，也就没有虚拟屏可挂。
    let sessions: Vec<(String, u32, String, String, OwnedObjectPath)> =
        manager.call("ListSessions", &()).context("列出会话失败")?;
    let display = display_session(connection, &manager, uid);
    for (id, session_uid, _, seat, path) in sessions {
        if session_uid != uid || seat.is_empty() {
            continue;
        }
        let session = Proxy::new(
            connection,
            "org.freedesktop.login1",
            path.as_str(),
            "org.freedesktop.login1.Session",
        )
        .context("读取会话失败")?;
        let active = session
            .get_property::<bool>("Active")
            .context("读取会话的 Active 属性失败")?;
        if !active {
            continue;
        }
        if display.as_deref() == Some(path.as_str()) {
            return Ok(Some(id));
        }
        // 显式写了 Remote 的会话（ssh、串口……）不算：座位上没有它的画面。
        let remote = session
            .get_property::<bool>("Remote")
            .context("读取会话的 Remote 属性失败")?;
        // 图形会话才有合成器接管新显示器；纯 tty 上建出来没人认领。
        let kind = session
            .get_property::<String>("Type")
            .context("读取会话的 Type 属性失败")?;
        if !remote && matches!(kind.as_str(), "wayland" | "x11") {
            return Ok(Some(id));
        }
    }
    Ok(None)
}

/// 这个 uid 现在显示着的那个会话（`org.freedesktop.login1.User` 的 `Display`）。
///
/// 只是个加分项：老 systemd 或者查询失败时返回 `None`，那条路少一种放行理由，
/// 不影响原来那条。
fn display_session(connection: &Connection, manager: &Proxy<'_>, uid: u32) -> Option<String> {
    let user: OwnedObjectPath = manager.call("GetUser", &(uid,)).ok()?;
    let user = Proxy::new(
        connection,
        "org.freedesktop.login1",
        user.as_str(),
        "org.freedesktop.login1.User",
    )
    .ok()?;
    // `Display` 是 (会话号, 对象路径)；没有显示中的会话时是 ("", "/")。
    let (id, path): (String, OwnedObjectPath) = user.get_property("Display").ok()?;
    (!id.is_empty()).then(|| path.as_str().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机：把「当前用户有没有一个我们会放行的会话」实际问一遍 logind。
    ///
    /// 只读查询，不动任何东西，但它要这台机器上有 systemd-logind，所以不放进默认
    /// 测试集：`cargo test --bin openuuyc-vkms -- --ignored --nocapture`
    #[test]
    #[ignore = "需要本机的 systemd-logind"]
    fn the_current_user_is_matched_against_a_real_logind() {
        // SAFETY: getuid 只是读一个进程属性。
        let uid = unsafe { libc::getuid() };
        let session = graphical_session(uid).expect("查询 logind 失败");
        println!("uid {uid} 匹配到的会话：{session:?}");
    }
}
