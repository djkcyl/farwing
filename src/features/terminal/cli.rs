//! `OpenUUYC terminal --device ...` 的远端分支。
//!
//! 有 GUI 在跑时经本机控制通道复用它的终端连接；没有 GUI 时自己起一个
//! 私有服务。两种情况下本地窗口都只面对同样的 5 字节帧协议。

use super::protocol::SessionInfo;
use super::{relay, service, tty};
use crate::account::client::AuthenticatedClient;
use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const SIZE_TICK: Duration = Duration::from_millis(200);

pub(crate) fn run(
    session: Option<&str>,
    list: bool,
    new: bool,
    device: Option<String>,
    device_id: Option<String>,
    via_gui: bool,
) -> Result<()> {
    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(run_async(session, list, new, device, device_id, via_gui))
}

async fn run_async(
    session: Option<&str>,
    list: bool,
    new: bool,
    device: Option<String>,
    device_id: Option<String>,
    via_gui: bool,
) -> Result<()> {
    // GUI 拉起窗口时只给了设备 ID；此时不需要再打开一份登录态，直接经
    // 控制中心的通道走，控制中心自己知道当前账号。
    let direct_id = device_id.as_deref().filter(|_| via_gui && device.is_none());
    let client = if direct_id.is_none() {
        Some(Arc::new(AuthenticatedClient::from_saved_session()?))
    } else {
        None
    };
    let target = match direct_id {
        Some(id) => {
            crate::account::api::validate_device_id(id)?;
            id.to_owned()
        }
        None => {
            resolve_id(
                client.as_deref().expect("没有直连设备时一定加载了登录态"),
                device.as_deref(),
                device_id.as_deref(),
            )
            .await?
        }
    };
    if list {
        let sessions = match relay::connect_gui().await {
            Some((port, token)) => relay_list(port, &token, &target)?,
            None if via_gui => bail!("OpenUUYC 控制中心没有运行，无法列出远端终端"),
            None => {
                direct_list(
                    Arc::clone(client.as_ref().expect("没有控制中心时才走直连")),
                    &target,
                )
                .await?
            }
        };
        print_sessions(&sessions);
        if let Some(client) = client {
            client.close().await;
        }
        return Ok(());
    }

    let (cols, rows) = tty::size();
    if !tty::is_terminal() {
        bail!("请在终端里运行，输入不是一个终端");
    }
    let (session_id, name) = if new {
        (None, session.map(str::to_owned))
    } else {
        (
            Some(
                session
                    .context("缺少会话编号")?
                    .parse::<u32>()
                    .context("会话编号应为数字")?,
            ),
            None,
        )
    };

    match relay::connect_gui().await {
        Some((port, token)) => {
            tokio::task::spawn_blocking(move || {
                attach_blocking(port, token, target, session_id, name, cols, rows)
            })
            .await??;
        }
        None if via_gui => bail!("OpenUUYC 控制中心没有运行，无法接入远端终端"),
        None => {
            let client = client.as_ref().context("OpenUUYC 控制中心没有运行")?;
            let handle = service::start_for(Arc::clone(client), &target).await?;
            let private = relay::start_private(handle).await?;
            let port = private.port;
            let token = private.token.clone();
            let result = tokio::task::spawn_blocking(move || {
                attach_blocking(port, token, target.clone(), session_id, name, cols, rows)
            })
            .await?;
            private.shutdown().await;
            service::shutdown_all().await;
            result?;
        }
    }
    if let Some(client) = client {
        client.close().await;
    }
    Ok(())
}

async fn resolve_id(
    client: &AuthenticatedClient,
    device: Option<&str>,
    device_id: Option<&str>,
) -> Result<String> {
    let device = super::client::resolve_device(client, device, device_id).await?;
    Ok(device.device_id)
}

async fn direct_list(client: Arc<AuthenticatedClient>, target: &str) -> Result<Vec<SessionInfo>> {
    let handle = service::start_for(client, target).await?;
    let sessions = handle.list().await;
    service::shutdown_all().await;
    sessions
}

fn print_sessions(sessions: &[SessionInfo]) {
    if sessions.is_empty() {
        println!("对端没有活跃终端会话。");
        return;
    }
    for session in sessions {
        println!(
            "{:>3}  {}  {}  {}",
            session.session_id,
            if session.name.is_empty() {
                "未命名"
            } else {
                &session.name
            },
            session.state_label(),
            session.shell,
        );
    }
}

