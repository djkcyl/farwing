//! OpenUUYC 的虚拟显示器辅助程序。以 root 运行，只做一件事：按主程序要的分辨率
//! 把 VKMS 的一块连接器接上或断开。
//!
//! 为什么要一个 root 的进程：凭空多出一块显示器要写 `/sys/kernel/config/vkms`，
//! 那是 configfs，内核不会把它的权限放给任何非 root 的进程（不像 `/dev/uinput`
//! 能靠 udev 规则授权），所以除了一个只做这一件事的小程序，没有别的选择。
//!
//! 它刻意不依赖任何应用代码，也不做日志框架：特权进程的每一行都该是能一眼看完的。
mod configfs;
mod peer;
mod privilege;

use anyhow::{Context, Result, bail, ensure};
use openuuyc_vkms::protocol::{MAX_DIMENSION, MAX_HZ, MIN_DIMENSION, Reply, Request};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// systemd 的 `openuuyc-vkms.socket` 监听这个路径。
const DEFAULT_SOCKET: &str = "/run/openuuyc-vkms/control.sock";
/// 任何人都连得上，因为「谁可以」不是靠文件权限判的，而是连上以后按会话判的
/// （见 [`peer::authorize`]）。这样包里既不用建组，也不用装 polkit 策略。
const SOCKET_MODE: u32 = 0o666;
/// 空转检查退出标志的节拍。短到关机时察觉不到，长到空闲时不烧 CPU。
const STOP_POLL: Duration = Duration::from_millis(200);
/// 同时接受多少条连接。套接字是 0666 的，一个本地用户随手就能开上几千条，而每条连接
/// 一个线程；超过就回一句「忙」再关掉。正常用法（主程序一条、诊断页每 5 秒探一下）
/// 离这个数很远。
const MAX_CONNECTIONS: usize = 16;
/// 每个 uid 同时最多占几条。只有总数上限的话，一个本地账户开满 16 条就能让坐在机器
/// 前的人一条都连不上；按 uid 分开，至少要凑够 4 个不同的账户才挤得满。
const MAX_PER_UID: usize = 4;
/// 一行请求的长度上限。请求都是几十字节的 JSON，没人会写满它。
const MAX_LINE: usize = 4096;
/// 还没租到屏的连接，两条完整请求之间最多隔多久。是整段的期限，不是单次 `read` 的
/// 超时：后者挡不住每隔几秒滴一个字节的对端。主程序真要用时会重新连（见
/// `src/platform/linux/virtual_display.rs`），空闲连接不值得一直占着一个线程。
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// 被拒绝的连接：回过原因、关掉写端以后，最多再花多久读掉对端发来的字节。
///
/// 不能回完就关：对端这时候多半还没把请求写过来，关掉以后它的 write 拿到 EPIPE，
/// 就读不到我们回的那句原因了。也不能等它把请求写完：那正是慢速发送占位的口子。
const LINGER: Duration = Duration::from_secs(1);
/// 主线程里同时在收尾的「忙」连接上限。再多就直接关掉最早的那条——那是有人在灌连接，
/// 对它们没必要客气。
const MAX_LINGERING: usize = 32;
/// 没有连接也没有租约时，自己退出的时间。套接字单元还在监听，下次连接会把它拉起来。
const IDLE_EXIT: Duration = Duration::from_secs(60);

const EXIT_ERROR: i32 = 1;

