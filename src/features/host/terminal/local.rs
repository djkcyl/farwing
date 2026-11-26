//! 本机终端接上被控会话的通道。
//!
//! 会话跑在 GUI 进程里；侧边栏点「在本地打开」会启动一个系统终端模拟器，
//! 里面跑的是同一个二进制的 `terminal attach` 子命令。子命令经
//! `$XDG_RUNTIME_DIR/openuuyc/terminal.sock` 连回 GUI 进程，收发原始字节，
//! 于是本机终端与手机端看到的是同一个 shell：谁输入都进同一个 pty，输出
//! 两边都收得到，和 tmux attach 一样。
//!
//! 关闭本地终端只是让这条 socket 断开，会话本身留在表里等下一次 attach。
//!
//! ## 线协议
//!
//! 连接后先是一行 JSON 请求（`:list` / `:attach`），回复也是一行 JSON。
//! attach 成功后双方都发定长头的二进制帧：1 字节类型 + 4 字节小端长度 +
//! 负载。输出（1）与服务端到客户端，输入（2）与 resize（3）反向，退出（4）
//! 只在 shell 结束时报一次。
//!
//! socket 放在 0700 的 `$XDG_RUNTIME_DIR/openuuyc` 里，文件本身设 0600，
//! 连上来的对端还要是同一个用户（`SO_PEERCRED`）；这本来也就是「本机用户
//! 可以直接操作这些 shell」的意思。

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::Sessions;

/// 这个平台能不能把会话接到本机终端里。
pub(crate) const SUPPORTED: bool = true;
/// Never shown on Linux; the other platforms' stub says it.
pub(crate) const UNSUPPORTED: &str = "本机终端接入暂只支持 Linux";

/// 服务端一次最多发给本机的字节数，与远程 pump 的上限一致。
const PUMP_BUDGET: usize = 256 * 1024;
/// 一帧的上限：输入与输出都远小于它，超了就是坏数据。
const MAX_FRAME: usize = 1024 * 1024;
const FRAME_OUTPUT: u8 = 1;
const FRAME_INPUT: u8 = 2;
const FRAME_RESIZE: u8 = 3;
const FRAME_EXIT: u8 = 4;
/// pump 与尺寸轮询的间隔。
const TICK: Duration = Duration::from_millis(20);
const SIZE_TICK: Duration = Duration::from_millis(200);
/// 请求行的上限。真正的请求只有几十个字节；不带换行一直发的连接到这里就
/// 断开，不会让控制中心的内存跟着涨。
const MAX_REQUEST: u64 = 64 * 1024;
/// 等请求行的时间。连上不说话的连接到时就断开，不会一直占着一个线程。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// 同时服务的本机连接数。一个 attach 会一直连着，正常用不到这么多。
const MAX_CLIENTS: usize = 32;

fn socket_path() -> Option<PathBuf> {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .map(|base| base.join("openuuyc/terminal.sock"))
}

/// 起监听线程。没有 `XDG_RUNTIME_DIR` 时本机 attach 不可用，其余功能照常。
pub(crate) fn serve(table: Arc<Mutex<Sessions>>) {
    let Some(path) = socket_path() else {
        tracing::debug!("XDG_RUNTIME_DIR 未设置，本机终端接入不可用");
        return;
    };
    if let Err(error) = listen_at(&path, table) {
        tracing::warn!(error = %format!("{error:#}"), "本机终端接入不可用");
    }
}

/// 在 `path` 上监听，照 `ui/control_linux.rs` 的做法：目录 0700，只清理
/// 没人监听的残留 socket；路径上是别的东西或者已经有实例在听，就报错不动。
fn listen_at(path: &Path, table: Arc<Mutex<Sessions>>) -> Result<()> {
    if let Some(directory) = path.parent() {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
            .context("创建终端通道目录失败")?;
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700))
            .context("设置终端通道目录权限失败")?;
    }
    clear_stale(path)?;
    let listener = UnixListener::bind(path)
        .with_context(|| format!("建立终端通道失败：{}", path.display()))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .context("设置终端通道权限失败")?;
    std::thread::Builder::new()
        .name("terminal attach".into())
        .spawn(move || accept(&listener, &table))
        .context("启动终端通道线程失败")?;
    Ok(())
}

