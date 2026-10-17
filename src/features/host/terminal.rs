//! Host-side terminal (`CT_TERMINAL`, connection `kind = 8`).
//!
//! UU's Android client opens a shell on the controlled machine. The macOS
//! server splits the work in two: `TerminalHandler` parses requests off the
//! wire and `TerminalSession` runs the pty (see
//! [`crate::platform::terminal`]).
//!
//! ## Two protocols on one channel
//!
//! The client mixes two message shapes on `BINARY_DATA_CHANNEL`:
//!
//! * `terminal_check_env_req` and the rename RPC are ordinary protobuf
//!   `PbControlMessage` envelopes, handled by the shared session handler.
//! * Everything else is a private `TERM` frame.
//!
//! The client itself makes the same split: its coordinator logs
//! `TERM 帧解析失败` and `非 TERM 数据解析 GvPb_Message 失败` on the two paths.
//!
//! ## `TERM` frame layout
//!
//! Reconstructed from the shipped encoder (`UURemoteServer.arm64`
//! `0x1001a5888`) and confirmed against a capture:
//!
//! ```text
//! offset  size  field
//!      0     4  "TERM"
//!      4     1  protocol version (1)
//!      5     1  frame type, declaration index + 1
//!      6     2  zero
//!      8     4  session handle, little endian (0 for list requests)
//!     12     4  payload length, little endian
//!     16     n  payload
//! ```
//!
//! The captured `terminal_check_env_req` frame from the Android client was
//! `54 45 52 4d 01 09 ...`: version 1, type 9 = `listSessionsReq`, empty
//! handle and payload, which matches the session list the phone then shows.
//!
//! The handle is chosen by the controller and echoed by the host, so it is the
//! only thing that identifies a session on the wire.

use bytes::Bytes;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use webrtc::data_channel::RTCDataChannel;

#[cfg(target_os = "linux")]
pub(crate) mod local;
/// 本机 attach 目前只有 Unix socket 一套实现；Windows 用命名管道还没做，
/// 侧边栏与子命令先明确说不支持。
#[cfg(not(target_os = "linux"))]
pub(crate) mod local {
    /// 这个平台能不能把会话接到本机终端里。
    pub(crate) const SUPPORTED: bool = false;
    pub(crate) const UNSUPPORTED: &str = "本机终端接入暂只支持 Linux";
    pub(crate) fn serve(_table: std::sync::Arc<std::sync::Mutex<super::Sessions>>) {}
    pub(crate) fn run_client(
        _session: Option<&str>,
        _list: bool,
        _new: bool,
    ) -> anyhow::Result<()> {
        anyhow::bail!(UNSUPPORTED)
    }
    pub(crate) fn open_in_terminal(_session: u32) -> anyhow::Result<()> {
        anyhow::bail!(UNSUPPORTED)
    }
}

/// The four bytes every terminal frame starts with.
const MAGIC: &[u8; 4] = b"TERM";
/// Fixed header size, magic included.
const HEADER: usize = 16;
/// Only version 1 has ever been seen; refuse anything else rather than guess.
const VERSION: u8 = 1;
/// Shells one login may hold.
const MAX_SESSIONS: usize = 16;
/// The client rejects a name longer than this and shows its own length hint;
/// matching the limit keeps the two in agreement instead of failing at the
/// client after the server already accepted it.
const MAX_NAME: usize = 64;
/// Shells all logins together may hold, on top of [`MAX_SESSIONS`] each.
const MAX_TOTAL: usize = 64;
/// 本机自己创建的会话在表里的所有者。手机端按账号列出会话，看不到这些。
const LOCAL_OWNER: &str = "local";
/// 本机会话的编号从这里往上找，避开主控常用的小编号，免得两边撞号。
const LOCAL_HANDLE_BASE: u32 = 1000;
/// Input a session may hold when the pty will not take it yet. A frame that
/// would go past it is refused whole (see [`Sessions::write`]).
const PENDING_LIMIT: usize = 1024 * 1024;
/// Most pty output one connection reads in a single pump tick.
const PUMP_BUDGET: usize = 256 * 1024;
/// Output the data channel may hold before the pump stops reading the ptys,
/// the same mark the file transfer keeps to.
const OUTPUT_HIGH_WATER: usize = 2 * 1024 * 1024;
/// A shell nobody has attached to for this long is ended.
const IDLE_LIMIT: std::time::Duration = std::time::Duration::from_secs(30 * 60);
/// How often detached shells are checked against [`IDLE_LIMIT`].
const SWEEP_INTERVAL: std::time::Duration = std::time::Duration::from_secs(60);
/// The largest terminal a controller may ask for. The screen model allocates
/// every cell up front (32 bytes each, twice while a full-screen program uses
/// the alternate screen), so an unchecked 65535 x 65535 would ask for 128 GiB
/// and take the whole host down. 512 columns is wider than a 4K screen at a
/// 7-pixel font; 512 x 256 is 4 MiB a screen. The bound also keeps the size
/// inside the signed 16-bit `COORD` ConPTY takes.
const MAX_COLS: u16 = 512;
const MAX_ROWS: u16 = 256;

/// The size the pty and the model actually get for a requested one.
fn clamp_size(cols: u16, rows: u16) -> (u16, u16) {
    (cols.clamp(1, MAX_COLS), rows.clamp(1, MAX_ROWS))
}

/// Frame kinds, in the declaration order the shipped enum uses.
///
/// The wire value is `index + 1`, so `open` is 1 and `detach` is 16. The
/// encoder adds one; both binaries agree on the resulting numbers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FrameType {
    Open = 1,
    Stdin = 2,
    Resize = 3,
    Close = 4,
    Stdout = 5,
    Exit = 6,
    Signal = 7,
    Error = 8,
    ListSessionsReq = 9,
    ListSessionsResp = 10,
    KillSessionReq = 11,
    KillSessionResp = 12,
    OpenResp = 13,
    UnlockReq = 14,
    UnlockResp = 15,
    Detach = 16,
}

impl FrameType {
    fn from_wire(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::Open,
            2 => Self::Stdin,
            3 => Self::Resize,
            4 => Self::Close,
            5 => Self::Stdout,
            6 => Self::Exit,
            7 => Self::Signal,
            8 => Self::Error,
            9 => Self::ListSessionsReq,
            10 => Self::ListSessionsResp,
            11 => Self::KillSessionReq,
            12 => Self::KillSessionResp,
            13 => Self::OpenResp,
            14 => Self::UnlockReq,
            15 => Self::UnlockResp,
            16 => Self::Detach,
            _ => return None,
        })
    }

    fn wire(self) -> u8 {
        self as u8
    }
}

/// One decoded `TERM` frame.
pub(crate) struct Frame {
    pub(crate) kind: FrameType,
    pub(crate) session: u32,
    pub(crate) payload: Vec<u8>,
}

impl Frame {
    /// Decodes a frame, or returns `None` for protobuf or malformed data.
    ///
    /// A frame is never abandoned halfway: the declared payload length has to
    /// fit, otherwise a truncated read would be mistaken for a short payload.
    pub(crate) fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < HEADER || &bytes[..4] != MAGIC || bytes[4] != VERSION {
            return None;
        }
        let kind = FrameType::from_wire(bytes[5])?;
        let session = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let length = u32::from_le_bytes(bytes[12..16].try_into().ok()?) as usize;
        let payload = bytes.get(HEADER..HEADER.checked_add(length)?)?.to_vec();
        Some(Self {
            kind,
            session,
            payload,
        })
    }

    /// Builds a frame the controller will accept.
    fn encode(kind: FrameType, session: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER + payload.len());
        out.extend_from_slice(MAGIC);
        out.push(VERSION);
        out.push(kind.wire());
        out.extend_from_slice(&[0, 0]);
        out.extend_from_slice(&session.to_le_bytes());
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// Lifecycle state of one shell, mirroring the client's `TerminalSessionState`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionState {
    Running,
    Detached,
    Exited,
}

impl SessionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Detached => "detached",
            Self::Exited => "exited",
        }
    }
}

/// 一个附着的输出消费者：远程主控连接，或本机 attach 的终端。
///
/// 两者读的是同一个 pty，只是出口不同：远程走 DataChannel，本机走 Unix
/// socket。用同一个枚举做队列的键，pump 读一次 pty 就能把数据分给所有
/// 消费者。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Consumer {
    Remote(u64),
    // Only the Linux socket service (`local.rs`) attaches a local terminal;
    // elsewhere the page lists and ends sessions but cannot open them here.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    Local(u64),
}

/// One shell and the metadata the session list shows.
struct Session {
    pty: crate::platform::terminal::Session,
    name: String,
    state: SessionState,
    created_ms: i64,
    last_active_ms: i64,
    /// The shell's screen, kept as a terminal model rather than a byte log.
    ///
    /// A reattach replays this, so what the controller draws is what was on
    /// screen. Re-feeding a byte log instead runs the output through the
    /// client's parser a second time, which is wrong the moment the log is
    /// cut: byte N of a terminal stream has no meaning on its own, and a
    /// full-screen program that never emits a newline has no safe cut at all —
    /// the byte cap left a `top` replay truncated in the middle of one.
    screen: vt100::Parser,
    /// Input the pty would not take yet. A paste or a slow shell fills the
    /// pty buffer; dropping the rest would lose keystrokes silently.
    pending: Vec<u8>,
    /// 每个消费者待发的输出。只要有消费者，pump 就会读 pty；没人附着时
    /// shell 由 pty 自己的流控挡住，输出等下一次 attach。
    output: BTreeMap<Consumer, Vec<u8>>,
    /// 远程主控最近一次给出的尺寸。
    remote_size: Option<(u16, u16)>,
    /// 本机 attach 最近一次给出的尺寸。
    local_size: Option<(u16, u16)>,
    /// When the last connection let go, for the idle limit.
    detached_since: Option<std::time::Instant>,
}

impl Session {
    /// Pushes as much queued input as the pty accepts right now.
    fn flush_pending(&mut self) {
        while !self.pending.is_empty() {
            match self.pty.write(&self.pending) {
                Ok(0) | Err(_) => break,
                Ok(written) => {
                    self.pending.drain(..written);
                }
            }
        }
    }

    /// Feeds output to the session's screen model.
    ///
    /// The model applies the terminal's own rules — cursor motion, scrolling,
    /// the alternate screen — so what a reattach replays is a screen rather
    /// than a byte log. No scrollback is kept: a reattach shows the visible
    /// screen, which is what `capture-pane` gives the reference server.
    fn record(&mut self, data: &[u8]) {
        self.screen.process(data);
    }

    /// 让一个消费者离开。只有最后一个消费者走掉时才算「没人附着」。
    fn detach(&mut self, consumer: Consumer) {
        if self.output.remove(&consumer).is_none() {
            return;
        }
        match consumer {
            Consumer::Remote(_) => self.remote_size = None,
            Consumer::Local(_) => self.local_size = None,
        }
        self.apply_size();
        if self.output.is_empty() {
            self.detached_since = Some(std::time::Instant::now());
            if self.state == SessionState::Running {
                self.state = SessionState::Detached;
            }
        }
        self.last_active_ms = now_ms();
    }