fn request(port: u16, token: &str, mut value: Value) -> Result<TcpStream> {
    value["token"] = Value::String(token.to_owned());
    let mut stream = TcpStream::connect(("127.0.0.1", port)).context("连不上本机终端控制通道")?;
    stream.set_nodelay(true).ok();
    stream.set_read_timeout(Some(Duration::from_secs(60))).ok();
    let mut line = serde_json::to_vec(&value)?;
    line.push(b'\n');
    stream.write_all(&line)?;
    stream.flush()?;
    Ok(stream)
}

fn read_reply(reader: &mut impl BufRead) -> Result<Value> {
    let mut line = String::new();
    reader
        .read_line(&mut line)
        .context("读取终端控制回复失败")?;
    ensure!(!line.is_empty(), "终端控制通道提前关闭");
    let reply: Value = serde_json::from_str(line.trim()).context("终端控制回复不是 JSON")?;
    if !reply["ok"].as_bool().unwrap_or(false) {
        bail!(
            "{}",
            reply["message"].as_str().unwrap_or("终端控制请求失败")
        );
    }
    Ok(reply)
}

fn relay_list(port: u16, token: &str, target: &str) -> Result<Vec<SessionInfo>> {
    let stream = request(port, token, json!({"action": "list", "device_id": target}))?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let reply = read_reply(&mut reader)?;
    serde_json::from_value(reply["sessions"].clone()).context("终端会话列表格式无效")
}

fn attach_blocking(
    port: u16,
    token: String,
    target: String,
    session: Option<u32>,
    name: Option<String>,
    cols: u16,
    rows: u16,
) -> Result<()> {
    let mut stream = request(
        port,
        &token,
        json!({
            "action": "attach",
            "device_id": target,
            "session": session,
            "name": name,
            "cols": cols,
            "rows": rows,
        }),
    )?;
    let mut reader = BufReader::new(stream.try_clone()?);
    let reply = read_reply(&mut reader)?;
    let _handle = reply["handle"].as_u64().unwrap_or_default();
    reader.get_ref().set_read_timeout(None).ok();
    stream.set_read_timeout(None).ok();

    let _raw = tty::RawMode::enter()?;
    let stop = Arc::new(AtomicBool::new(false));
    let out_stop = Arc::clone(&stop);
    let out_thread = std::thread::spawn(move || {
        let mut stdout = std::io::stdout();
        while let Ok((kind, payload)) = read_frame(&mut reader) {
            match kind {
                relay::FRAME_OUTPUT => {
                    if stdout.write_all(&payload).is_err() || stdout.flush().is_err() {
                        break;
                    }
                }
                relay::FRAME_EXIT => break,
                _ => {}
            }
        }
        out_stop.store(true, Ordering::Relaxed);
    });

    let mut write_stream = stream.try_clone()?;
    let in_stop = Arc::clone(&stop);
    let _in_thread = std::thread::spawn(move || {
        let mut stdin = std::io::stdin();
        let mut buffer = [0u8; 4096];
        while !in_stop.load(Ordering::Relaxed) {
            match stdin.read(&mut buffer) {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    if write_frame(&mut write_stream, relay::FRAME_INPUT, &buffer[..read]).is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    let mut applied = (cols, rows);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(SIZE_TICK);
        let size = tty::size();
        if size != applied && size != (0, 0) {
            applied = size;
            let mut payload = [0u8; 4];
            payload[..2].copy_from_slice(&size.0.to_le_bytes());
            payload[2..].copy_from_slice(&size.1.to_le_bytes());
            if write_frame(&mut stream, relay::FRAME_RESIZE, &payload).is_err() {
                break;
            }
        }
    }
    let _ = stream.shutdown(std::net::Shutdown::Both);
    let _ = out_thread.join();
    Ok(())
}

fn write_frame(stream: &mut impl Write, kind: u8, payload: &[u8]) -> std::io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    stream.write_all(&header)?;
    stream.write_all(payload)?;
    stream.flush()
}

fn read_frame(reader: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header)?;
    let length = u32::from_le_bytes(header[1..].try_into().expect("四字节")) as usize;
    if length > 1024 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "终端通道帧过大",
        ));
    }
    let mut payload = vec![0u8; length];
    reader.read_exact(&mut payload)?;
    Ok((header[0], payload))
}