/// 上一个进程没来得及清掉的 socket 会挡住绑定。只有连上去得到
/// `ECONNREFUSED` 的 socket 才算残留；别的文件或者有人在听都原样留着。
fn clear_stale(path: &Path) -> Result<()> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(error).with_context(|| format!("读取终端通道 {} 失败", path.display()));
        }
    };
    if !metadata.file_type().is_socket() {
        bail!("终端通道位置 {} 被其他文件占用", path.display());
    }
    match UnixStream::connect(path) {
        Ok(_) => bail!("已经有实例占着终端通道 {}", path.display()),
        Err(error) if error.kind() == ErrorKind::ConnectionRefused => std::fs::remove_file(path)
            .with_context(|| format!("清理残留的终端通道 {} 失败", path.display())),
        Err(error) if error.kind() == ErrorKind::NotFound => Ok(()),
        Err(error) => {
            Err(error).with_context(|| format!("无法确认终端通道 {} 是否残留", path.display()))
        }
    }
}

fn accept(listener: &UnixListener, table: &Arc<Mutex<Sessions>>) {
    let clients = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else {
            continue;
        };
        if let Err(error) = same_user(&stream) {
            tracing::warn!(%error, "拒绝了别的用户的终端通道连接");
            continue;
        }
        if clients.fetch_add(1, Ordering::AcqRel) >= MAX_CLIENTS {
            clients.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!("本机终端连接过多，拒绝新的连接");
            continue;
        }
        // 一个 attach 会连着到会话结束，不能占住 accept：每个连接自己一个
        // 线程。
        let table = Arc::clone(table);
        let done = Arc::clone(&clients);
        let spawned = std::thread::Builder::new()
            .name("terminal attach client".into())
            .spawn(move || {
                if let Err(error) = handle_client(stream, table) {
                    tracing::debug!(error = %format!("{error:#}"), "本机终端通道的请求结束");
                }
                done.fetch_sub(1, Ordering::AcqRel);
            });
        if let Err(error) = spawned {
            clients.fetch_sub(1, Ordering::AcqRel);
            tracing::warn!(%error, "无法处理本机终端连接");
        }
    }
}

/// 对端必须是同一个用户。目录与 socket 的权限已经挡住了别人，这里再核对
/// 一次，免得 `XDG_RUNTIME_DIR` 被指到共享目录时门户大开。
fn same_user(stream: &UnixStream) -> Result<()> {
    let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
    let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: `getsockopt` 只往我们给的 ucred 里写，长度如实传入。
    let read = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut credentials as *mut libc::ucred).cast(),
            &mut length,
        )
    };
    if read != 0 {
        return Err(std::io::Error::last_os_error()).context("读取终端通道对端身份失败");
    }
    // SAFETY: `geteuid` 没有参数也不会失败。
    let own = unsafe { libc::geteuid() };
    anyhow::ensure!(
        credentials.uid == own,
        "对端用户 {} 不是本用户 {own}",
        credentials.uid
    );
    Ok(())
}

fn handle_client(mut stream: UnixStream, table: Arc<Mutex<Sessions>>) -> Result<()> {
    stream.set_read_timeout(Some(REQUEST_TIMEOUT))?;
    let mut reader = BufReader::new(stream.try_clone().context("复制终端通道失败")?);
    let mut line = String::new();
    (&mut reader)
        .take(MAX_REQUEST)
        .read_line(&mut line)
        .context("读取终端通道请求失败")?;
    anyhow::ensure!(line.ends_with('\n'), "终端通道请求过长或不完整");
    // 请求之后是 attach 的交互，用户可以很久不敲键盘。
    stream.set_read_timeout(None)?;
    let request: Value = serde_json::from_str(line.trim()).context("终端通道请求不是 JSON")?;
    match request.get("action").and_then(Value::as_str) {
        Some("list") => {
            let listed: Vec<_> = super::lock_table(&table)
                .list_local()
                .into_iter()
                .map(|session| {
                    json!({
                        "handle": session.handle,
                        "name": session.name,
                        "shell": session.shell,
                        "state": session.state,
                        "created_at_ms": session.created_ms,
                        "last_active_ms": session.last_active_ms,
                        "remote": session.remote,
                        "local": session.local,
                        "foreground": session.foreground.map(|foreground| {
                            json!({"name": foreground.name, "command": foreground.command})
                        }),
                    })
                })
                .collect();
            let mut answer = serde_json::to_vec(&Value::Array(listed))?;
            answer.push(b'\n');
            stream.write_all(&answer)?;
            Ok(())
        }
        Some("new") => {
            let name = request
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let created = match super::local_unavailable() {
                Some(reason) => Err(super::SessionError::Unavailable(reason)),
                None => super::lock_table(&table).create_local(name),
            };
            let handle = match created {
                Ok(handle) => handle,
                Err(error) => {
                    let mut answer =
                        serde_json::to_vec(&json!({"ok": false, "message": error.to_string()}))?;
                    answer.push(b'\n');
                    stream.write_all(&answer)?;
                    return Ok(());
                }
            };
            // 新建完直接接入；客户端看到的是同一个回复形状。
            let mut attach_request = request.clone();
            attach_request["action"] = Value::from("attach");
            attach_request["session"] = Value::from(handle);
            attach(stream, reader, &table, &attach_request)
        }
        Some("attach") => attach(stream, reader, &table, &request),
        other => bail!("未知的终端通道请求 {other:?}"),
    }
}