    /// 把 pty 与屏幕模型的尺寸同步到当前所有附着者。
    ///
    /// 有多个附着者时取各维的最小值，与 tmux 一致：多出来的那端留白，
    /// 谁都不会看到被折行的内容。最后一个附着者离开后恢复成仍附着那端
    /// 的尺寸。
    fn apply_size(&mut self) {
        let size = match (self.remote_size, self.local_size) {
            (Some((remote_cols, remote_rows)), Some((local_cols, local_rows))) => {
                (remote_cols.min(local_cols), remote_rows.min(local_rows))
            }
            (Some(size), None) | (None, Some(size)) => size,
            (None, None) => return,
        };
        let (cols, rows) = clamp_size(size.0, size.1);
        if self.pty.resize(cols, rows).is_err() {
            return;
        }
        // The model has to follow the pty, or the replay would describe a
        // screen of the old size.
        self.screen.screen_mut().set_size(rows, cols);
    }
}

/// Sessions taken out of the table.
///
/// Dropping a session ends its shell, which can block for the pty's hangup
/// grace period while a shell that ignores `SIGHUP` is killed. The table
/// therefore hands ended sessions back instead of dropping them under its
/// lock, and [`Ended::finish`] drops them once the caller has let go.
#[must_use = "dropping ended sessions can block; call `finish` after releasing the table"]
pub(crate) struct Ended {
    sessions: Vec<Session>,
    /// Ended by someone other than the shell itself: every process the
    /// shell's session still holds goes too, background jobs included.
    forced: bool,
}

impl Ended {
    fn exited(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            forced: false,
        }
    }

    fn forced(sessions: Vec<Session>) -> Self {
        Self {
            sessions,
            forced: true,
        }
    }

    /// Ends the shells off the caller's thread: on the blocking pool from the
    /// runtime, on a short-lived thread otherwise (the settings page calls
    /// this from the UI thread). No async worker or frame waits for a shell
    /// to die.
    pub(crate) fn finish(self) {
        if self.sessions.is_empty() {
            return;
        }
        let end = move || {
            for session in self.sessions {
                if self.forced {
                    session.pty.terminate();
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                runtime.spawn_blocking(end);
            }
            Err(_) => {
                if let Err(error) = std::thread::Builder::new()
                    .name("terminal end".into())
                    .spawn(end)
                {
                    tracing::warn!(%error, "terminal shells could not be ended off thread");
                }
            }
        }
    }
}

/// Owns every shell opened by a controller.
///
/// A shell is keyed by the login generation that opened it and the handle
/// its controller picked: the controller chooses handles, so two logins may
/// well pick the same number, and nothing one login does (open, rename,
/// count against the limit, read output) may touch or reveal another's.
/// Names are unique per login because the client lists sessions by name and
/// offers a rename action; a duplicate would make two rows
/// indistinguishable.
#[derive(Default)]
pub(crate) struct Sessions {
    sessions: HashMap<(String, u32), Session>,
    next_name: u64,
}

/// 本机侧看到的会话，给侧边栏与 CLI 用。
#[derive(Clone, Debug)]
pub(crate) struct LocalSession {
    pub handle: u32,
    pub name: String,
    pub shell: String,
    pub state: &'static str,
    pub created_ms: i64,
    /// Shown by `OpenUUYC terminal --list` (Linux only).
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub last_active_ms: i64,
    /// 远程主控是否附着。
    pub remote: bool,
    /// 本机是否已经有终端接上。
    pub local: bool,
    /// 会话是本机自己开的（侧边栏或命令行），不是主控开的。只有主控获准接入
    /// 本机会话时，主控才看得到它。
    pub opened_here: bool,
    /// 前台正在跑的命令；空闲时为 `None`。
    pub foreground: Option<crate::platform::Foreground>,
}

/// 一次本机 pump 的结果。
///
/// The local-terminal items marked `allow(dead_code)` off Linux are driven
/// only by the Linux socket service in `local.rs`; the other platforms list
/// and end sessions but have no local attach yet.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) struct LocalOutput {
    pub data: Vec<u8>,
    /// shell 已经退出；这个消费者已被摘掉，客户端应当收尾。
    pub exited: bool,
}

/// Why a session operation was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub(crate) enum SessionError {
    #[error("终端会话不存在")]
    Gone,
    #[error("终端会话名无效")]
    InvalidName,
    #[error("终端会话名已存在")]
    DuplicateName,
    #[error("终端会话名过长")]
    NameTooLong,
    #[error("终端会话数已达上限")]
    TooMany,
    #[error("输入过快，这段输入未送达终端")]
    InputOverflow,
    /// This machine cannot host a shell for the local user, for the reason
    /// given (no local attach on this platform, or running as root/SYSTEM).
    #[error("{0}")]
    Unavailable(&'static str),
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_millis() as i64)
        .unwrap_or(0)
}

fn key(owner: &str, handle: u32) -> (String, u32) {
    (owner.to_owned(), handle)
}

/// Who a controller's request acts for: its login generation, and whether
/// that login may also reach the shells opened on this machine.
///
/// The second part is the host's 「允许主控接入本机终端会话」 switch
/// ([`crate::features::host::Lease::terminal_local_allowed`]). While it is
/// off, a local session does not exist as far as the controller can tell:
/// it is not listed, and every request naming its handle is answered as for
/// a handle that was never opened.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Owner<'a> {
    account: &'a str,
    local: bool,
}

impl<'a> Owner<'a> {
    pub(crate) fn new(account: &'a str, local: bool) -> Self {
        Self { account, local }
    }

    fn sees(&self, who: &str) -> bool {
        who == self.account || (self.local && who == LOCAL_OWNER)
    }
}

impl<'a> From<&'a str> for Owner<'a> {
    /// A controller that may see only its own sessions.
    fn from(account: &'a str) -> Self {
        Self::new(account, false)
    }
}