fn main() {
    let socket = match socket_path() {
        Ok(socket) => socket,
        Err(error) => fail(error),
    };
    let listener = match listen(&socket) {
        Ok(listener) => listener,
        Err(error) => fail(error),
    };
    // 日志里写实际在听的路径：套接字激活时它由单元的 ListenStream 决定，不是我们的默认值。
    let socket = listening_on(&listener).unwrap_or(socket);
    // 设备建不出来也要继续监听。启动失败就退出的话，`Restart=on-failure` 会在一秒内
    // 重试 5 次，撞上 start limit 之后连套接字单元一起变成 failed
    // （`service-start-limit-hit`），诊断页只会看到「连不上」，看不到真正的原因；
    // 而内核太老、模块没编进去这类问题重试多少次都好不了。所以照常起来，把原因原样
    // 报给每一个请求。
    let device = match configfs::Instance::open() {
        Ok(instance) => {
            note(&format!(
                "已就绪，监听 {socket}，可用 {} 块虚拟显示器",
                instance.capacity().unwrap_or(0)
            ));
            Device::Ready(instance)
        }
        Err(error) => {
            let reason = format!("{error:#}");
            note(&format!(
                "已就绪，监听 {socket}，但用不了虚拟显示器：{reason}"
            ));
            Device::Unavailable {
                unsupported: error
                    .chain()
                    .any(|cause| cause.is::<configfs::KernelTooOld>()),
                reason,
            }
        }
    };

    // 加载模块的权限只在上面那一步用得到，成没成都不再需要了。必须在开任何线程之前：
    // capset 只管调用它的那个线程。
    privilege::drop_all();

    let shared = Arc::new(Shared::new(device));
    watch_signals();
    let slots = Arc::new(Slots::new(MAX_CONNECTIONS, MAX_PER_UID));
    // 回过「忙」、还在等对端写完的连接。它们在主线程里收尾，不占连接线程。
    let mut lingering: Vec<Lingering> = Vec::new();
    let mut turned_away = Throttle::default();
    let mut connections = 0u64;
    let mut idle_since = Instant::now();
    loop {
        if stopping() {
            break;
        }
        // 退出的节拍握在这里，而不是指望 SIGTERM 把 accept 打断：信号是发给整个进程的，
        // 内核可能把它投递给某条连接线程（每条连接一个线程），主线程的 accept 于是照旧
        // 阻塞，收尾逻辑永远轮不到——systemd 只能等满 TimeoutStopSec 再 SIGKILL，
        // 关机就白卡 90 秒。
        let incoming = wait(&listener, &lingering, STOP_POLL);
        lingering.retain_mut(Lingering::drain);
        if !incoming {
            // 「不常驻」得名副其实：拉起一次就一直跑到关机的话，单元里写的就是假话。
            if slots.live() == 0 && lingering.is_empty() && idle_since.elapsed() >= IDLE_EXIT {
                note("空闲超时，退出");
                break;
            }
            continue;
        }
        idle_since = Instant::now();
        match listener.accept() {
            Ok((stream, _)) => {
                // 先认 uid 再给连接位：按 uid 限数要在开线程之前就知道是谁。
                // SO_PEERCRED 是内核记好的，不会阻塞。
                let credentials = match peer::credentials(&stream) {
                    Ok(credentials) => credentials,
                    Err(error) => {
                        note(&format!("新连接无法识别：{error:#}"));
                        continue;
                    }
                };
                let slot = match slots.acquire(credentials.uid) {
                    Ok(slot) => slot,
                    Err(full) => {
                        if let Some(skipped) = turned_away.ready() {
                            note(&format!(
                                "拒绝 uid {} 的新连接：{}{}",
                                credentials.uid,
                                full.log(),
                                match skipped {
                                    0 => String::new(),
                                    n => format!("（此前另有 {n} 条同样被拒）"),
                                }
                            ));
                        }
                        if lingering.len() >= MAX_LINGERING {
                            lingering.remove(0);
                        }
                        lingering.extend(Lingering::start(stream, &Reply::failed(full.reply())));
                        continue;
                    }
                };
                connections += 1;
                let id = connections;
                let shared = Arc::clone(&shared);
                // 每条连接一个线程：主程序会一直连着不放，串行处理会让诊断页的
                // 探测排在它后面永远不返回。连接位跟着线程走，线程结束就还回去。
                let spawned = std::thread::Builder::new()
                    .name(format!("client-{id}"))
                    .spawn(move || {
                        let _slot = slot;
                        serve(&shared, id, stream, credentials);
                    });
                if let Err(error) = spawned {
                    note(&format!("无法处理新连接：{error}"));
                }
            }
            Err(error) => {
                if stopping() {
                    break;
                }
                note(&format!("接受连接失败：{error}"));
                std::thread::sleep(STOP_POLL);
            }
        }
    }
    // 退出前把所有路断开：我们这个进程没了以后，屏幕上不该还留着一块没人管的屏。
    shared.release_all();
    note("已退出");
}

/// 设备这一侧的状态。用不了的时候进程照样活着，只是把原因如实报给请求方。
enum Device {
    Ready(configfs::Instance),
    Unavailable {
        /// 原因会原样转给主控和诊断页，例如「本机内核 6.8 的 VKMS 不支持 configfs」。
        reason: String,
        /// 是不是内核本身不够新。只有这种才报成「不支持」：modinfo 跑不起来、modprobe
        /// 失败、等不到 DRM 卡这些是这一次没成，下次启动（空闲一分钟就会退出重来）
        /// 未必还这样，诊断页说成「内核不支持」会让人去升级一个本来就够新的内核。
        unsupported: bool,
    },
}