fn attach(
    mut stream: UnixStream,
    mut reader: BufReader<UnixStream>,
    table: &Arc<Mutex<Sessions>>,
    request: &Value,
) -> Result<()> {
    let handle = match request.get("session") {
        Some(Value::Number(number)) => number
            .as_u64()
            .and_then(|value| u32::try_from(value).ok())
            .context("会话编号无效")?,
        Some(Value::String(name)) => {
            let name = name.trim();
            super::lock_table(table)
                .list_local()
                .into_iter()
                .find(|session| session.name == name)
                .map(|session| session.handle)
                .with_context(|| format!("找不到名为 {name} 的会话"))?
        }
        _ => bail!("请求缺少会话"),
    };
    let cols = request
        .get("cols")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(80);
    let rows = request
        .get("rows")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .unwrap_or(24);
    let consumer = super::next_local_consumer();
    let (name, replay) = match super::lock_table(table).attach_local(handle, consumer, cols, rows) {
        Ok(attached) => attached,
        Err(error) => {
            let mut answer =
                serde_json::to_vec(&json!({"ok": false, "message": error.to_string()}))?;
            answer.push(b'\n');
            stream.write_all(&answer)?;
            return Ok(());
        }
    };
    let mut answer = serde_json::to_vec(&json!({"ok": true, "handle": handle, "name": name}))?;
    answer.push(b'\n');
    stream.write_all(&answer)?;
    if !replay.is_empty() {
        write_frame(&mut stream, FRAME_OUTPUT, &replay)?;
    }

    // 客户端不读时不能无限期占住：写超时就当它走了，断开这一端。否则它的
    // 队列会涨到高水位，把整个会话的 pty 读取一起停住。
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    // 读线程处理输入与 resize；主线程按 tick 取输出。EOF 或任一侧写失败时
    // shutdown 这把 socket，另一头会跟着醒。
    let stop = Arc::new(AtomicBool::new(false));
    let read_stop = stop.clone();
    let read_table = Arc::clone(table);
    // 请求行后面可能已经跟着输入帧，读线程必须接着用同一个 BufReader，
    // 不能重新包一层，否则缓冲里的字节会丢。
    let reader_thread = std::thread::Builder::new()
        .name("terminal attach read".into())
        .spawn(move || {
            while let Ok((kind, payload)) = read_frame(&mut reader) {
                let mut table = super::lock_table(&read_table);
                match kind {
                    FRAME_INPUT => {
                        let _ = table.write_local(handle, &payload);
                    }
                    FRAME_RESIZE if payload.len() >= 4 => {
                        let cols = u16::from_le_bytes([payload[0], payload[1]]);
                        let rows = u16::from_le_bytes([payload[2], payload[3]]);
                        let _ = table.resize_local(handle, cols, rows);
                    }
                    _ => {}
                }
            }
            read_stop.store(true, Ordering::Relaxed);
        })?;

    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(TICK);
        let Some(output) = super::lock_table(table).pump_local(consumer, PUMP_BUDGET) else {
            break;
        };
        if !output.data.is_empty() && write_frame(&mut stream, FRAME_OUTPUT, &output.data).is_err()
        {
            break;
        }
        if output.exited {
            let _ = write_frame(&mut stream, FRAME_EXIT, b"");
            break;
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = reader_thread.join();
    super::lock_table(table).detach_local(consumer);
    Ok(())
}

fn write_frame(stream: &mut UnixStream, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)
}