impl Sessions {
    /// Opens a shell, or reattaches to one, and returns its handle, name and
    /// any output to replay. `connection` becomes the one its output goes to.
    ///
    /// The controller normally picks the handle, but its `open` frame may
    /// carry zero when it wants the host to allocate one; the returned handle
    /// is what later `stdin`/`resize` frames must use. Re-opening an existing
    /// handle re-uses the shell and returns its screen, which is what the
    /// controller redraws on reattach.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn open<'a>(
        &mut self,
        owner: impl Into<Owner<'a>>,
        handle: u32,
        connection: u64,
        shell: &str,
        name: Option<String>,
        cols: u16,
        rows: u16,
    ) -> Result<(u32, String, Vec<u8>), SessionError> {
        let owner = owner.into();
        let handle = if handle == 0 {
            self.free_handle(owner.account)
        } else {
            handle
        };
        // Re-opening an existing handle happens on reconnect and when the
        // size changes; replacing the pty would hang up a shell in use.
        if let Some(key) = self.resolve(owner, handle)
            && let Some(existing) = self.sessions.get_mut(&key)
        {
            if existing.state != SessionState::Exited {
                existing.state = SessionState::Running;
            }
            existing.remote_size = Some((cols, rows));
            // A reopen is a takeover, as it always was: the controller
            // reconnected (or moved the tab to another connection), and the
            // old connection's queue would otherwise stay until its channel
            // finally closes, tens of seconds on a lost network. Reaching the
            // high-water mark there stops the shell for everyone. A local
            // terminal attached to the same shell stays.
            existing.output.retain(|consumer, _| match consumer {
                Consumer::Remote(other) => *other == connection,
                Consumer::Local(_) => true,
            });
            existing
                .output
                .entry(Consumer::Remote(connection))
                .or_default();
            existing.detached_since = None;
            existing.apply_size();
            existing.last_active_ms = now_ms();
            return Ok((
                handle,
                existing.name.clone(),
                existing.screen.screen().state_formatted(),
            ));
        }
        let owner = owner.account;
        if self.owned(owner).count() >= MAX_SESSIONS || self.sessions.len() >= MAX_TOTAL {
            return Err(SessionError::TooMany);
        }
        let name = match name {
            Some(name) => validate_name(&name)?,
            None => self.default_name(owner),
        };
        if self.owned(owner).any(|(_, session)| session.name == name) {
            return Err(SessionError::DuplicateName);
        }
        let (cols, rows) = clamp_size(cols, rows);
        let pty = crate::platform::terminal::Session::open(shell, cols, rows)
            .map_err(|_| SessionError::Gone)?;
        let stamp = now_ms();
        self.sessions.insert(
            key(owner, handle),
            Session {
                pty,
                name: name.clone(),
                state: SessionState::Running,
                created_ms: stamp,
                last_active_ms: stamp,
                screen: vt100::Parser::new(rows, cols, 0),
                pending: Vec::new(),
                output: BTreeMap::from([(Consumer::Remote(connection), Vec::new())]),
                remote_size: Some((cols, rows)),
                local_size: None,
                detached_since: None,
            },
        );
        Ok((handle, name, Vec::new()))
    }

    /// 本机主动开一个会话，不经过主控。
    ///
    /// 这类会话不在 30 分钟空闲清扫的范围里，接上再断开也一样：用户自己开
    /// 的东西应当等用户自己关。shell 自己退出后，清扫会把它收走（见
    /// [`Sessions::sweep`]）。编号从 [`LOCAL_HANDLE_BASE`] 起，与手机端
    /// 常用的小编号分开。
    pub(crate) fn create_local(&mut self, name: Option<String>) -> Result<u32, SessionError> {
        if self.sessions.len() >= MAX_TOTAL {
            return Err(SessionError::TooMany);
        }
        let name = match name {
            Some(name) => validate_name(&name)?,
            None => self.default_name(LOCAL_OWNER),
        };
        if self
            .owned(LOCAL_OWNER)
            .any(|(_, session)| session.name == name)
        {
            return Err(SessionError::DuplicateName);
        }
        let handle = (LOCAL_HANDLE_BASE..u32::MAX)
            .find(|handle| !self.sessions.contains_key(&key(LOCAL_OWNER, *handle)))
            .unwrap_or(LOCAL_HANDLE_BASE);
        let shell = Sessions::default_shell();
        // 尺寸只是占位：接上本地终端时 `attach_local` 会按那端改。
        let pty = crate::platform::terminal::Session::open(&shell, 80, 24)
            .map_err(|_| SessionError::Gone)?;
        let stamp = now_ms();
        self.sessions.insert(
            key(LOCAL_OWNER, handle),
            Session {
                pty,
                name,
                state: SessionState::Detached,
                created_ms: stamp,
                last_active_ms: stamp,
                screen: vt100::Parser::new(24, 80, 0),
                pending: Vec::new(),
                output: BTreeMap::new(),
                remote_size: None,
                local_size: None,
                detached_since: None,
            },
        );
        Ok(handle)
    }

    fn owned<'a>(&'a self, owner: &'a str) -> impl Iterator<Item = (u32, &'a Session)> + 'a {
        self.sessions
            .iter()
            .filter(move |((who, _), _)| who == owner)
            .map(|((_, handle), session)| (*handle, session))
    }

    /// 按编号找会话：先找这个账号自己的；主控获准接入本机会话时再找本机
    /// 开的那些（编号从 1000 起，与主控自己的小编号不会撞）。
    fn resolve(&self, owner: Owner<'_>, handle: u32) -> Option<(String, u32)> {
        let own = key(owner.account, handle);
        if self.sessions.contains_key(&own) {
            return Some(own);
        }
        let local = key(LOCAL_OWNER, handle);
        (owner.local && self.sessions.contains_key(&local)).then_some(local)
    }

    fn get_mut(&mut self, owner: Owner<'_>, handle: u32) -> Result<&mut Session, SessionError> {
        let key = self.resolve(owner, handle).ok_or(SessionError::Gone)?;
        self.sessions.get_mut(&key).ok_or(SessionError::Gone)
    }

    /// The lowest handle this login does not use; the controller's handle
    /// space starts at 1, leaving 0 free to mean "assign one".
    fn free_handle(&self, owner: &str) -> u32 {
        (1..u32::MAX)
            .find(|handle| !self.sessions.contains_key(&key(owner, *handle)))
            .unwrap_or(1)
    }

    /// `shell-N`, skipping names this login already uses so a closed
    /// session's number is not immediately reused by a different shell.
    fn default_name(&mut self, owner: &str) -> String {
        for _ in 0..=MAX_SESSIONS {
            self.next_name = self.next_name.wrapping_add(1);
            let name = format!("shell-{}", self.next_name);
            if !self.owned(owner).any(|(_, session)| session.name == name) {
                return name;
            }
        }
        format!("shell-{}", self.next_name)
    }

    /// The default shell, matching what a terminal emulator would start.
    ///
    /// The platform decides: `$SHELL` (falling back to the system's login
    /// shell) on POSIX, `ComSpec` on Windows.
    pub(crate) fn default_shell() -> String {
        crate::platform::terminal::default_shell()
    }

    /// The sessions `owner` may see, as the objects the client parses.
    pub(crate) fn list<'a>(&self, owner: impl Into<Owner<'a>>) -> Vec<Value> {
        let owner = owner.into();
        // 本机自己开的会话只在主控获准接入时列出。
        let mut listed: Vec<_> = self
            .sessions
            .iter()
            .filter(|((who, _), _)| owner.sees(who))
            .map(|((_, handle), session)| {
                json!({
                    "session_id": handle,
                    "state": session.state.as_str(),
                    "name": session.name,
                    "created_at_ms": session.created_ms,
                    "last_active_ms": session.last_active_ms,
                    "shell": session.pty.shell(),
                })
            })
            .collect();
        listed.sort_by_key(Value::to_string);
        listed
    }

    /// A digest of what [`Sessions::list`] shows `owner`, without the
    /// activity time that moves with every keystroke.
    ///
    /// Each terminal connection compares it once per tick and tells its
    /// controller to re-list when it changes. Comparing what this controller
    /// can see, rather than counting changes to the whole table, keeps
    /// another login's shells and hidden local shells from showing through
    /// the notifications, and still catches the local-session grant turning
    /// on or off, which changes the list without touching the table.
    pub(crate) fn listing<'a>(&self, owner: impl Into<Owner<'a>>) -> u64 {
        use std::hash::{Hash as _, Hasher as _};
        let owner = owner.into();
        let mut visible: Vec<_> = self
            .sessions
            .iter()
            .filter(|((who, _), _)| owner.sees(who))
            .map(|((_, handle), session)| {
                (
                    *handle,
                    session.state.as_str(),
                    session.name.as_str(),
                    session.created_ms,
                    session.pty.shell(),
                )
            })
            .collect();
        visible.sort_unstable();
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        visible.hash(&mut hasher);
        hasher.finish()
    }

    /// Queues keyboard input and pushes what the pty accepts right now.
    ///
    /// Input that does not fit is refused as a whole frame. Dropping part of
    /// the queue instead would hand the shell the start of one command and
    /// the end of another, which it would then run.
    pub(crate) fn write<'a>(
        &mut self,
        owner: impl Into<Owner<'a>>,
        handle: u32,
        data: &[u8],
    ) -> Result<(), SessionError> {
        let session = self.get_mut(owner.into(), handle)?;
        write_to(session, data)
    }

    /// Applies a new size, which the shell sees as `SIGWINCH`.
    pub(crate) fn resize<'a>(
        &mut self,
        owner: impl Into<Owner<'a>>,
        handle: u32,
        cols: u16,
        rows: u16,
    ) -> Result<(), SessionError> {
        let session = self.get_mut(owner.into(), handle)?;
        session.remote_size = Some((cols, rows));
        session.apply_size();
        session.last_active_ms = now_ms();
        Ok(())
    }

    /// Renames one of `owner`'s sessions.
    pub(crate) fn rename<'a>(
        &mut self,
        owner: impl Into<Owner<'a>>,
        handle: u32,
        name: &str,
    ) -> Result<(), SessionError> {
        let owner = owner.into();
        let name = validate_name(name)?;
        let key = self.resolve(owner, handle).ok_or(SessionError::Gone)?;
        let duplicate = self.sessions.iter().any(|((who, other), session)| {
            owner.sees(who) && (*other != handle || who != &key.0) && session.name == name
        });
        if duplicate {
            return Err(SessionError::DuplicateName);
        }
        self.get_mut(owner, handle)?.name = name;
        Ok(())
    }

    /// Ends every session `owner` owns.
    pub(crate) fn kill_owned(&mut self, owner: &str) -> Ended {
        let keys: Vec<_> = self
            .sessions
            .keys()
            .filter(|(who, _)| who == owner)
            .cloned()
            .collect();
        Ended::forced(
            keys.iter()
                .filter_map(|key| self.sessions.remove(key))
                .collect(),
        )
    }

    /// Ends every session, whoever opened it.
    pub(crate) fn kill_all(&mut self) -> Ended {
        Ended::forced(self.sessions.drain().map(|(_, session)| session).collect())
    }

    /// Removes one of `owner`'s sessions; finishing the result ends the shell.
    pub(crate) fn kill<'a>(
        &mut self,
        owner: impl Into<Owner<'a>>,
        handle: u32,
    ) -> Result<Ended, SessionError> {
        let key = self
            .resolve(owner.into(), handle)
            .ok_or(SessionError::Gone)?;
        let session = self.sessions.remove(&key).ok_or(SessionError::Gone)?;
        Ok(Ended::forced(vec![session]))
    }

    /// Lets go of one of `owner`'s sessions without ending the shell.
    pub(crate) fn detach<'a>(
        &mut self,
        owner: impl Into<Owner<'a>>,
        handle: u32,
    ) -> Result<(), SessionError> {
        let session = self.get_mut(owner.into(), handle)?;
        let remotes: Vec<_> = session
            .output
            .keys()
            .filter(|consumer| matches!(consumer, Consumer::Remote(_)))
            .copied()
            .collect();
        for consumer in remotes {
            session.detach(consumer);
        }
        Ok(())
    }

    /// 本机终端接上一个会话，返回名字与要重放的屏幕。
    ///
    /// 本机不按登录账号过滤：进程就是同一个用户，侧边栏列出的是这台
    /// 机器上正在跑的 shell。消费者用一个独立编号，与远程连接互不影响。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn attach_local(
        &mut self,
        handle: u32,
        consumer: u64,
        cols: u16,
        rows: u16,
    ) -> Result<(String, Vec<u8>), SessionError> {
        let key = self.local_key(handle).ok_or(SessionError::Gone)?;
        let session = self.sessions.get_mut(&key).ok_or(SessionError::Gone)?;
        if session.state == SessionState::Exited {
            return Err(SessionError::Gone);
        }
        session.state = SessionState::Running;
        session.output.entry(Consumer::Local(consumer)).or_default();
        session.local_size = Some((cols, rows));
        session.detached_since = None;
        session.apply_size();
        session.last_active_ms = now_ms();
        Ok((
            session.name.clone(),
            session.screen.screen().state_formatted(),
        ))
    }

    /// 本机终端自己离开了；shell 留下来等下一次 attach。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn detach_local(&mut self, consumer: u64) {
        for session in self.sessions.values_mut() {
            session.detach(Consumer::Local(consumer));
        }
    }

    /// 本机终端输入的字节写回 pty。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn write_local(&mut self, handle: u32, data: &[u8]) -> Result<(), SessionError> {
        let key = self.local_key(handle).ok_or(SessionError::Gone)?;
        let session = self.sessions.get_mut(&key).ok_or(SessionError::Gone)?;
        write_to(session, data)
    }

    /// 本机终端改了大小。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn resize_local(
        &mut self,
        handle: u32,
        cols: u16,
        rows: u16,
    ) -> Result<(), SessionError> {
        let key = self.local_key(handle).ok_or(SessionError::Gone)?;
        let session = self.sessions.get_mut(&key).ok_or(SessionError::Gone)?;
        session.local_size = Some((cols, rows));
        session.apply_size();
        session.last_active_ms = now_ms();
        Ok(())
    }

    /// 本机侧看到的会话列表。
    pub(crate) fn list_local(&self) -> Vec<LocalSession> {
        let mut listed: Vec<_> = self
            .sessions
            .iter()
            .map(|((who, handle), session)| LocalSession {
                handle: *handle,
                opened_here: who == LOCAL_OWNER,
                name: session.name.clone(),
                shell: session.pty.shell().to_owned(),
                state: session.state.as_str(),
                created_ms: session.created_ms,
                last_active_ms: session.last_active_ms,
                remote: session
                    .output
                    .keys()
                    .any(|consumer| matches!(consumer, Consumer::Remote(_))),
                local: session
                    .output
                    .keys()
                    .any(|consumer| matches!(consumer, Consumer::Local(_))),
                foreground: session.pty.foreground(),
            })
            .collect();
        listed.sort_by_key(|session| session.created_ms);
        listed
    }

    /// 按编号找会话。编号只在同一个所有者内唯一：本机开的会话优先（主控
    /// 看不到它时，可能自己开了一个同号的），其余取字典序最小的账号。
    fn local_key(&self, handle: u32) -> Option<(String, u32)> {
        let mut keys: Vec<_> = self
            .sessions
            .keys()
            .filter(|(_, candidate)| *candidate == handle)
            .cloned()
            .collect();
        keys.sort_by(|a, b| (a.0 != LOCAL_OWNER, &a.0).cmp(&(b.0 != LOCAL_OWNER, &b.0)));
        keys.into_iter().next()
    }

    /// Lets go of every session attached to `connection`, which has closed.
    pub(crate) fn release(&mut self, connection: u64) {
        for session in self.sessions.values_mut() {
            session.detach(Consumer::Remote(connection));
        }
    }

    /// Takes `connection` off the shells opened on this machine, which it may
    /// no longer reach, and returns their handles. The shells keep running:
    /// they belong to the local user, not to the controller.
    pub(crate) fn release_local(&mut self, connection: u64) -> Vec<u32> {
        let mut released = Vec::new();
        for ((who, handle), session) in self.sessions.iter_mut() {
            if who == LOCAL_OWNER && session.output.contains_key(&Consumer::Remote(connection)) {
                session.detach(Consumer::Remote(connection));
                released.push(*handle);
            }
        }
        released.sort_unstable();
        released
    }

    /// Whether `connection` is attached to a shell opened on this machine.
    pub(crate) fn on_local(&self, connection: u64) -> bool {
        self.sessions.iter().any(|((who, _), session)| {
            who == LOCAL_OWNER && session.output.contains_key(&Consumer::Remote(connection))
        })
    }

    /// How many shells are running, for the host's settings page.
    pub(crate) fn count(&self) -> usize {
        self.sessions.len()
    }

    /// 把 pty 里现成的输出读出来，分发给每个消费者的队列。
    ///
    /// 一次读取同时喂屏幕模型和所有队列，所以远程与本机看到的是同一份
    /// 数据。任一个消费者的积压到了上限就不再读这个会话，让 pty 的缓冲
    /// 反压 shell，而不是把内存堆爆。
    fn drain(&mut self, budget: usize) {
        fn readable(session: &Session) -> bool {
            !session.output.is_empty()
                && session.state != SessionState::Exited
                && !session
                    .output
                    .values()
                    .any(|queue| queue.len() >= OUTPUT_HIGH_WATER)
        }
        let mut buffer = [0u8; 8192];
        // Split the budget between the shells someone is reading, so one that
        // floods output cannot starve the others.
        let readers = self.sessions.values().filter(|s| readable(s)).count();
        if readers == 0 {
            return;
        }
        let share = (budget / readers).max(1);
        for session in self.sessions.values_mut() {
            if !readable(session) {
                continue;
            }
            session.flush_pending();
            let mut left = share;
            let mut dry = false;
            while left > 0 {
                let want = left.min(buffer.len());
                match session.pty.read(&mut buffer[..want]) {
                    Ok(Some(0)) | Ok(None) | Err(_) => {
                        dry = true;
                        break;
                    }
                    Ok(Some(read)) => {
                        session.record(&buffer[..read]);
                        for queue in session.output.values_mut() {
                            queue.extend_from_slice(&buffer[..read]);
                        }
                        left -= read;
                        session.last_active_ms = now_ms();
                    }
                }
            }
            // Only a pty read dry counts: a shell that ran out of budget this
            // tick may still hold the last of its output.
            if dry && session.pty.exited() {
                session.state = SessionState::Exited;
            }
        }
    }

    /// Takes up to `budget` bytes the shells attached to `connection` have
    /// written since the last poll.
    ///
    /// The budget is what the data channel can still take, so a shell that
    /// floods output is held back by the pty (the shell blocks writing)
    /// instead of piling up in the send queue. Each shell's exit is reported
    /// once, so the controller learns a session ended without polling the
    /// list.
    pub(crate) fn pump(
        &mut self,
        connection: u64,
        budget: usize,
    ) -> Vec<(u32, FrameType, Vec<u8>)> {
        self.drain(budget);
        let consumer = Consumer::Remote(connection);
        let mut out = Vec::new();
        let mut attached: Vec<_> = self
            .sessions
            .iter_mut()
            .filter(|(_, session)| session.output.contains_key(&consumer))
            .map(|((_, handle), session)| (*handle, session))
            .collect();
        attached.sort_by_key(|(handle, _)| *handle);
        // Split the budget so one busy shell cannot starve the others.
        let share = (budget / attached.len().max(1)).max(1);
        for (handle, session) in attached {
            let Some(queue) = session.output.get_mut(&consumer) else {
                continue;
            };
            if !queue.is_empty() {
                let take = queue.len().min(share);
                out.push((handle, FrameType::Stdout, queue.drain(..take).collect()));
            }
            if session.state == SessionState::Exited && queue.is_empty() {
                // Report the exit once everything before it went out, then
                // drop this consumer so `reap` can recover the session after
                // every other one saw it too.
                session.output.remove(&consumer);
                out.push((handle, FrameType::Exit, b"{}".to_vec()));
            }
        }
        out
    }

    /// 取走本机 attach 的待发输出；`None` 表示这个消费者已经不在表里。
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(crate) fn pump_local(&mut self, consumer: u64, budget: usize) -> Option<LocalOutput> {
        self.drain(budget);
        self.sessions.values_mut().find_map(|session| {
            let queue = session.output.get_mut(&Consumer::Local(consumer))?;
            let take = queue.len().min(budget);
            let data: Vec<u8> = queue.drain(..take).collect();
            // The exit goes out after the last of the output, not with it.
            let exited = session.state == SessionState::Exited && queue.is_empty();
            if exited {
                session.output.remove(&Consumer::Local(consumer));
            }
            Some(LocalOutput { data, exited })
        })
    }

    /// Takes out the sessions whose exit every attached consumer has seen.
    ///
    /// A session is not removed on the first pump that notices the shell
    /// ended: the other end (a local attach, another connection) still has
    /// to be told, and its queue only empties once it was.
    pub(crate) fn reap(&mut self) -> Ended {
        let exited: Vec<_> = self
            .sessions
            .iter()
            .filter(|(_, session)| {
                session.state == SessionState::Exited && session.output.is_empty()
            })
            .map(|(key, _)| key.clone())
            .collect();
        Ended::exited(
            exited
                .iter()
                .filter_map(|key| self.sessions.remove(key))
                .collect(),
        )
    }

    /// Takes out the shells nobody is attached to that have exited, and ends
    /// those a controller opened and nobody has attached to for `idle`.
    ///
    /// The shells opened on this machine are never ended for being idle, even
    /// after a local terminal attached and left: the local user closes them.
    /// Once one exits it is taken out like any other, so it does not sit in
    /// the list as exited, holding a slot, until someone presses 结束.
    pub(crate) fn sweep(&mut self, idle: std::time::Duration) -> (Ended, Ended) {
        let mut exited = Vec::new();
        let mut abandoned = Vec::new();
        for (key, session) in self.sessions.iter_mut() {
            if !session.output.is_empty() {
                continue;
            }
            if session.pty.exited() {
                exited.push(key.clone());
            } else if key.0 != LOCAL_OWNER
                && session
                    .detached_since
                    .is_some_and(|since| since.elapsed() >= idle)
            {
                abandoned.push(key.clone());
            }
        }
        let mut take = |keys: Vec<(String, u32)>| -> Vec<Session> {
            keys.iter()
                .filter_map(|key| self.sessions.remove(key))
                .collect()
        };
        (Ended::exited(take(exited)), Ended::forced(take(abandoned)))
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }
}