impl Device {
    /// 用不了时该回的那句话，能用就是 `None`。
    fn unavailable(&self) -> Option<Reply> {
        match self {
            Self::Ready(_) => None,
            Self::Unavailable {
                reason,
                unsupported: true,
            } => Some(Reply::unsupported(reason)),
            Self::Unavailable { reason, .. } => Some(Reply::failed(reason)),
        }
    }

    fn instance(&self) -> Result<&configfs::Instance> {
        match self {
            Self::Ready(instance) => Ok(instance),
            Self::Unavailable { reason, .. } => bail!("{reason}"),
        }
    }
}

struct Shared {
    device: Device,
    /// 全局占用表：哪一路被哪条连接占着。跨连接共享，所以不同客户端不会互相抢。
    ///
    /// 租约跟着连接走，不跟着「现在是谁在用这台机器」走：A 登录时建了一块屏、然后
    /// 快速切到用户 B，那块屏归 B 的合成器管，租约却还在 A 的 OpenUUYC 手里，直到
    /// 它断开。可以接受——切换用户之后 A 的会话本来也还在屏幕上，而这里是同一台机器、
    /// 同一个 uid 边界内的事。
    taken: Mutex<BTreeMap<usize, (configfs::Lease, u64)>>,
}

impl Shared {
    fn new(device: Device) -> Self {
        Self {
            device,
            taken: Mutex::new(BTreeMap::new()),
        }
    }

    /// 设备本身用不了时该回的话，没有就是 `None`。每个请求都要先看这个。
    fn unavailable(&self) -> Option<Reply> {
        self.device.unavailable()
    }

    /// 这条连接现在租着几块屏。连接算不算「空闲」由它决定。
    fn held(&self, id: u64) -> usize {
        lock(&self.taken)
            .values()
            .filter(|(_, owner)| *owner == id)
            .count()
    }

    fn release_all(&self) {
        if let Device::Ready(instance) = &self.device
            && let Err(error) = instance.release_all()
        {
            note(&format!("退出时断开连接器失败：{error:#}"));
        }
    }

    fn create(&self, id: u64, width: u32, height: u32, hz: u32) -> Result<String> {
        validate(width, height, hz)?;
        let instance = self.device.instance()?;
        let mut taken = lock(&self.taken);
        let slots = taken.keys().copied().collect::<BTreeSet<_>>();
        let lease = instance.attach(&slots)?;
        note(&format!(
            "连接 {id} 拿到 {}（{width}x{height}@{hz}）",
            lease.connector()
        ));
        let connector = lease.connector().to_owned();
        taken.insert(lease.slot(), (lease, id));
        Ok(connector)
    }

    /// 只能还自己拿到的：名字由辅助程序发出，也在辅助程序这里核对属于谁。
    fn destroy(&self, id: u64, connector: &str) -> Result<()> {
        let instance = self.device.instance()?;
        let mut taken = lock(&self.taken);
        let Some((slot, _)) = taken
            .iter()
            .find(|(_, (lease, owner))| lease.connector() == connector && *owner == id)
        else {
            bail!("{connector} 不是这条连接创建的虚拟显示器");
        };
        let slot = *slot;
        instance.detach(&taken[&slot].0)?;
        taken.remove(&slot);
        note(&format!("连接 {id} 归还 {connector}"));
        Ok(())
    }

    fn release(&self, id: u64) {
        let mut taken = lock(&self.taken);
        let mine = taken
            .iter()
            .filter(|(_, (_, owner))| *owner == id)
            .map(|(slot, (lease, _))| (*slot, lease.clone()))
            .collect::<Vec<_>>();
        for (slot, lease) in mine {
            if let Err(error) = self.device.instance().and_then(|i| i.detach(&lease)) {
                note(&format!(
                    "连接 {id} 断开 {} 失败：{error:#}",
                    lease.connector()
                ));
                // 断开失败也要把它从占用表里拿掉：留着的话这一路在重启之前都用不了，
                // 而且记在一条已经断开的连接名下，谁都还不回来。
                taken.remove(&slot);
                continue;
            }
            taken.remove(&slot);
        }
    }

    fn status(&self, id: u64) -> Result<openuuyc_vkms::protocol::Status> {
        let instance = self.device.instance()?;
        let taken = lock(&self.taken);
        Ok(openuuyc_vkms::protocol::Status {
            capacity: instance.capacity()? as u32,
            // 本连接租的，和整机现在占着的（不带属主）：诊断页每次都是一条新连接，
            // 只看前者永远显示不出「已在使用」。
            leased: taken
                .values()
                .filter(|(_, owner)| *owner == id)
                .map(|(lease, _)| lease.connector().to_owned())
                .collect(),
            in_use: taken
                .values()
                .map(|(lease, _)| lease.connector().to_owned())
                .collect(),
        })
    }
}