fn read_frame(reader: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header[1..].try_into().expect("四字节")) as usize;
    if length > MAX_FRAME {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "终端通道帧过大",
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok((header[0], payload))
}

/// 在本机终端里列出会话、接上一个，或新建一个再接上。
pub(crate) fn run_client(session: Option<&str>, list: bool, new: bool) -> Result<()> {
    let path = socket_path().context("XDG_RUNTIME_DIR 未设置，无法接入本机终端")?;
    if new {
        // 新建时位置参数是名字，可以省略。
        return attach_client(&path, session, true);
    }
    match session.filter(|_| !list) {
        Some(session) => attach_client(&path, Some(session), false),
        None => list_sessions(&path),
    }
}

/// 连上控制中心；`allow_start` 为真时，它没在跑就先把它拉起来。
///
/// 会话必须有个宿主进程，命令行自己不能当宿主（否则关掉终端会话就没了）。
/// 所以 `--new` 与 attach 在连不上时会以 `--background` 启动控制中心，等
/// socket 就绪；列出会话不这么做，免得只敲一下 `terminal` 就冒出个托盘图标。
fn connect(path: &std::path::Path, allow_start: bool) -> Result<UnixStream> {
    const START_TIMEOUT: Duration = Duration::from_secs(10);
    match UnixStream::connect(path) {
        Ok(stream) => Ok(stream),
        Err(error)
            if allow_start
                && matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                ) =>
        {
            start_host()?;
            let deadline = std::time::Instant::now() + START_TIMEOUT;
            let mut last = error;
            while std::time::Instant::now() < deadline {
                match UnixStream::connect(path) {
                    Ok(stream) => return Ok(stream),
                    Err(error) => {
                        last = error;
                        std::thread::sleep(Duration::from_millis(200));
                    }
                }
            }
            Err(last).with_context(|| format!("控制中心启动了，但 {} 一直没有就绪", path.display()))
        }
        Err(error) => Err(error).with_context(|| {
            format!(
                "连不上 {}：控制中心没有运行（`OpenUUYC terminal --new` 会自动拉起它）",
                path.display()
            )
        }),
    }
}