/// 把输入排进会话并尽量交给 pty。
fn write_to(session: &mut Session, data: &[u8]) -> Result<(), SessionError> {
    session.last_active_ms = now_ms();
    session.flush_pending();
    if session.pending.len() + data.len() > PENDING_LIMIT {
        return Err(SessionError::InputOverflow);
    }
    session.pending.extend_from_slice(data);
    session.flush_pending();
    Ok(())
}

/// Maps the controller's shell choice to a real program.
///
/// The `open` payload names a shell *kind* (`powershell`, `cmd`, `bash`), not
/// a path, and each platform knows which kinds it can offer. The platform
/// layer decides; this stays as a named seam so the call sites and tests here
/// read the same on every OS.
fn resolve_shell(requested: &str) -> String {
    crate::platform::terminal::resolve_shell(requested)
}

/// Rejects the names the client's own validation rejects, so a rename that
/// the server accepts is never refused at the client afterwards.
fn validate_name(name: &str) -> Result<String, SessionError> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return Err(SessionError::InvalidName);
    }
    if trimmed.chars().count() > MAX_NAME {
        return Err(SessionError::NameTooLong);
    }
    // The client reports `errorInvalidChar`; keep names to what it can render
    // and what survives a round trip through the request payload.
    if trimmed
        .chars()
        .any(|c| c.is_control() || matches!(c, '/' | '\\'))
    {
        return Err(SessionError::InvalidName);
    }
    Ok(trimmed.to_owned())
}

/// The process-wide shell table.
///
/// Terminals outlive a single connection: the controller reconnects
/// routinely (screen rotation, app switching) and expects the same sessions
/// to still be there. Keeping the table per `Screens` would drop every shell
/// when a connection closed, so one table serves the whole process, the way
/// the reference server keeps its sessions in tmux.
static SESSIONS: std::sync::OnceLock<Arc<std::sync::Mutex<Sessions>>> = std::sync::OnceLock::new();

pub(crate) fn shared() -> Arc<std::sync::Mutex<Sessions>> {
    Arc::clone(SESSIONS.get_or_init(|| {
        let table = Arc::new(std::sync::Mutex::new(Sessions::default()));
        let janitor = Arc::downgrade(&table);
        if let Err(error) = std::thread::Builder::new()
            .name("terminal sweep".into())
            .spawn(move || sweep_forever(janitor))
        {
            tracing::warn!(%error, "terminal idle sweep could not start");
        }
        // 本机终端要能接上这些会话，socket 服务随表一起起来（Linux）。Unit
        // tests use this table too, and must not replace the socket of an
        // OpenUUYC running on the same machine; they serve a private path.
        if !cfg!(test) {
            local::serve(Arc::clone(&table));
        }
        table
    }))
}

/// Every [`SWEEP_INTERVAL`], takes out shells that exited while nobody was
/// attached and ends those left detached longer than [`IDLE_LIMIT`].
fn sweep_forever(table: std::sync::Weak<std::sync::Mutex<Sessions>>) {
    loop {
        std::thread::sleep(SWEEP_INTERVAL);
        let Some(table) = table.upgrade() else {
            return;
        };
        let (exited, abandoned) = lock_table(&table).sweep(IDLE_LIMIT);
        exited.finish();
        abandoned.finish();
    }
}

fn lock_table(table: &std::sync::Mutex<Sessions>) -> std::sync::MutexGuard<'_, Sessions> {
    table.lock().unwrap_or_else(|e| e.into_inner())
}

/// Ends every shell a login generation opened, whether or not a controller is
/// connected right now. Called wherever that login's grant ends: sign-out or
/// switching accounts ([`crate::features::host::Handle::retire`]), turning the
/// terminal or remote control off, and the local user's "disconnect".
pub(crate) fn end_login(account: &str) {
    // Nothing to end in a process that never served a terminal (the Windows
    // service, any platform without a backend); do not create the table.
    let Some(table) = SESSIONS.get() else {
        return;
    };
    let ended = lock_table(table).kill_owned(account);
    if !ended.sessions.is_empty() {
        tracing::info!(
            sessions = ended.sessions.len(),
            "host terminal shells ended with their grant"
        );
    }
    ended.finish();
}

/// Ends every shell, for the host's "end all" button.
pub(crate) fn end_all() {
    if let Some(table) = SESSIONS.get() {
        lock_table(table).kill_all().finish();
    }
}

/// Why this host cannot serve a terminal; `None` when it can.
///
/// The platform answers for this process (root on POSIX, LocalSystem on
/// Windows). In unattended mode the shell would come from the SYSTEM service
/// rather than from this process, so the settings page, which runs as the
/// user, is told the same thing the service would answer and does not offer
/// a switch every connection would refuse.
pub(crate) fn unavailable() -> Option<&'static str> {
    crate::platform::terminal::unavailable().or_else(|| {
        crate::platform::host_service::resident::managed()
            .then_some("无人值守模式以 SYSTEM 身份运行，不提供终端")
    })
}

/// Why the local user cannot open a shell from this machine's sessions page
/// or command line; `None` when they can.
///
/// The same refusals as the controller's terminal apply (root on POSIX,
/// SYSTEM on Windows), plus platforms without a local attach yet, so the page
/// never offers a button that is bound to fail.
pub(crate) fn local_unavailable() -> Option<&'static str> {
    if !local::SUPPORTED {
        return Some(local::UNSUPPORTED);
    }
    unavailable()
}

/// How many shells this process is running, for the host's settings page.
pub(crate) fn running() -> usize {
    SESSIONS.get().map_or(0, |table| lock_table(table).count())
}

/// 本机侧看到的会话列表，给侧边栏用。
pub(crate) fn local_sessions() -> Vec<LocalSession> {
    SESSIONS
        .get()
        .map(|table| lock_table(table).list_local())
        .unwrap_or_default()
}