/// 一条连接：先认人，再按行收请求。
fn serve(shared: &Shared, id: u64, stream: UnixStream, credentials: peer::Credentials) {
    // 不认的对端回一句原因就走，不等它的请求：等的话，一个每隔几秒滴一个字节的对端
    // 就能把这条连接一直挂着。
    let session = match peer::authorize(&credentials) {
        Ok(session) => session,
        Err(error) => {
            refuse(stream, &Reply::failed(format!("{error:#}")), LINGER);
            return;
        }
    };
    let writer = match stream.try_clone() {
        Ok(writer) => writer,
        Err(error) => {
            note(&format!("连接 {id} 无法复制套接字：{error}"));
            return;
        }
    };
    let mut writer = writer;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    let mut logged = false;
    loop {
        // 还没租到屏的连接要在期限内凑出下一条完整的请求，否则断开：套接字是 0666 的，
        // 不设期限的话一条不发数据（或者慢慢滴数据）的连接就能一直占着一个线程和一个
        // 连接位。租到屏的连接不设期限——主程序会把它开一整个会话。
        let deadline = (shared.held(id) == 0).then(|| Instant::now() + IDLE_TIMEOUT);
        line.clear();
        match read_request(&mut reader, &mut line, deadline) {
            Ok(Incoming::Line) => {}
            Ok(Incoming::Closed) => break,
            Ok(Incoming::Idle) => {
                note(&format!(
                    "连接 {id} 空闲超过 {} 秒，断开",
                    IDLE_TIMEOUT.as_secs()
                ));
                break;
            }
            Err(error) => {
                note(&format!("连接 {id} 读取失败：{error:#}"));
                break;
            }
        }
        let reply = match serde_json::from_str::<Request>(line.trim()) {
            Ok(request) => match shared.unavailable() {
                Some(reply) => reply,
                None => {
                    // 只在真的动内核的时候记一行：诊断页每 5 秒探一次的纯粹查询
                    // 不值得写进 journal，写下来就是刷屏。
                    if !logged
                        && matches!(request, Request::Create { .. } | Request::Destroy { .. })
                    {
                        logged = true;
                        note(&format!(
                            "放行 pid={} uid={} gid={} 会话={session}",
                            credentials.pid, credentials.uid, credentials.gid
                        ));
                    }
                    match request {
                        Request::Create { width, height, hz } => {
                            match shared.create(id, width, height, hz) {
                                Ok(connector) => Reply::connector(connector),
                                Err(error) => Reply::failed(error),
                            }
                        }
                        Request::Destroy { connector } => match shared.destroy(id, &connector) {
                            Ok(()) => Reply::done(),
                            Err(error) => Reply::failed(error),
                        },
                        Request::Status => match shared.status(id) {
                            Ok(status) => Reply::status(status),
                            Err(error) => Reply::failed(error),
                        },
                    }
                }
            },
            Err(error) => Reply::failed(format!("请求无法解析：{error}")),
        };
        let mut text = match serde_json::to_string(&reply) {
            Ok(text) => text,
            Err(error) => {
                note(&format!("连接 {id} 应答无法序列化：{error}"));
                break;
            }
        };
        text.push('\n');
        if let Err(error) = writer.write_all(text.as_bytes()) {
            note(&format!("连接 {id} 应答失败：{error}"));
            break;
        }
    }
    // 连接断了就是租约到期。主程序崩溃、被杀、正常退出，走的都是这里。
    shared.release(id);
}

/// 回一句拒绝的原因，然后在 `linger` 之内读掉对端发来的东西再关。
///
/// 先回再读，而且读有总期限：对端拿得到原因，却没法靠慢慢发数据把连接挂着。
/// 关掉写端是告诉对端「这边不会再说什么了」，它读完那一行就会看到 EOF。
fn refuse(stream: UnixStream, reply: &Reply, linger: Duration) {
    let _ = stream.set_write_timeout(Some(linger));
    let _ = (&stream).write_all(&reply_line(reply));
    let _ = stream.shutdown(Shutdown::Write);
    let until = Instant::now() + linger;
    let mut buffer = [0u8; 512];
    loop {
        let left = until.saturating_duration_since(Instant::now());
        if left.is_zero() || stream.set_read_timeout(Some(left)).is_err() {
            return;
        }
        match (&stream).read(&mut buffer) {
            Ok(0) => return,
            Ok(_) => {}
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(_) => return,
        }
    }
}