/// 以不建窗口的方式启动控制中心。已经有实例在跑时，新进程会激活它并退出。
fn start_host() -> Result<()> {
    let exe = std::env::current_exe().context("找不到当前程序路径")?;
    std::process::Command::new(exe)
        .args(["gui", "--background"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("启动控制中心失败")?;
    Ok(())
}

fn list_sessions(path: &std::path::Path) -> Result<()> {
    let mut stream = connect(path, false)?;
    stream.write_all(b"{\"action\":\"list\"}\n")?;
    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let sessions: Vec<Value> = serde_json::from_str(line.trim()).context("会话列表不是 JSON")?;
    if sessions.is_empty() {
        println!("当前没有终端会话。");
        return Ok(());
    }
    for session in sessions {
        let handle = session["handle"].as_u64().unwrap_or_default();
        let name = session["name"].as_str().unwrap_or_default();
        let shell = session["shell"].as_str().unwrap_or_default();
        let state = session["state"].as_str().unwrap_or_default();
        let mut attached = Vec::new();
        if session["remote"].as_bool().unwrap_or(false) {
            attached.push("主控");
        }
        if session["local"].as_bool().unwrap_or(false) {
            attached.push("本机");
        }
        let attached = if attached.is_empty() {
            "无".to_owned()
        } else {
            attached.join("、")
        };
        let running = session["foreground"]["name"]
            .as_str()
            .map(|command| format!("  {command}"))
            .unwrap_or_default();
        println!("{handle:>3}  {name}  {state}  {shell}{running}  已附着：{attached}");
    }
    Ok(())
}

fn attach_client(path: &std::path::Path, session: Option<&str>, new: bool) -> Result<()> {
    use std::os::fd::AsRawFd;
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    if !is_terminal(stdin.as_raw_fd()) {
        bail!("请在终端里运行，输入不是一个终端");
    }
    let (cols, rows) = terminal_size(stdout.as_raw_fd());
    let mut stream = connect(path, true)?;
    // 名字里不会有引号之外的麻烦：会话名在服务端已经过滤掉控制字符。
    let request = if new {
        json!({
            "action": "new",
            "name": session,
            "cols": cols,
            "rows": rows,
        })
    } else {
        let session = session.context("缺少会话")?;
        json!({
            "action": "attach",
            "session": session.parse::<u32>().map(Value::from).unwrap_or_else(|_| Value::from(session)),
            "cols": cols,
            "rows": rows,
        })
    };
    let mut line = serde_json::to_vec(&request)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    let reader_stream = stream.try_clone()?;
    let mut reader = BufReader::new(reader_stream);
    let mut answer = String::new();
    reader.read_line(&mut answer)?;
    let reply: Value = serde_json::from_str(answer.trim()).context("接入回复不是 JSON")?;
    if !reply["ok"].as_bool().unwrap_or(false) {
        bail!("{}", reply["message"].as_str().unwrap_or("接入失败"));
    }

    let _raw = RawMode::enter(stdin.as_raw_fd())?;
    let stop = Arc::new(AtomicBool::new(false));

    // socket -> 本机屏幕
    let out_stop = stop.clone();
    let out_thread = std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        while let Ok((kind, payload)) = read_frame(&mut reader) {
            match kind {
                FRAME_OUTPUT => {
                    if stdout.write_all(&payload).is_err() || stdout.flush().is_err() {
                        break;
                    }
                }
                FRAME_EXIT => break,
                _ => {}
            }
        }
        out_stop.store(true, Ordering::Relaxed);
    });

    // 本机键盘 -> socket。raw 模式下这里拿到的是原始字节。
    let mut write_stream = stream.try_clone()?;
    let in_stop = stop.clone();
    let _in_thread = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 4096];
        while !in_stop.load(Ordering::Relaxed) {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if write_frame(&mut write_stream, FRAME_INPUT, &buffer[..read]).is_err() {
                        break;
                    }
                }
            }
        }
    });

    // 窗口大小变化：raw 模式下没有 SIGWINCH 处理，轮询就够了。
    let mut applied = (cols, rows);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(SIZE_TICK);
        let size = terminal_size(1);
        if size != applied && size != (0, 0) {
            applied = size;
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&size.0.to_le_bytes());
            payload[2..].copy_from_slice(&size.1.to_le_bytes());
            if write_frame(&mut stream, FRAME_RESIZE, &payload).is_err() {
                break;
            }
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = out_thread.join();
    Ok(())
}

fn is_terminal(fd: i32) -> bool {
    // SAFETY: `isatty` 只读一个 fd。
    unsafe { libc::isatty(fd) == 1 }
}

fn terminal_size(fd: i32) -> (u16, u16) {
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    // SAFETY: `fd` 由调用方保证是打开的终端，`size` 是有效的 winsize。
    let filled = unsafe { libc::ioctl(fd, libc::TIOCGWINSZ as libc::c_ulong, &mut size) };
    if filled != 0 || size.ws_col == 0 || size.ws_row == 0 {
        return (80, 24);
    }
    (size.ws_col, size.ws_row)
}

/// raw 模式的作用域守卫：无论怎么退出都把终端还给 shell。
struct RawMode {
    fd: i32,
    original: libc::termios,
}

impl RawMode {
    fn enter(fd: i32) -> Result<Self> {
        // SAFETY: `tcgetattr` 只写我们提供的 termios。
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            bail!("无法读取终端设置");
        }
        let mut raw = original;
        // SAFETY: `cfmakeraw` 只改结构体字段。
        unsafe { libc::cfmakeraw(&mut raw) };
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            bail!("无法切换终端到原始模式");
        }
        Ok(Self { fd, original })
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: fd 在作用域内一直有效，original 来自 tcgetattr。
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