/// 结束一个会话（本机侧，侧边栏的「结束」）。`opened_here` 来自页面上那一
/// 行，主控与本机各有一个同号会话时结束的是点的那一个。
pub(crate) fn kill_local(handle: u32, opened_here: bool) -> bool {
    let Some(table) = SESSIONS.get() else {
        return false;
    };
    let ended = {
        let mut sessions = lock_table(table);
        let owner = sessions
            .sessions
            .keys()
            .filter(|(who, candidate)| *candidate == handle && (who == LOCAL_OWNER) == opened_here)
            .map(|(who, _)| who.clone())
            .min();
        owner.and_then(|owner| sessions.kill(owner.as_str(), handle).ok())
    };
    match ended {
        Some(ended) => {
            ended.finish();
            true
        }
        None => false,
    }
}

/// 当前平台能不能报出终端里的前台命令。
pub(crate) fn foreground_supported() -> bool {
    crate::platform::terminal::foreground_supported()
}

/// 本机新建一个会话，返回它的编号。
pub(crate) fn create_local(name: Option<String>) -> Result<u32, SessionError> {
    if let Some(reason) = local_unavailable() {
        return Err(SessionError::Unavailable(reason));
    }
    lock_table(&shared()).create_local(name)
}

/// 在系统终端模拟器里接上这个会话。
pub(crate) fn open_local_terminal(handle: u32) -> anyhow::Result<()> {
    local::open_in_terminal(handle)
}

/// 本机 attach 的消费者编号，与远程连接分开计。
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(crate) fn next_local_consumer() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Numbers each terminal connection, so a shell knows whose pump reads it.
fn next_connection() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// Binds the terminal handler to `channel`.
///
/// Protobuf requests (the environment check, rename) go through the shared
/// session handler. `TERM` frames are handled here. Responses return on the
/// channel the request arrived on.
pub(crate) fn bind(
    channel: &Arc<RTCDataChannel>,
    screens: Arc<tokio::sync::Mutex<crate::features::host::peer::screens::Screens>>,
    cancel: CancellationToken,
) {
    let connection = next_connection();
    let stop = cancel.child_token();
    let close = stop.clone();
    channel.on_close(Box::new(move || {
        close.cancel();
        Box::pin(async {})
    }));
    // The message closure consumes the originals, so keep a handle for the
    // output pump first.
    let pump_screens = screens.clone();
    let pump_stop = stop.clone();
    let weak = Arc::downgrade(channel);
    channel.on_message(Box::new(move |message| {
        let weak = weak.clone();
        let screens = screens.clone();
        let stop = stop.clone();
        Box::pin(async move {
            if stop.is_cancelled() {
                return;
            }
            let Some(channel) = weak.upgrade() else {
                return;
            };
            let data = message.data.clone();
            if data.starts_with(MAGIC) {
                handle_frame(&channel, &screens, &data, connection, &stop).await;
                return;
            }
            let responses = {
                let mut state = screens.lock().await;
                crate::features::stream_control::publisher::receive_session(
                    &mut state, &data, false,
                )
                .await
            };
            match responses {
                Ok(responses) => {
                    for response in responses.messages {
                        if stop.is_cancelled() {
                            break;
                        }
                        if let Err(error) = channel.send(&Bytes::from(response)).await {
                            tracing::warn!(%error, "host terminal response failed");
                            break;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(
                        %error,
                        bytes = data.len(),
                        "rejected host terminal message"
                    );
                }
            }
        })
    }));

    // Pump shell output. The read side is non-blocking, so poll on a short
    // interval; a shell that prints nothing costs one lock per tick.
    let pump_channel = Arc::downgrade(channel);
    let table = shared();
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(std::time::Duration::from_millis(20));
        // What the access notice was last told about this connection being in
        // a local shell.
        let mut notice_on_local = false;
        // The session list this controller saw last; the first tick only
        // records it, so connecting does not itself send a notification.
        let mut listed: Option<u64> = None;
        loop {
            ticker.tick().await;
            let Some(channel) = pump_channel.upgrade().filter(|_| !pump_stop.is_cancelled()) else {
                break;
            };
            // The grant is re-read every tick, not only when a frame comes
            // in: a controller that only watches output must lose it as soon
            // as the terminal is turned off, remote control is disabled or
            // the login ends. Ending the shells is the job of those switches
            // (`end_login`); a lease that merely went stale (signalling
            // reconnected) only closes this connection, so the shells wait
            // for the reattach.
            let lease = pump_screens.lock().await.authorization();
            if !lease.terminal_allowed() {
                tracing::info!("host terminal grant ended; closing the terminal connection");
                pump_stop.cancel();
                cancel.cancel();
                let _ = channel.close().await;
                break;
            }
            // The same goes for the shells opened on this machine: once the
            // controller may no longer reach them, it lets go of them within
            // a tick, and each is reported as ended to the controller (an
            // error saying why, then its exit) so the phone closes the tab
            // instead of waiting on a terminal that went quiet. The shells
            // themselves keep running for the local user.
            let mut frames = Vec::new();
            if !lease.terminal_local_allowed() {
                for handle in lock_table(&table).release_local(connection) {
                    frames.push((
                        handle,
                        FrameType::Error,
                        reply_payload(1, "本机已收回对这个本机终端会话的访问", handle),
                    ));
                    frames.push((handle, FrameType::Exit, b"{}".to_vec()));
                }
            }
            // The official controller only takes a list (type 10) as the
            // answer to its own request (type 9) and drops one pushed
            // unasked. The reference host instead sends the empty
            // `TerminalSessionChanged` control message, and the controller
            // re-lists. It goes out the way this channel's other control
            // replies do.
            let listing = lock_table(&table)
                .listing(Owner::new(lease.account(), lease.terminal_local_allowed()));
            if listed.is_some_and(|before| before != listing) {
                tracing::debug!("host terminal session list changed");
                let changed =
                    crate::features::stream_control::publisher::terminal_session_changed();
                if let Err(error) = channel.send(&Bytes::from(changed)).await {
                    tracing::warn!(%error, "host terminal list notice failed");
                    pump_stop.cancel();
                    break;
                }
            }
            listed = Some(listing);
            let on_local = lock_table(&table).on_local(connection);
            if on_local != notice_on_local {
                lease.terminal_on_local(on_local);
                notice_on_local = on_local;
            }
            let room = OUTPUT_HIGH_WATER.saturating_sub(channel.buffered_amount().await);
            if room > 0 {
                frames.extend(lock_table(&table).pump(connection, room.min(PUMP_BUDGET)));
            }
            let mut failed = false;
            for (handle, kind, payload) in frames {
                let frame = Frame::encode(kind, handle, &payload);
                if let Err(error) = channel.send(&Bytes::from(frame)).await {
                    tracing::warn!(%error, "host terminal output failed");
                    failed = true;
                    break;
                }
            }
            if failed {
                // Output can no longer reach the controller; stop taking its
                // input too, and let the shells wait for a reattach.
                pump_stop.cancel();
                break;
            }
            lock_table(&table).reap().finish();
        }
        lock_table(&table).release(connection);
    });
}

/// Reads the request payload as JSON, falling back to `u16` columns and rows.
///
/// The client sends `open` and `resize` as JSON objects; keeping the binary
/// fallback means a version that packs the pair differently still resizes.
fn geometry(payload: &[u8]) -> (Option<u16>, Option<u16>) {
    if let Ok(value) = serde_json::from_slice::<Value>(payload) {
        let number = |key: &str| {
            value
                .get(key)
                .and_then(Value::as_u64)
                .and_then(|v| u16::try_from(v).ok())
        };
        return (number("cols"), number("rows"));
    }
    if payload.len() >= 4 {
        return (
            Some(u16::from_le_bytes([payload[0], payload[1]])),
            Some(u16::from_le_bytes([payload[2], payload[3]])),
        );
    }
    (None, None)
}

/// The `code`/`message`/`session_id` object the controller reads in replies
/// and `Error` frames.
fn reply_payload(code: i32, message: &str, session: u32) -> Vec<u8> {
    serde_json::to_vec(&json!({
        "code": code,
        "message": message,
        "session_id": session,
    }))
    .unwrap_or_default()
}

/// Translates a Signal frame into the terminal control character it stands for.
///
/// The official controller sends JSON such as `{"sig":"SIGINT"}`. Only the
/// signals the terminal driver turns into a key have a byte; anything else
/// returns `None` and the caller logs and drops it.
fn signal_keystroke(payload: &[u8]) -> Option<u8> {
    let value: Value = serde_json::from_slice(payload).ok()?;
    match value.get("sig")?.as_str()? {
        "SIGINT" => Some(0x03),
        "SIGQUIT" => Some(0x1c),
        "SIGTSTP" => Some(0x1a),
        _ => None,
    }
}