fn reply_line(reply: &Reply) -> Vec<u8> {
    let mut line = serde_json::to_vec(reply).unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
    line.push(b'\n');
    line
}

/// 连接位：总数一个上限，每个 uid 再一个上限。
struct Slots {
    total: usize,
    per_uid: usize,
    by_uid: Mutex<BTreeMap<u32, usize>>,
}

/// 为什么没给连接位。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Full {
    /// 总数满了。
    Busy,
    /// 这个 uid 自己的份额满了。
    User,
}

impl Full {
    /// 回给对端的那句话，会原样显示给用户。
    fn reply(self) -> String {
        match self {
            Self::Busy => "虚拟显示器辅助程序忙：同时连接数已达上限，请稍后再试".into(),
            Self::User => {
                format!("虚拟显示器辅助程序忙：这个用户已经开着 {MAX_PER_UID} 条连接，请稍后再试")
            }
        }
    }
    fn log(self) -> String {
        match self {
            Self::Busy => format!("同时连接数已达上限（{MAX_CONNECTIONS}）"),
            Self::User => format!("该用户的连接数已达上限（{MAX_PER_UID}）"),
        }
    }
}

impl Slots {
    fn new(total: usize, per_uid: usize) -> Self {
        Self {
            total,
            per_uid,
            by_uid: Mutex::new(BTreeMap::new()),
        }
    }

    fn acquire(self: &Arc<Self>, uid: u32) -> Result<Slot, Full> {
        let mut by_uid = lock(&self.by_uid);
        if by_uid.values().sum::<usize>() >= self.total {
            return Err(Full::Busy);
        }
        let mine = by_uid.entry(uid).or_default();
        if *mine >= self.per_uid {
            return Err(Full::User);
        }
        *mine += 1;
        Ok(Slot {
            slots: Arc::clone(self),
            uid,
        })
    }

    /// 现在占着的连接位总数。
    fn live(&self) -> usize {
        lock(&self.by_uid).values().sum()
    }
}

/// 占着的一个连接位，丢掉就还回去。
struct Slot {
    slots: Arc<Slots>,
    uid: u32,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut by_uid = lock(&self.slots.by_uid);
        if let Some(count) = by_uid.get_mut(&self.uid) {
            *count = count.saturating_sub(1);
            if *count == 0 {
                by_uid.remove(&self.uid);
            }
        }
    }
}

/// 主线程里收尾的一条被拒连接：原因已经写过去、写端已经关了，在期限内读掉对端发来的
/// 字节，期限一到或者对端关了就丢掉。和 [`refuse`] 是同一件事，只是不阻塞——
/// 主线程还要接着 accept。
struct Lingering {
    stream: UnixStream,
    until: Instant,
}

impl Lingering {
    fn start(stream: UnixStream, reply: &Reply) -> Option<Self> {
        stream.set_nonblocking(true).ok()?;
        // 新连接的发送缓冲是空的，这一行一次就写得进去；写不进去就算了，照样关。
        let _ = (&stream).write_all(&reply_line(reply));
        let _ = stream.shutdown(Shutdown::Write);
        Some(Self {
            stream,
            until: Instant::now() + LINGER,
        })
    }

    /// 读掉现在能读的。还要继续等就返回 `true`。
    fn drain(&mut self) -> bool {
        if Instant::now() >= self.until {
            return false;
        }
        let mut buffer = [0u8; 512];
        // 每轮最多读这么多次：对端一直在灌数据的话，主线程也不能被它拖在这里。
        for _ in 0..16 {
            match (&self.stream).read(&mut buffer) {
                Ok(0) => return false,
                Ok(_) => {}
                Err(error) if error.kind() == ErrorKind::Interrupted => {}
                Err(error) => return error.kind() == ErrorKind::WouldBlock,
            }
        }
        true
    }
}

/// 拒绝连接的日志节流：有人在灌连接时，journal 里每 10 秒只记一行。
#[derive(Default)]
struct Throttle {
    last: Option<Instant>,
    skipped: u64,
}

impl Throttle {
    /// 现在该记一行就返回此前被略掉的条数。
    fn ready(&mut self) -> Option<u64> {
        if self
            .last
            .is_some_and(|last| last.elapsed() < Duration::from_secs(10))
        {
            self.skipped += 1;
            return None;
        }
        self.last = Some(Instant::now());
        Some(std::mem::take(&mut self.skipped))
    }
}