/// 在系统终端模拟器里接上会话。
pub(crate) fn open_in_terminal(session: u32) -> Result<()> {
    let exe = std::env::current_exe().context("找不到当前程序路径")?;
    let (program, args) = terminal_emulator().context("找不到可用的终端模拟器")?;
    let mut command = std::process::Command::new(&program);
    command.args(args);
    command.args(attach_arguments(&exe, session));
    let mut child = command
        .spawn()
        .with_context(|| format!("启动 {program} 失败"))?;
    // Reap it when it exits. Many emulators hand the window to a server and
    // exit at once (gnome-terminal, `kitty --single-instance`); left alone,
    // each 「在本地打开」 would leave a zombie under the control center.
    std::thread::Builder::new()
        .name("terminal emulator".into())
        .spawn(move || {
            let _ = child.wait();
        })
        .context("无法等待终端模拟器退出")?;
    Ok(())
}

/// 在系统终端里重新运行本程序本身，再接上会话。
///
/// 用当前可执行文件的路径而不是写死 `OpenUUYC`：便携运行、`cargo run` 和
/// 安装到别的目录时，命令名不一定就是 PATH 里的那一个。Linux 上
/// `current_exe` 读的就是 `/proc/self/exe`。
fn attach_arguments(exe: &std::path::Path, session: u32) -> Vec<std::ffi::OsString> {
    vec![
        exe.as_os_str().to_owned(),
        "terminal".into(),
        session.to_string().into(),
    ]
}

/// 找一个能运行命令的终端模拟器。`$TERMINAL` 优先，再试常见的几个。
fn terminal_emulator() -> Option<(String, Vec<String>)> {
    if let Some(setting) = std::env::var_os("TERMINAL").and_then(|value| value.into_string().ok()) {
        // `$TERMINAL` 是用户自己的选择，可能带着自己的参数（`foot -e`）。
        let mut words = setting.split_whitespace();
        if let Some(program) = words.next()
            && in_path(program)
        {
            return Some((program.to_owned(), words.map(str::to_owned).collect()));
        }
    }
    const CANDIDATES: &[(&str, &[&str])] = &[
        ("xdg-terminal-exec", &[]),
        ("foot", &["-e"]),
        ("alacritty", &["-e"]),
        ("kitty", &["-e"]),
        ("konsole", &["-e"]),
        ("ghostty", &["-e"]),
        ("wezterm", &["start", "--"]),
        ("gnome-terminal", &["--"]),
        ("xterm", &["-e"]),
    ];
    CANDIDATES
        .iter()
        .find(|(program, _)| in_path(program))
        .map(|(program, args)| {
            (
                (*program).to_owned(),
                args.iter().map(|arg| (*arg).to_owned()).collect(),
            )
        })
}