async fn handle_frame(
    channel: &Arc<RTCDataChannel>,
    screens: &Arc<tokio::sync::Mutex<crate::features::host::peer::screens::Screens>>,
    data: &[u8],
    connection: u64,
    stop: &CancellationToken,
) {
    let Some(frame) = Frame::parse(data) else {
        tracing::warn!(bytes = data.len(), "host terminal frame unparsed");
        return;
    };
    // Metadata only: payloads carry keystrokes, screen contents and replayed
    // history, none of which may reach the log files or a diagnostics bundle.
    tracing::debug!(
        kind = ?frame.kind,
        session = frame.session,
        bytes = frame.payload.len(),
        "host terminal frame"
    );
    // Re-check the grant on every frame, not only when the connection was
    // accepted. A refused frame is answered and dropped; ending the shells is
    // up to whatever ended the grant (`end_login`), because a lease that only
    // went stale in a signalling reconnect must not take the shells with it.
    // The output pump closes the connection within a tick.
    let (allowed, local, account, table) = {
        let state = screens.lock().await;
        let lease = state.authorization();
        (
            lease.terminal_allowed(),
            lease.terminal_local_allowed(),
            lease.account().to_owned(),
            state.terminal(),
        )
    };
    let owner = Owner::new(&account, local);
    let reply = reply_payload;
    let mut outgoing = Vec::new();
    if !allowed {
        tracing::warn!(kind = ?frame.kind, "host terminal frame refused");
        match frame.kind {
            FrameType::Open => {
                outgoing.push((
                    FrameType::OpenResp,
                    frame.session,
                    reply(1, "本机未允许终端", frame.session),
                ));
            }
            FrameType::KillSessionReq => {
                outgoing.push((
                    FrameType::KillSessionResp,
                    frame.session,
                    reply(1, "本机未允许终端", frame.session),
                ));
            }
            FrameType::ListSessionsReq => {
                outgoing.push((FrameType::ListSessionsResp, frame.session, b"[]".to_vec()));
            }
            _ => {}
        }
    } else {
        match frame.kind {
            FrameType::ListSessionsReq => {
                let listed = lock_table(&table).list(owner);
                let payload =
                    serde_json::to_vec(&Value::Array(listed)).unwrap_or_else(|_| b"[]".into());
                outgoing.push((FrameType::ListSessionsResp, frame.session, payload));
            }
            FrameType::Open => {
                let text = serde_json::from_slice::<Value>(&frame.payload).ok();
                let (cols, rows) = geometry(&frame.payload);
                let shell = match text
                    .as_ref()
                    .and_then(|v| v.get("shell").and_then(Value::as_str))
                {
                    Some(requested) if !requested.is_empty() => resolve_shell(requested),
                    _ => Sessions::default_shell(),
                };
                tracing::info!(requested = shell, "host terminal shell selected");
                let name = text
                    .as_ref()
                    .and_then(|v| v.get("name").and_then(Value::as_str))
                    .map(str::to_owned);
                let result = lock_table(&table).open(
                    owner,
                    frame.session,
                    connection,
                    &shell,
                    name,
                    cols.unwrap_or(80),
                    rows.unwrap_or(24),
                );
                let (payload, session, replay) = match result {
                    Ok((handle, name, replay)) => (
                        json!({
                            // The client reads `code`/`message` and a session
                            // id; the exact spelling of the id key is not
                            // certain, so every plausible one carries the same
                            // value and the client's decoder picks up
                            // whichever it knows.
                            "code": 0,
                            "message": "",
                            "session_id": handle,
                            "resp_session_id": handle,
                            "respSessionId": handle,
                            "handle": handle,
                            "name": name,
                            "shell": shell,
                        }),
                        handle,
                        replay,
                    ),
                    Err(error) => (
                        json!({
                            "code": 1,
                            "message": error.to_string(),
                            "session_id": frame.session,
                        }),
                        frame.session,
                        Vec::new(),
                    ),
                };
                outgoing.push((
                    FrameType::OpenResp,
                    session,
                    serde_json::to_vec(&payload).unwrap_or_default(),
                ));
                if !replay.is_empty() {
                    // Redraw what the screen last held. The reset and the
                    // clear come first so the replay lands on a blank screen:
                    // the formatted state positions the cursor and sets
                    // attributes itself, but assumes it draws onto one.
                    let mut redraw = Vec::with_capacity(replay.len() + 16);
                    redraw.extend_from_slice(b"\x1b[0m\x1b[2J\x1b[3J\x1b[H");
                    redraw.extend_from_slice(&replay);
                    outgoing.push((FrameType::Stdout, session, redraw));
                }
            }
            FrameType::Stdin => {
                let result = lock_table(&table).write(owner, frame.session, &frame.payload);
                match result {
                    Ok(()) => {}
                    Err(SessionError::InputOverflow) => {
                        tracing::warn!(
                            session = frame.session,
                            bytes = frame.payload.len(),
                            "host terminal input refused: the shell is not reading"
                        );
                        outgoing.push((
                            FrameType::Error,
                            frame.session,
                            reply(1, &SessionError::InputOverflow.to_string(), frame.session),
                        ));
                    }
                    Err(error) => {
                        tracing::debug!(%error, session = frame.session, "host terminal input dropped");
                    }
                }
            }
            FrameType::Resize => {
                let (cols, rows) = geometry(&frame.payload);
                if let (Some(cols), Some(rows)) = (cols, rows)
                    && let Err(error) = lock_table(&table).resize(owner, frame.session, cols, rows)
                {
                    tracing::debug!(%error, session = frame.session, "host terminal resize dropped");
                }
            }
            FrameType::Close | FrameType::KillSessionReq => {
                let result = lock_table(&table).kill(owner, frame.session);
                // End the shell only once the table is released.
                let result = result.map(Ended::finish);
                if frame.kind == FrameType::KillSessionReq {
                    let payload = match result {
                        Ok(()) => reply(0, "", frame.session),
                        Err(error) => reply(1, &error.to_string(), frame.session),
                    };
                    outgoing.push((FrameType::KillSessionResp, frame.session, payload));
                }
            }
            FrameType::Detach => {
                if let Err(error) = lock_table(&table).detach(owner, frame.session) {
                    tracing::debug!(%error, session = frame.session, "host terminal detach dropped");
                }
            }
            FrameType::UnlockReq => {
                // The controller drives a locked machine's unlock sheet with
                // this: the macOS server checks `com.apple.screenIsUnlocked`
                // and replies with `code`/`message`/`success`. We never drive
                // an unlock (that needs the user's password); we only report
                // the state we can actually observe, and an unknown state is
                // not reported as unlocked.
                let payload = match crate::platform::capture::session_locked() {
                    Some(false) => json!({"success": true, "code": 0, "message": ""}),
                    Some(true) => json!({
                        "success": false,
                        "code": 1,
                        "message": "本机已锁屏，不支持远程解锁",
                    }),
                    None => json!({
                        "success": false,
                        "code": 1,
                        "message": "无法确认本机是否锁屏，不支持远程解锁",
                    }),
                };
                outgoing.push((
                    FrameType::UnlockResp,
                    frame.session,
                    serde_json::to_vec(&payload).unwrap_or_default(),
                ));
            }
            FrameType::Signal => {
                // The actual frame is `{"sig":"SIGINT"}` (16 bytes). Writing the
                // control character into the pty is the safest translation: the
                // terminal driver turns it into SIGINT for the foreground
                // process group, the same path a local Ctrl-C takes; killing
                // the process directly would skip the program's own handling
                // (vim, for one, wants to ask first).
                match signal_keystroke(&frame.payload) {
                    Some(byte) => {
                        if let Err(error) = lock_table(&table).write(owner, frame.session, &[byte])
                        {
                            tracing::debug!(
                                %error,
                                session = frame.session,
                                "host terminal signal dropped"
                            );
                        }
                    }
                    None => {
                        // The first bytes are enough to recognise a new
                        // signal; the frame itself can be as long as any.
                        let payload: String = frame
                            .payload
                            .iter()
                            .take(64)
                            .map(|byte| format!("{byte:02x}"))
                            .collect();
                        tracing::debug!(
                            session = frame.session,
                            bytes = frame.payload.len(),
                            payload,
                            "host terminal signal ignored"
                        );
                    }
                }
            }
            other => {
                tracing::debug!(?other, "host terminal frame ignored");
            }
        }
    }
    for (kind, handle, payload) in outgoing {
        if stop.is_cancelled() {
            return;
        }
        let frame = Frame::encode(kind, handle, &payload);
        tracing::debug!(
            kind = ?kind,
            session = handle,
            bytes = payload.len(),
            "host terminal reply"
        );
        if let Err(error) = channel.send(&Bytes::from(frame)).await {
            tracing::warn!(%error, "host terminal reply failed");
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_round_trips() {
        let encoded = Frame::encode(FrameType::ListSessionsResp, 7, b"hello");
        assert_eq!(&encoded[..4], MAGIC);
        assert_eq!(encoded[4], VERSION);
        assert_eq!(encoded[5], 10);
        let frame = Frame::parse(&encoded).expect("parse");
        assert_eq!(frame.kind, FrameType::ListSessionsResp);
        assert_eq!(frame.session, 7);
        assert_eq!(frame.payload, b"hello");
    }

    #[test]
    fn the_captured_list_request_decodes() {
        // The phone's first terminal frame, byte for byte.
        let captured = [
            0x54, 0x45, 0x52, 0x4d, 0x01, 0x09, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let frame = Frame::parse(&captured).expect("parse");
        assert_eq!(frame.kind, FrameType::ListSessionsReq);
        assert_eq!(frame.session, 0);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn protobuf_is_not_mistaken_for_a_frame() {
        // The check_env envelope the phone sends first.
        let protobuf = [
            0x08, 0x05, 0x10, 0x8a, 0x86, 0xb6, 0xed, 0x93, 0x34, 0xaa, 0x01, 0x07, 0x0a, 0x02,
            0x08, 0x01, 0xe2, 0x01, 0x00,
        ];
        assert!(Frame::parse(&protobuf).is_none());
    }

    #[test]
    fn a_truncated_frame_is_refused() {
        let mut encoded = Frame::encode(FrameType::Stdout, 1, b"payload");
        encoded.truncate(HEADER + 2);
        assert!(Frame::parse(&encoded).is_none());
    }

    /// The shell the pty tests drive. `cmd.exe` is present on every Windows
    /// install; `/bin/sh` is on every POSIX one.
    fn shell() -> &'static str {
        if cfg!(windows) { "cmd.exe" } else { "/bin/sh" }
    }

    /// One command line for that shell. `cmd.exe` reads a bare `\n` as part of
    /// the command in some cases, so Windows gets the CRLF it expects.
    fn line(command: &str) -> Vec<u8> {
        if cfg!(windows) {
            format!("{command}\r\n").into_bytes()
        } else {
            format!("{command}\n").into_bytes()
        }
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn sessions_are_keyed_by_the_controller_handle() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 11, 1, shell(), None, 80, 24)
            .expect("open");
        sessions
            .open("acct", 22, 1, shell(), None, 80, 24)
            .expect("open");
        assert_eq!(sessions.list("acct").len(), 2);
        assert!(sessions.write("acct", 11, &line("echo A")).is_ok());
        assert!(sessions.write("acct", 33, &line("echo B")).is_err());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn a_duplicate_name_is_refused() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), Some("build".into()), 80, 24)
            .expect("open");
        assert_eq!(
            sessions.open("acct", 2, 1, shell(), Some("build".into()), 80, 24),
            Err(SessionError::DuplicateName)
        );
    }

    #[test]
    fn invalid_names_are_refused() {
        let mut sessions = Sessions::default();
        assert_eq!(
            sessions.open("acct", 1, 1, shell(), Some("   ".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open("acct", 1, 1, shell(), Some("a/b".into()), 80, 24),
            Err(SessionError::InvalidName)
        );
        assert_eq!(
            sessions.open(
                "acct",
                1,
                1,
                shell(),
                Some("x".repeat(MAX_NAME + 1)),
                80,
                24
            ),
            Err(SessionError::NameTooLong)
        );
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn write_and_resize_reach_the_shell() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.resize("acct", 1, 120, 40).expect("resize");
        sessions
            .write("acct", 1, &line("echo READY"))
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(1, PUMP_BUDGET) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains("READY") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("shell produced: {:?}", String::from_utf8_lossy(&seen));
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn a_reattach_replays_the_screen() {
        let mut sessions = Sessions::default();
        let (handle, _, replay) = sessions
            .open("acct", 0, 1, shell(), None, 80, 24)
            .expect("open");
        assert!(replay.is_empty(), "a fresh session has nothing to replay");
        sessions
            .write("acct", handle, &line("echo HISTORY"))
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            let output: Vec<u8> = sessions
                .pump(1, PUMP_BUDGET)
                .into_iter()
                .filter(|(_, kind, _)| *kind == FrameType::Stdout)
                .flat_map(|(_, _, payload)| payload)
                .collect();
            if String::from_utf8_lossy(&output).contains("HISTORY") {
                break;
            }
        }
        let (again, _, replay) = sessions
            .open("acct", handle, 1, shell(), None, 100, 40)
            .expect("open");
        assert_eq!(again, handle, "reattach keeps the same shell");
        assert!(
            String::from_utf8_lossy(&replay).contains("HISTORY"),
            "reattach replays the screen: {:?}",
            String::from_utf8_lossy(&replay)
        );
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn an_exited_shell_reports_once() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.write("acct", 1, &line("exit")).expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while std::time::Instant::now() < deadline {
            if sessions
                .pump(1, PUMP_BUDGET)
                .iter()
                .any(|(_, kind, _)| *kind == FrameType::Exit)
            {
                sessions.reap().finish();
                assert!(sessions.is_empty());
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("shell did not exit");
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn killing_removes_a_session_and_a_second_kill_is_gone() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.kill("acct", 1).expect("kill").finish();
        assert_eq!(sessions.kill("acct", 1).err(), Some(SessionError::Gone));
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn every_requested_shell_kind_resolves_to_a_real_program() {
        // The Android client always asks for `powershell`. On Windows that is
        // the program itself; elsewhere it falls back to the login shell.
        // Either way the answer has to be a path that exists, because refusing
        // it is what made the phone report a failed session creation.
        assert!(std::path::Path::new(&resolve_shell("powershell")).exists());
        assert!(std::path::Path::new(&resolve_shell("cmd")).exists());
        assert!(std::path::Path::new(&resolve_shell("bash")).exists());
    }

    /// Drains what `connection` would send for a few seconds, until `needle`.
    fn output_until(sessions: &mut Sessions, connection: u64, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(connection, PUMP_BUDGET) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains(needle) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn output_goes_only_to_the_attached_connection() {
        let mut sessions = Sessions::default();
        sessions
            .open("old-login", 1, 7, shell(), None, 80, 24)
            .expect("open");
        sessions
            .write("old-login", 1, b"echo SECRET_$((40+2))\n")
            .expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        // Another login's connection, and a second connection of the same
        // login that never attached, read nothing.
        assert!(sessions.list("new-login").is_empty());
        for _ in 0..10 {
            assert!(sessions.pump(8, PUMP_BUDGET).is_empty());
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(output_until(&mut sessions, 7, "SECRET_42").contains("SECRET_42"));
        // Another login cannot reattach to it either: the handle is its own.
        let (handle, _, replay) = sessions
            .open("new-login", 1, 8, shell(), None, 80, 24)
            .expect("open");
        assert_eq!(handle, 1);
        assert!(
            replay.is_empty(),
            "a new login gets a new shell, not the old screen"
        );
        assert_eq!(sessions.list("old-login").len(), 1);
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn a_released_shell_waits_for_the_reattach() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 7, shell(), None, 80, 24)
            .expect("open");
        sessions.release(7);
        assert_eq!(sessions.list("acct")[0]["state"], "detached");
        sessions.write("acct", 1, b"echo LATER\n").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(
            sessions.pump(7, PUMP_BUDGET).is_empty(),
            "nobody is attached"
        );
        sessions
            .open("acct", 1, 9, shell(), None, 80, 24)
            .expect("reattach");
        assert_eq!(sessions.list("acct")[0]["state"], "running");
        assert!(output_until(&mut sessions, 9, "LATER").contains("LATER"));
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn a_reopen_takes_the_shell_over_from_a_stalled_connection() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 7, shell(), None, 80, 24)
            .expect("open");
        // The phone reconnects while the old channel is still open and no
        // longer drained: only the new connection is pumped.
        sessions
            .open("acct", 1, 9, shell(), None, 80, 24)
            .expect("reopen");
        sessions
            .write("acct", 1, b"yes | head -c 3000000; echo DONE-$((40+2))\n")
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(9, PUMP_BUDGET) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if seen.windows(7).any(|window| window == b"DONE-42") {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        panic!(
            "the new connection stalled after {} bytes behind the old one",
            seen.len()
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "local attach is Linux-only")]
    fn the_exit_follows_the_last_output_to_every_end() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.attach_local(1, 100, 80, 24).expect("attach local");
        sessions
            .write(
                "acct",
                1,
                b"yes | head -c 600000; echo TAIL-$((40+2)); exit\n",
            )
            .expect("write");
        let mut remote = Vec::new();
        let mut local = Vec::new();
        let (mut remote_done, mut local_done) = (false, false);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while !(remote_done && local_done) && std::time::Instant::now() < deadline {
            // A small budget, as a busy data channel gives.
            for (_, kind, payload) in sessions.pump(1, 64 * 1024) {
                match kind {
                    FrameType::Stdout => remote.extend_from_slice(&payload),
                    FrameType::Exit => remote_done = true,
                    _ => {}
                }
            }
            if let Some(output) = sessions.pump_local(100, 64 * 1024) {
                local.extend_from_slice(&output.data);
                local_done |= output.exited;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert!(remote_done && local_done, "both ends saw the exit");
        for (end, seen) in [("remote", &remote), ("local", &local)] {
            assert!(
                seen.windows(7).any(|window| window == b"TAIL-42"),
                "{end} lost the tail: got {} bytes",
                seen.len()
            );
        }
        sessions.reap().finish();
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn limits_and_names_are_per_login() {
        let mut sessions = Sessions::default();
        for handle in 1..=MAX_SESSIONS as u32 {
            sessions
                .open("a", handle, 1, shell(), None, 80, 24)
                .expect("open");
        }
        assert_eq!(
            sessions.open("a", 99, 1, shell(), None, 80, 24),
            Err(SessionError::TooMany)
        );
        sessions.rename("a", 1, "prod").expect("rename");
        // Another login is not held back by the first one's shells, and its
        // names do not reveal the first one's.
        sessions
            .open("b", 1, 2, shell(), Some("prod".into()), 80, 24)
            .expect("a second login opens its own shell");
        sessions.kill_owned("a").finish();
        assert_eq!(sessions.count(), 1);
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn input_that_does_not_fit_is_refused_whole() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        // The shell is busy and reads nothing; the pty takes a little, the
        // queue the rest, then a frame that would overflow is refused.
        sessions.write("acct", 1, b"sleep 3\n").expect("write");
        std::thread::sleep(std::time::Duration::from_millis(200));
        let line = [b'#'; 1023];
        let mut chunk = Vec::new();
        for _ in 0..64 {
            chunk.extend_from_slice(&line);
            chunk.push(b'\n');
        }
        let mut refused = false;
        for _ in 0..64 {
            match sessions.write("acct", 1, &chunk) {
                Ok(()) => {}
                Err(SessionError::InputOverflow) => {
                    refused = true;
                    break;
                }
                Err(other) => panic!("unexpected {other:?}"),
            }
        }
        assert!(refused, "a 4 MiB paste into a busy shell is refused");
        // Nothing was cut out of the queue to make room: what is queued is
        // whole frames on top of what the pty took, and stays within limit.
        let queued = sessions.sessions[&key("acct", 1)].pending.len();
        assert!(queued <= PENDING_LIMIT, "queue stays bounded: {queued}");
        assert!(
            queued + chunk.len() > PENDING_LIMIT,
            "refused only when it would not fit"
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn the_sweep_takes_exited_and_abandoned_shells() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions
            .open("acct", 2, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.write("acct", 1, &line("exit")).expect("write");
        std::thread::sleep(std::time::Duration::from_millis(300));
        sessions.release(1);
        let (exited, abandoned) = sessions.sweep(std::time::Duration::from_secs(3600));
        assert_eq!((exited.sessions.len(), abandoned.sessions.len()), (1, 0));
        exited.finish();
        abandoned.finish();
        let (exited, abandoned) = sessions.sweep(std::time::Duration::ZERO);
        assert_eq!((exited.sessions.len(), abandoned.sessions.len()), (0, 1));
        exited.finish();
        abandoned.finish();
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "needs a pty, which only Linux has")]
    fn ending_a_login_ends_only_its_shells() {
        let table = shared();
        lock_table(&table)
            .open("ending-login", 1, 1, shell(), None, 80, 24)
            .expect("open");
        lock_table(&table)
            .open("staying-login", 1, 2, shell(), None, 80, 24)
            .expect("open");
        end_login("ending-login");
        assert!(lock_table(&table).list("ending-login").is_empty());
        assert_eq!(lock_table(&table).list("staying-login").len(), 1);
        lock_table(&table).kill_owned("staying-login").finish();
    }

    #[test]
    // The bulk producer below is a POSIX shell loop. The model itself is
    // platform-independent, so Linux coverage of this property is enough.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_reattach_replays_the_screen_not_the_output_log() {
        let mut sessions = Sessions::default();
        let (handle, _, _) = sessions
            .open("acct", 0, 1, "/bin/sh", None, 80, 24)
            .expect("open");
        // Far more output than the byte log we used to keep, then a marker. A
        // log cut at 128 KiB can land inside an escape sequence and leave the
        // client printing the tail of it; a screen cannot be cut at all.
        sessions
            .write(
                "acct",
                handle,
                &line("for i in $(seq 1 4000); do echo FILLER-FILLER-FILLER-$i; done"),
            )
            .expect("write");
        sessions
            .write("acct", handle, &line("echo THE-LAST-LINE"))
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(1, usize::MAX) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains("THE-LAST-LINE") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            String::from_utf8_lossy(&seen).contains("THE-LAST-LINE"),
            "the shell never finished"
        );
        let (again, _, replay) = sessions
            .open("acct", handle, 1, "/bin/sh", None, 80, 24)
            .expect("reattach");
        assert_eq!(again, handle, "reattach keeps the same shell");
        let text = String::from_utf8_lossy(&replay);
        assert!(
            text.contains("THE-LAST-LINE"),
            "the replay is not the current screen"
        );
        // The property the model gives us: the replay describes one screen, so
        // it does not grow with how much the shell has printed.
        assert!(
            replay.len() < 16 * 1024,
            "the replay grew with the output: {} bytes",
            replay.len()
        );
    }

    /// 本机侧收一段输出，直到出现 `needle`。
    fn local_output_until(sessions: &mut Sessions, consumer: u64, needle: &str) -> String {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            if let Some(output) = sessions.pump_local(consumer, PUMP_BUDGET) {
                seen.extend_from_slice(&output.data);
                if output.exited {
                    break;
                }
            }
            if String::from_utf8_lossy(&seen).contains(needle) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        String::from_utf8_lossy(&seen).into_owned()
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "local attach is Linux-only")]
    fn a_local_terminal_attaches_and_leaves_without_ending_the_shell() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions
            .write("acct", 1, &line("echo BEFORE-LOCAL"))
            .expect("write");
        output_until(&mut sessions, 1, "BEFORE-LOCAL");

        // 手机端先走，本机再接上：屏幕还在，接着用同一个 shell。
        sessions.detach("acct", 1).expect("detach");
        let (name, replay) = sessions.attach_local(1, 100, 90, 30).expect("attach local");
        assert_eq!(name, "shell-1");
        assert!(
            String::from_utf8_lossy(&replay).contains("BEFORE-LOCAL"),
            "the local attach replays the screen"
        );
        sessions
            .write_local(1, &line("echo FROM-LOCAL"))
            .expect("write local");
        let seen = local_output_until(&mut sessions, 100, "FROM-LOCAL");
        assert!(seen.contains("FROM-LOCAL"), "local input reaches the shell");

        // 关掉本地终端只断开这一端。
        sessions.detach_local(100);
        assert_eq!(sessions.count(), 1, "the shell outlives the local terminal");
        let (_, replay) = sessions.attach_local(1, 101, 90, 30).expect("reattach");
        assert!(
            String::from_utf8_lossy(&replay).contains("FROM-LOCAL"),
            "the screen survives a detach and reattach"
        );
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "local attach is Linux-only")]
    fn both_ends_see_the_same_shell() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        sessions.attach_local(1, 100, 80, 24).expect("attach local");
        sessions
            .write_local(1, &line("echo BOTH-ENDS"))
            .expect("write local");

        // 同一条输出同时进两个消费者的队列：谁都不会把它读走。
        let remote = output_until(&mut sessions, 1, "BOTH-ENDS");
        let local = local_output_until(&mut sessions, 100, "BOTH-ENDS");
        assert!(remote.contains("BOTH-ENDS"), "the controller sees it");
        assert!(local.contains("BOTH-ENDS"), "the local terminal sees it");
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "local attach is Linux-only")]
    fn the_smallest_attached_size_wins() {
        let mut sessions = Sessions::default();
        sessions
            .open("acct", 1, 1, shell(), None, 100, 40)
            .expect("open");
        sessions.attach_local(1, 100, 80, 30).expect("attach local");
        sessions
            .write_local(1, &line("stty size"))
            .expect("write local");
        let seen = local_output_until(&mut sessions, 100, "30 80");
        assert!(
            seen.contains("30 80"),
            "the pty takes the smallest of both ends: {seen:?}"
        );
    }

    #[test]
    // A POSIX `printf`; the model is platform-independent.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn a_resize_reaches_the_replayed_screen() {
        let mut sessions = Sessions::default();
        let (handle, _, _) = sessions
            .open("acct", 0, 1, "/bin/sh", None, 80, 24)
            .expect("open");
        // A phone rotating or raising its keyboard narrows the terminal.
        sessions.resize("acct", handle, 40, 24).expect("resize");
        let wide = format!("{}{}", "A".repeat(30), "B".repeat(31));
        // The command line splits the two halves, so only the output holds
        // the joined line.
        sessions
            .write(
                "acct",
                handle,
                &line(&format!(
                    "printf '%s%s\\n' {} {}",
                    "A".repeat(30),
                    "B".repeat(31)
                )),
            )
            .expect("write");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut seen = Vec::new();
        while std::time::Instant::now() < deadline {
            for (_, kind, payload) in sessions.pump(1, PUMP_BUDGET) {
                if kind == FrameType::Stdout {
                    seen.extend_from_slice(&payload);
                }
            }
            if String::from_utf8_lossy(&seen).contains(&wide) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            String::from_utf8_lossy(&seen).contains(&wide),
            "the shell never printed the line"
        );
        let (_, _, replay) = sessions
            .open("acct", handle, 1, "/bin/sh", None, 40, 24)
            .expect("reattach");
        // Draw the replay the way the controller would, at its width; the line
        // wraps onto a second row there, and none of it may be missing.
        let mut client = vt100::Parser::new(24, 40, 0);
        client.process(&replay);
        let drawn: String = client
            .screen()
            .contents()
            .chars()
            .filter(|c| !c.is_whitespace())
            .collect();
        assert!(drawn.contains(&wide), "the replay lost the line: {drawn:?}");
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn an_oversized_terminal_is_clamped() {
        let mut sessions = Sessions::default();
        // Unclamped, this model alone would ask for 128 GiB.
        let (handle, _, _) = sessions
            .open("acct", 0, 1, shell(), None, u16::MAX, u16::MAX)
            .expect("open");
        let size = |sessions: &Sessions| {
            sessions.sessions[&key("acct", handle)]
                .screen
                .screen()
                .size()
        };
        assert_eq!(size(&sessions), (MAX_ROWS, MAX_COLS));
        sessions.resize("acct", handle, 100, 40).expect("resize");
        assert_eq!(size(&sessions), (40, 100));
        sessions
            .resize("acct", handle, u16::MAX, 0)
            .expect("resize");
        assert_eq!(size(&sessions), (1, MAX_COLS));
        sessions
            .open("acct", handle, 1, shell(), None, 0, u16::MAX)
            .expect("reattach");
        assert_eq!(size(&sessions), (MAX_ROWS, 1));
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "local attach is Linux-only")]
    fn a_local_session_is_created_listed_and_kept() {
        let mut sessions = Sessions::default();
        let handle = sessions.create_local(Some("mine".into())).expect("create");
        assert!(
            handle >= LOCAL_HANDLE_BASE,
            "local handles stay out of the controller's small numbers"
        );
        let listed = sessions.list_local();
        assert!(
            listed
                .iter()
                .any(|session| session.handle == handle && session.name == "mine"),
            "the new session is listed"
        );
        // 主控获准接入本机会话时能看到它，编号就是本机区间里的那个。
        assert!(
            sessions
                .list(Owner::new("acct", true))
                .iter()
                .any(|session| session["session_id"] == handle),
            "a controller allowed in sees the local session"
        );
        // 用户自己开的会话不参与 30 分钟空闲清扫，本机接上再断开后也一样。
        let (exited, abandoned) = sessions.sweep(std::time::Duration::ZERO);
        assert!(exited.sessions.is_empty());
        assert!(abandoned.sessions.is_empty());
        sessions
            .attach_local(handle, 100, 80, 24)
            .expect("attach local");
        sessions.detach_local(100);
        let (exited, abandoned) = sessions.sweep(std::time::Duration::ZERO);
        assert!(exited.sessions.is_empty());
        assert!(abandoned.sessions.is_empty(), "kept after a local detach");
        assert_eq!(
            sessions.create_local(Some("mine".into())).err(),
            Some(SessionError::DuplicateName)
        );
        // Once its shell exits, the sweep takes it out of the list.
        sessions.write_local(handle, &line("exit")).expect("exit");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let (exited, abandoned) = sessions.sweep(std::time::Duration::ZERO);
            assert!(abandoned.sessions.is_empty());
            let taken = exited.sessions.len();
            exited.finish();
            if taken == 1 {
                break;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "the exited shell stayed"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(sessions.is_empty());
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn the_controller_can_take_over_a_local_session() {
        let mut sessions = Sessions::default();
        let handle = sessions
            .create_local(Some("shared".into()))
            .expect("create local");
        // 获准接入的主控用同一个编号 open，接上的是本机开的那个 shell，不是新的。
        let allowed = Owner::new("acct", true);
        let (again, name, _) = sessions
            .open(allowed, handle, 1, shell(), None, 80, 24)
            .expect("open");
        assert_eq!(again, handle);
        assert_eq!(name, "shared");
        assert_eq!(sessions.count(), 1, "no second shell was started");

        sessions
            .write(allowed, handle, &line("echo MOBILE-TO-LOCAL"))
            .expect("write");
        let seen = output_until(&mut sessions, 1, "MOBILE-TO-LOCAL");
        assert!(
            seen.contains("MOBILE-TO-LOCAL"),
            "controller input reaches the local shell"
        );

        // 本机接上同一会话时，看到的是同一份输出。
        sessions
            .attach_local(handle, 100, 80, 24)
            .expect("attach local");
        let local = local_output_until(&mut sessions, 100, "MOBILE-TO-LOCAL");
        assert!(local.contains("MOBILE-TO-LOCAL"));
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn local_sessions_do_not_exist_for_a_controller_not_allowed_in() {
        let mut sessions = Sessions::default();
        let handle = sessions
            .create_local(Some("private".into()))
            .expect("create local");
        assert!(sessions.list("acct").is_empty(), "not listed");
        assert_eq!(
            sessions.write("acct", handle, b"x"),
            Err(SessionError::Gone)
        );
        assert_eq!(
            sessions.resize("acct", handle, 100, 40),
            Err(SessionError::Gone)
        );
        assert_eq!(
            sessions.rename("acct", handle, "taken"),
            Err(SessionError::Gone)
        );
        assert_eq!(sessions.detach("acct", handle), Err(SessionError::Gone));
        assert_eq!(
            sessions.kill("acct", handle).err(),
            Some(SessionError::Gone)
        );
        // Opening its handle starts the controller's own shell, with nothing
        // of the local one to replay.
        let (again, name, replay) = sessions
            .open("acct", handle, 1, shell(), None, 80, 24)
            .expect("open");
        assert_eq!(again, handle);
        assert_ne!(name, "private");
        assert!(replay.is_empty());
        assert_eq!(sessions.count(), 2, "a second shell, not the local one");
        assert!(!sessions.on_local(1));
        sessions.kill_all().finish();
    }

    #[test]
    #[cfg_attr(
        not(any(target_os = "linux", target_os = "macos", windows)),
        ignore = "no terminal backend on this platform"
    )]
    fn revoking_local_access_lets_go_of_the_local_shell_only() {
        let mut sessions = Sessions::default();
        let local = sessions
            .create_local(Some("mine".into()))
            .expect("create local");
        sessions
            .open(Owner::new("acct", true), local, 7, shell(), None, 80, 24)
            .expect("attach to the local shell");
        sessions
            .open("acct", 1, 7, shell(), None, 80, 24)
            .expect("own shell");
        assert!(sessions.on_local(7));
        assert_eq!(sessions.release_local(7), vec![local]);
        assert!(!sessions.on_local(7));
        assert_eq!(sessions.release_local(7), Vec::<u32>::new(), "only once");
        assert_eq!(sessions.count(), 2, "the local shell keeps running");
        // The controller's own shell is still attached.
        assert!(
            sessions.sessions[&key("acct", 1)]
                .output
                .contains_key(&Consumer::Remote(7))
        );
        sessions.kill_all().finish();
    }

    #[test]
    fn geometry_accepts_json_and_binary() {
        assert_eq!(
            geometry(br#"{"cols":100,"rows":40}"#),
            (Some(100), Some(40))
        );
        assert_eq!(geometry(&[50, 0, 20, 0]), (Some(50), Some(20)));
        assert_eq!(geometry(b""), (None, None));
    }

    #[test]
    fn a_signal_frame_becomes_a_keystroke() {
        assert_eq!(signal_keystroke(br#"{"sig":"SIGINT"}"#), Some(0x03));
        assert_eq!(signal_keystroke(br#"{"sig":"SIGQUIT"}"#), Some(0x1c));
        assert_eq!(signal_keystroke(br#"{"sig":"SIGTSTP"}"#), Some(0x1a));
        assert_eq!(signal_keystroke(br#"{"sig":"SIGUSR1"}"#), None);
        assert_eq!(signal_keystroke(b""), None);
    }

    #[test]
    #[cfg_attr(not(target_os = "linux"), ignore = "local attach is Linux-only")]
    fn the_listing_moves_only_with_what_the_controller_sees() {
        let mut sessions = Sessions::default();
        let empty = sessions.listing("acct");
        sessions
            .open("acct", 1, 1, shell(), None, 80, 24)
            .expect("open");
        let opened = sessions.listing("acct");
        assert_ne!(opened, empty, "a new shell changes the list");
        // Typing moves the activity time, which the list page does not need.
        sessions.write("acct", 1, b"\n").expect("write");
        assert_eq!(sessions.listing("acct"), opened, "input is not a change");
        sessions.rename("acct", 1, "renamed").expect("rename");
        let renamed = sessions.listing("acct");
        assert_ne!(renamed, opened, "renaming changes the list");
        // Another login's shells and hidden local shells do not show through.
        sessions
            .open("other", 1, 2, shell(), None, 80, 24)
            .expect("other login");
        sessions.create_local(None).expect("local");
        assert_eq!(sessions.listing("acct"), renamed, "nothing this login sees");
        // The local-session grant changes the list without touching the table.
        assert_ne!(
            sessions.listing(Owner::new("acct", true)),
            renamed,
            "the grant shows the local shell"
        );
        sessions.kill("acct", 1).expect("kill").finish();
        assert_ne!(
            sessions.listing("acct"),
            renamed,
            "ending a shell changes it"
        );
        sessions.kill_all().finish();
    }

    #[test]
    fn a_session_change_is_the_control_message_field() {
        // PbControlMessage's oneof field 28 (wire type 2), an empty message.
        assert_eq!(
            crate::features::stream_control::publisher::terminal_session_changed(),
            [0xE2, 0x01, 0x00]
        );
    }
}