/// [`read_request`] 读到了什么。
#[derive(Debug, PartialEq, Eq)]
enum Incoming {
    /// 一整行请求。
    Line,
    /// 对端关掉了。
    Closed,
    /// 期限到了还没凑出一整行。
    Idle,
}

/// 能按剩余时间设读超时的连接。测试里用内存里的数据代替套接字。
trait Timed: Read {
    fn set_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()>;
}

impl Timed for UnixStream {
    fn set_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        self.set_read_timeout(timeout)
    }
}

/// 读一行请求，`deadline` 之前读不完整就算空闲。
///
/// 期限是整行的，每次读之前按剩下的时间重设超时；单次 `read` 的超时挡不住每隔几秒
/// 滴一个字节的对端。长度也要封顶：没有上限的 `read_line` 意味着一个本地用户发一条
/// 不带换行的 1 GiB 数据就能把这个 root 进程的内存吃光。
fn read_request<R: Timed>(
    reader: &mut BufReader<R>,
    line: &mut String,
    deadline: Option<Instant>,
) -> Result<Incoming> {
    let mut bytes: Vec<u8> = Vec::new();
    loop {
        let timeout = match deadline {
            Some(deadline) => {
                let left = deadline.saturating_duration_since(Instant::now());
                if left.is_zero() {
                    return Ok(Incoming::Idle);
                }
                Some(left)
            }
            None => None,
        };
        reader.get_ref().set_timeout(timeout)?;
        let (consumed, complete) = {
            let chunk = match reader.fill_buf() {
                Ok(chunk) => chunk,
                // 超时了就回到循环开头，由期限来判；信号打断的就再读一次。
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if chunk.is_empty() {
                // 对端关掉了。写了一半的行不算一条请求。
                return Ok(Incoming::Closed);
            }
            match chunk.iter().position(|byte| *byte == b'\n') {
                Some(index) => {
                    bytes.extend_from_slice(&chunk[..=index]);
                    (index + 1, true)
                }
                None => {
                    bytes.extend_from_slice(chunk);
                    (chunk.len(), false)
                }
            }
        };
        reader.consume(consumed);
        if complete {
            line.push_str(&String::from_utf8_lossy(&bytes));
            return Ok(Incoming::Line);
        }
        if bytes.len() >= MAX_LINE {
            bail!("请求超过 {MAX_LINE} 字节");
        }
    }
}

fn validate(width: u32, height: u32, hz: u32) -> Result<()> {
    let size = MIN_DIMENSION..=MAX_DIMENSION;
    ensure!(
        size.contains(&width) && size.contains(&height),
        "无效的虚拟显示器尺寸"
    );
    ensure!((1..=MAX_HZ).contains(&hz), "无效的虚拟显示器刷新率");
    Ok(())
}

/// 等监听套接字上来连接，或者收尾中的连接有数据，最多等 `timeout`（收尾的连接
/// 快到期时更早醒）。返回监听套接字是不是可读了。
fn wait(listener: &UnixListener, lingering: &[Lingering], timeout: Duration) -> bool {
    let mut descriptors = Vec::with_capacity(1 + lingering.len());
    descriptors.push(libc::pollfd {
        fd: listener.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    });
    descriptors.extend(lingering.iter().map(|l| libc::pollfd {
        fd: l.stream.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    }));
    let now = Instant::now();
    let timeout = lingering
        .iter()
        .map(|l| l.until.saturating_duration_since(now))
        .fold(timeout, Duration::min);
    // SAFETY: 描述符在这次调用期间一直有效，长度就是数组的长度。
    let ready = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            timeout.as_millis() as libc::c_int,
        )
    };
    ready > 0 && descriptors[0].revents & libc::POLLIN != 0
}

/// systemd 套接字激活时监听套接字是 fd 3，不必自己建、也不该自己删。
fn socket_path() -> Result<String> {
    let mut args = std::env::args().skip(1);
    match args.next() {
        None => Ok(DEFAULT_SOCKET.to_owned()),
        Some(arg) if arg == "--socket" => args.next().context("--socket 后面要跟路径"),
        Some(other) => anyhow::bail!("无法识别的参数：{other}"),
    }
}