fn in_path(program: &str) -> bool {
    let path = std::path::Path::new(program);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|directory| {
                let candidate = directory.join(program);
                candidate.is_file()
                    && std::fs::metadata(&candidate)
                        .map(|meta| meta.permissions().mode() & 0o111 != 0)
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    /// 重新运行的是当前可执行文件，参数与 CLI 的形状一致。
    #[test]
    fn the_attach_command_reruns_this_binary() {
        let arguments = attach_arguments(std::path::Path::new("/opt/openuuyc/OpenUUYC"), 7);
        assert_eq!(
            arguments,
            vec![
                std::ffi::OsString::from("/opt/openuuyc/OpenUUYC"),
                std::ffi::OsString::from("terminal"),
                std::ffi::OsString::from("7"),
            ]
        );
    }

    /// A private socket under a fresh directory, served from its own table:
    /// tests never touch `$XDG_RUNTIME_DIR` or a running OpenUUYC's socket.
    fn private_socket(name: &str) -> (PathBuf, Arc<Mutex<Sessions>>) {
        let dir =
            std::env::temp_dir().join(format!("openuuyc-terminal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("openuuyc/terminal.sock");
        let table = Arc::new(Mutex::new(Sessions::default()));
        listen_at(&path, Arc::clone(&table)).expect("listen");
        (path, table)
    }

    fn request(path: &Path, line: &[u8]) -> Value {
        let mut stream = UnixStream::connect(path).expect("connect");
        stream.write_all(line).expect("request");
        let mut reader = BufReader::new(stream);
        let mut answer = String::new();
        reader.read_line(&mut answer).expect("answer");
        serde_json::from_str(answer.trim()).expect("json")
    }

    /// 走一遍真实的 socket：列出一个会话，再接上它，再新建一个。
    #[test]
    fn the_socket_lists_and_attaches_sessions() {
        let (path, table) = private_socket("attach");
        let directory = path.parent().expect("directory");
        assert_eq!(
            std::fs::metadata(directory)
                .expect("dir")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&path)
                .expect("socket")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let shell = Sessions::default_shell();
        let (handle, _, _) = super::super::lock_table(&table)
            .open("acct", 1, 99, &shell, Some("socket-test".into()), 80, 24)
            .expect("open");

        let listed = request(&path, b"{\"action\":\"list\"}\n");
        assert!(
            listed
                .as_array()
                .expect("array")
                .iter()
                .any(|session| session["name"] == "socket-test" && session["handle"] == handle),
            "the list carries the session: {listed:?}"
        );
        let reply = request(
            &path,
            b"{\"action\":\"attach\",\"session\":1,\"cols\":80,\"rows\":24}\n",
        );
        assert_eq!(reply["ok"], true, "attach is accepted: {reply:?}");
        assert_eq!(reply["name"], "socket-test");
        // 新建：不经过手机端，编号落在本机区间；以 root 运行时（CI 容器）
        // 得到的是页面上同一句拒绝。
        let reply = request(
            &path,
            b"{\"action\":\"new\",\"name\":\"made-by-cli\",\"cols\":80,\"rows\":24}\n",
        );
        match super::super::local_unavailable() {
            Some(reason) => {
                assert_eq!(reply["ok"], false);
                assert_eq!(reply["message"], reason);
            }
            None => {
                assert_eq!(reply["ok"], true, "new is accepted: {reply:?}");
                assert!(reply["handle"].as_u64().expect("handle") >= 1000);
            }
        }
        // 命令行入口走的是同一条路。
        list_sessions(&path).expect("the client lists");
        super::super::lock_table(&table).kill_all().finish();
        let _ = std::fs::remove_dir_all(directory.parent().expect("temp"));
    }

    /// Only a socket nobody listens on is cleared; a file, or a running
    /// instance's socket, is left where it is.
    #[test]
    fn only_a_stale_socket_is_replaced() {
        let dir = std::env::temp_dir().join(format!("openuuyc-stale-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("dir");
        let table = || Arc::new(Mutex::new(Sessions::default()));

        let file = dir.join("file.sock");
        std::fs::write(&file, b"not a socket").expect("file");
        assert!(listen_at(&file, table()).is_err());
        assert_eq!(std::fs::read(&file).expect("kept"), b"not a socket");

        let live = dir.join("live.sock");
        let _running = UnixListener::bind(&live).expect("running instance");
        assert!(listen_at(&live, table()).is_err(), "a live socket is kept");
        assert!(UnixStream::connect(&live).is_ok(), "and still answers");

        let stale = dir.join("stale.sock");
        drop(UnixListener::bind(&stale).expect("left behind"));
        // Other tests fork shells meanwhile, and a child holds the listener
        // until it execs; wait until nobody is listening, as for a socket a
        // dead process left behind.
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while UnixStream::connect(&stale).is_ok() {
            assert!(std::time::Instant::now() < deadline, "the socket closes");
            std::thread::sleep(Duration::from_millis(10));
        }
        listen_at(&stale, table()).expect("a stale socket is replaced");
        assert_eq!(
            request(&stale, b"{\"action\":\"list\"}\n"),
            Value::Array(Vec::new())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A request line that never ends is cut off at the limit instead of
    /// growing the host's memory.
    #[test]
    fn an_endless_request_is_cut_off() {
        let (path, _table) = private_socket("endless");
        let mut stream = UnixStream::connect(&path).expect("connect");
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .expect("timeout");
        let chunk = vec![b'x'; 16 * 1024];
        let mut sent = 0usize;
        let cut = loop {
            match stream.write_all(&chunk) {
                Ok(()) => sent += chunk.len(),
                Err(_) => break true,
            }
            if sent > 8 * MAX_REQUEST as usize {
                break false;
            }
        };
        assert!(cut, "the server kept reading {sent} bytes of one request");
        let _ = std::fs::remove_dir_all(path.parent().and_then(Path::parent).expect("temp"));
    }
}