fn listen(path: &str) -> Result<UnixListener> {
    if activated() {
        note("由 systemd 套接字激活启动");
        // SAFETY: LISTEN_FDS/LISTEN_PID 表明父进程把监听套接字放在 fd 3 上了。
        return Ok(unsafe { UnixListener::from_raw_fd(3) });
    }
    let path = std::path::Path::new(path);
    if let Some(directory) = path.parent() {
        std::fs::create_dir_all(directory)
            .with_context(|| format!("无法创建 {}", directory.display()))?;
    }
    // 上一次没清干净的套接字文件会让 bind 失败，而它一定是没人监听的。
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => {
            return Err(error).context("无法清理旧的套接字");
        }
        _ => {}
    }
    let listener =
        UnixListener::bind(path).with_context(|| format!("无法监听 {}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(SOCKET_MODE))
        .context("无法设置套接字权限")?;
    Ok(listener)
}

/// 监听套接字绑在哪个路径上。抽象套接字或者查询失败时是 `None`。
fn listening_on(listener: &UnixListener) -> Option<String> {
    let address = listener.local_addr().ok()?;
    Some(address.as_pathname()?.display().to_string())
}

fn activated() -> bool {
    let pid = std::env::var("LISTEN_PID")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    let fds = std::env::var("LISTEN_FDS")
        .ok()
        .and_then(|v| v.parse::<u32>().ok());
    pid == Some(std::process::id()) && fds.unwrap_or(0) >= 1
}

static STOPPING: AtomicBool = AtomicBool::new(false);

fn stopping() -> bool {
    STOPPING.load(Ordering::Acquire)
}

/// systemd 停服务时发 SIGTERM。handler 只置一个标志，真正的收尾（断开所有连接器）
/// 回到主线程做——信号处理函数里不能做那些事。
fn watch_signals() {
    extern "C" fn on_signal(_: libc::c_int) {
        STOPPING.store(true, Ordering::Release);
    }
    for signal in [libc::SIGTERM, libc::SIGINT] {
        // SAFETY: handler 只写一个原子量，是异步信号安全的。
        unsafe {
            libc::signal(signal, on_signal as *const () as libc::sighandler_t);
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 日志就写到标准错误。这个程序由 systemd 拉起，stderr 直接进 journal，
/// 不值得为它拉一个日志框架进来。
pub fn note(message: &str) {
    eprintln!("openuuyc-vkms: {message}");
}

fn fail(error: anyhow::Error) -> ! {
    note(&format!("{error:#}"));
    std::process::exit(EXIT_ERROR)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_dimensions_the_kernel_or_the_protocol_cannot_carry() {
        assert!(validate(1920, 1080, 60).is_ok());
        assert!(validate(1, 1080, 60).is_err());
        assert!(validate(1920, 0, 60).is_err());
        assert!(validate(1920, 1080, 0).is_err());
        assert!(validate(MAX_DIMENSION + 1, 1080, 60).is_err());
    }

    impl Timed for std::io::Cursor<Vec<u8>> {
        fn set_timeout(&self, _: Option<Duration>) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn reads_one_request_line_and_stops_at_end_of_stream() {
        let mut reader = BufReader::new(std::io::Cursor::new(b"{\"op\":\"status\"}\n".to_vec()));
        let mut line = String::new();
        assert_eq!(
            read_request(&mut reader, &mut line, None).unwrap(),
            Incoming::Line
        );
        assert_eq!(line.trim(), "{\"op\":\"status\"}");
        line.clear();
        assert_eq!(
            read_request(&mut reader, &mut line, None).unwrap(),
            Incoming::Closed
        );
    }

    /// 不带换行的长数据不能把这个进程的内存吃光。
    #[test]
    fn a_request_line_is_bounded() {
        let mut reader = BufReader::new(std::io::Cursor::new(vec![b'x'; MAX_LINE * 8]));
        let mut line = String::new();
        assert!(read_request(&mut reader, &mut line, None).is_err());
        assert!(line.is_empty());
    }

    /// 对端每隔一小段滴一个字节：每次 read 都不超时，但整行的期限一到就该断开。
    /// 上一版只设单次 read 超时，这样的对端能把连接一直挂着。
    #[test]
    fn a_slow_drip_does_not_keep_the_connection() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let dripping = std::thread::spawn(move || {
            for _ in 0..30 {
                if (&theirs).write_all(b" ").is_err() {
                    return;
                }
                std::thread::sleep(Duration::from_millis(50));
            }
        });
        let started = Instant::now();
        let mut reader = BufReader::new(ours);
        let mut line = String::new();
        let outcome = read_request(
            &mut reader,
            &mut line,
            Some(started + Duration::from_millis(300)),
        )
        .unwrap();
        assert_eq!(outcome, Incoming::Idle);
        assert!(
            started.elapsed() < Duration::from_millis(800),
            "{:?}",
            started.elapsed()
        );
        drop(reader);
        dripping.join().unwrap();
    }

    /// 被拒绝的对端立刻拿到原因，然后不管它还在不在发，期限一到连接就关。
    #[test]
    fn a_refused_peer_gets_the_reason_and_cannot_linger() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let started = Instant::now();
        let refusing = std::thread::spawn(move || {
            refuse(ours, &Reply::failed("不允许"), Duration::from_millis(300));
        });
        let mut reader = BufReader::new(theirs.try_clone().unwrap());
        let mut answer = String::new();
        reader.read_line(&mut answer).unwrap();
        let reply: Reply = serde_json::from_str(&answer).unwrap();
        assert_eq!(reply.error.as_deref(), Some("不允许"));
        // 对端继续慢慢写，不能把对面拖住。
        for _ in 0..20 {
            if (&theirs).write_all(b" ").is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        refusing.join().unwrap();
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
        // 写端关了：再读就是 EOF。
        answer.clear();
        assert_eq!(reader.read_line(&mut answer).unwrap(), 0);
    }

    /// 一个 uid 占不满全部连接位；连接位还回去以后能再用。
    #[test]
    fn connections_are_capped_per_uid_and_in_total() {
        let slots = Arc::new(Slots::new(5, 2));
        let a: Vec<_> = (0..2).map(|_| slots.acquire(1000).unwrap()).collect();
        assert_eq!(slots.acquire(1000).err(), Some(Full::User));
        let b: Vec<_> = (0..2).map(|_| slots.acquire(65534).unwrap()).collect();
        let c = slots.acquire(1001).unwrap();
        assert_eq!(slots.live(), 5);
        assert_eq!(slots.acquire(1002).err(), Some(Full::Busy));
        drop(a);
        assert_eq!(slots.live(), 3);
        let _again = slots.acquire(1000).unwrap();
        drop((b, c));
        assert_eq!(slots.live(), 1);
    }

    /// 回过「忙」的连接：对端读得到原因，收尾期限一到就被丢掉。
    #[test]
    fn a_busy_connection_is_answered_then_dropped() {
        let (ours, theirs) = UnixStream::pair().unwrap();
        let mut lingering = Lingering::start(ours, &Reply::failed(Full::Busy.reply())).unwrap();
        (&theirs).write_all(b"{\"op\":\"status\"}\n").unwrap();
        assert!(lingering.drain());
        let mut answer = String::new();
        BufReader::new(&theirs).read_line(&mut answer).unwrap();
        assert!(answer.contains("忙"), "{answer}");
        lingering.until = Instant::now();
        assert!(!lingering.drain());
    }

    #[test]
    fn logs_the_path_it_actually_listens_on() {
        let directory =
            std::env::temp_dir().join(format!("openuuyc-vkms-listen-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("control.sock");
        let _ = std::fs::remove_file(&path);
        let listener = UnixListener::bind(&path).unwrap();
        assert_eq!(listening_on(&listener).as_deref(), path.to_str());
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// 设备用不了的时候进程照样活着，只是每次请求都回同一句原因。
    #[test]
    fn an_unusable_device_reports_its_reason() {
        let device = Device::Unavailable {
            reason: "本机内核 6.8 的 VKMS 不支持 configfs".into(),
            unsupported: true,
        };
        assert_eq!(
            device.unavailable(),
            Some(Reply::unsupported("本机内核 6.8 的 VKMS 不支持 configfs"))
        );
        assert!(device.instance().is_err());
    }

    /// 只有内核版本不够才说「不支持」；别的原因照常是失败，诊断页才不会叫人去升级内核。
    #[test]
    fn only_an_old_kernel_is_reported_as_unsupported() {
        let old: anyhow::Error =
            configfs::KernelTooOld("本机内核 6.8 的 VKMS 不支持 configfs".into()).into();
        assert!(
            old.chain()
                .any(|cause| cause.is::<configfs::KernelTooOld>())
        );
        let other = anyhow::anyhow!("加载 vkms 模块失败").context("打开设备失败");
        assert!(
            !other
                .chain()
                .any(|cause| cause.is::<configfs::KernelTooOld>())
        );
        let device = Device::Unavailable {
            reason: format!("{other:#}"),
            unsupported: false,
        };
        let reply = device.unavailable().unwrap();
        assert!(!reply.ok && !reply.unsupported);
    }
}
