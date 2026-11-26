//! 本机终端窗口到 GUI 终端服务的桥。
//!
//! 终端窗口是另一个进程（系统终端模拟器里的 `OpenUUYC terminal ...`），
//! 被控端又只接受一条会话，所以窗口不能自己再连一次对端。GUI 进程在
//! `127.0.0.1` 上开一个带随机令牌的通道，CLI 先在这里列出、开终端、接上；
//! GUI 没跑时 CLI 会在自己进程里起同样的私有通道，代码路径完全一致。

use super::service::{self, Attach, ClientInput, ServiceOutput};
use crate::account::client::AuthenticatedClient;
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::io::ErrorKind;
use std::path::PathBuf;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

const MAX_REQUEST: u64 = 64 * 1024;
const MAX_FRAME: u32 = 1024 * 1024;
pub(crate) const FRAME_OUTPUT: u8 = 1;
pub(crate) const FRAME_INPUT: u8 = 2;
pub(crate) const FRAME_RESIZE: u8 = 3;
pub(crate) const FRAME_EXIT: u8 = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Endpoint {
    pub(crate) port: u16,
    pub(crate) token: String,
}

struct Server {
    client: std::sync::Arc<AuthenticatedClient>,
    stop: CancellationToken,
    done: CancellationToken,
}

fn server() -> &'static Mutex<Option<Server>> {
    static SERVER: OnceLock<Mutex<Option<Server>>> = OnceLock::new();
    SERVER.get_or_init(Mutex::default)
}

fn lock<T>(value: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value.lock().unwrap_or_else(|error| error.into_inner())
}

/// Unix 上只用 `$XDG_RUNTIME_DIR`（按用户隔离、0700，和被控端的本机接入
/// 一样）；没有它就不开令牌通道，CLI 改为自己直连对端。不能退回 `/tmp`：
/// 别的本机用户可以抢先建好那个目录，读走令牌就能拿到对端的 shell。
/// Windows 的临时目录本来就在用户自己的配置目录下。
fn runtime_dir() -> Option<PathBuf> {
    #[cfg(unix)]
    {
        std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .filter(|path| path.is_absolute())
            .map(|path| path.join("openuuyc"))
    }
    #[cfg(not(unix))]
    {
        Some(std::env::temp_dir().join("openuuyc"))
    }
}

fn endpoint_path() -> Option<PathBuf> {
    runtime_dir().map(|directory| directory.join("terminal-controller.json"))
}

fn prepare_directory(path: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        use std::os::unix::fs::PermissionsExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(path)
            .context("创建终端控制通道目录失败")?;
        // 目录早已存在时 mode 不生效；改不了权限说明目录不归我们。
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .context("设置终端控制通道目录权限失败")?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(path)?;
    Ok(())
}

fn write_endpoint(endpoint: &Endpoint) -> Result<()> {
    use std::io::Write;
    let path = endpoint_path().context("没有可用的运行时目录")?;
    let directory = path.parent().context("运行时目录无效")?;
    prepare_directory(directory)?;
    let temporary = path.with_extension("json.tmp");
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    file.write_all(&serde_json::to_vec(endpoint)?)?;
    file.flush()?;
    #[cfg(windows)]
    let _ = std::fs::remove_file(&path);
    std::fs::rename(temporary, path)?;
    Ok(())
}

fn remove_endpoint(endpoint: &Endpoint) {
    let Some(path) = endpoint_path() else { return };
    let current = std::fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Endpoint>(&bytes).ok());
    if current
        .is_some_and(|current| current.port == endpoint.port && current.token == endpoint.token)
    {
        let _ = std::fs::remove_file(path);
    }
}

pub(crate) fn read_endpoint() -> Option<Endpoint> {
    let path = endpoint_path()?;
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// 有没有地方放令牌文件，也就是 GUI 能不能把终端窗口接进来。
pub(crate) fn available() -> bool {
    endpoint_path().is_some()
}

/// GUI 登录后调用；账号换了就换一条监听。同一账号监听失败后不再重试，
/// 免得每轮主循环都起一次、刷一条警告。
pub(crate) fn ensure(client: std::sync::Arc<AuthenticatedClient>) {
    if !available() {
        return;
    }
    let mut server = lock(server());
    if let Some(current) = server.as_ref()
        && std::sync::Arc::ptr_eq(&current.client, &client)
    {
        return;
    }
    if let Some(current) = server.take() {
        current.stop.cancel();
    }
    let stop = client.ended().child_token();
    let done = CancellationToken::new();
    let task_done = done.clone();
    let task_stop = stop.clone();
    let client_ref = std::sync::Arc::clone(&client);
    tokio::spawn(async move {
        let _done = task_done.drop_guard();
        if let Err(error) = serve(client, task_stop).await {
            tracing::warn!(error = %format!("{error:#}"), "terminal relay stopped");
        }
    });
    *server = Some(Server {
        client: client_ref,
        stop,
        done,
    });
}

pub(crate) async fn shutdown_all() {
    let server = lock(server()).take();
    if let Some(server) = server {
        server.stop.cancel();
        server.done.cancelled().await;
    }
}

async fn serve(client: std::sync::Arc<AuthenticatedClient>, stop: CancellationToken) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("监听本机终端控制通道失败")?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().to_string();
    let endpoint = Endpoint {
        port,
        token: token.clone(),
    };
    write_endpoint(&endpoint)?;
    let ended = client.ended();
    loop {
        tokio::select! {
            biased;
            _ = stop.cancelled() => break,
            _ = ended.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { continue; };
                let client = std::sync::Arc::clone(&client);
                let token = token.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(stream, Resolver::Gui(client), token).await {
                        tracing::debug!(error = %format!("{error:#}"), "terminal relay request ended");
                    }
                });
            }
        }
    }
    remove_endpoint(&endpoint);
    Ok(())
}

enum Resolver {
    Gui(std::sync::Arc<AuthenticatedClient>),
    Private(service::Handle),
}

impl Resolver {
    async fn handle(&self, device_id: &str) -> Result<service::Handle> {
        match self {
            Self::Gui(client) => service::start_for(std::sync::Arc::clone(client), device_id).await,
            Self::Private(handle) => {
                ensure!(handle.target == device_id, "这台设备的终端服务没有运行");
                Ok(handle.clone())
            }
        }
    }
}

async fn handle_connection(stream: TcpStream, resolver: Resolver, token: String) -> Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();
    {
        let mut limited = (&mut reader).take(MAX_REQUEST);
        limited
            .read_line(&mut line)
            .await
            .context("读取终端控制请求失败")?;
    }
    ensure!(line.ends_with('\n'), "终端控制请求过长或不完整");
    let request: serde_json::Value =
        serde_json::from_str(line.trim()).context("终端控制请求不是 JSON")?;
    if request.get("token").and_then(serde_json::Value::as_str) != Some(token.as_str()) {
        let _ = write_json(
            &mut write_half,
            &serde_json::json!({"ok": false, "message": "令牌无效"}),
        )
        .await;
        bail!("终端控制令牌无效");
    }
    let device_id = request
        .get("device_id")
        .and_then(serde_json::Value::as_str)
        .context("终端控制请求缺少设备")?;
    let handle = resolver.handle(device_id).await?;
    match request
        .get("action")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default()
    {
        "list" => {
            let sessions = handle.list().await?;
            write_json(
                &mut write_half,
                &serde_json::json!({"ok": true, "sessions": sessions}),
            )
            .await?;
        }
        "attach" => {
            let session = request
                .get("session")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u32::try_from(value).ok());
            let name = request
                .get("name")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            let cols = request
                .get("cols")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(80);
            let rows = request
                .get("rows")
                .and_then(serde_json::Value::as_u64)
                .and_then(|value| u16::try_from(value).ok())
                .unwrap_or(24);
            let attach = handle.attach(session, name, cols, rows)?;
            let Attach {
                ready,
                input,
                output,
            } = attach;
            let attached = tokio::time::timeout(Duration::from_secs(45), ready)
                .await
                .context("等待对端打开终端超时")?
                .context("终端服务已停止")?;
            let attached = match attached {
                Ok(attached) => attached,
                Err(message) => {
                    write_json(
                        &mut write_half,
                        &serde_json::json!({"ok": false, "message": message}),
                    )
                    .await?;
                    return Ok(());
                }
            };
            write_json(
                &mut write_half,
                &serde_json::json!({
                    "ok": true,
                    "handle": attached.handle,
                    "name": attached.name,
                    "shell": attached.shell,
                }),
            )
            .await?;
            bridge(reader, write_half, input, output).await?;
        }
        other => bail!("未知的终端控制操作：{other}"),
    }
    Ok(())
}

async fn write_json(
    writer: &mut (impl AsyncWrite + Unpin),
    value: &serde_json::Value,
) -> Result<()> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    writer.write_all(&bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn bridge(
    mut reader: BufReader<impl AsyncRead + Unpin + Send>,
    mut writer: impl AsyncWrite + Unpin + Send + 'static,
    input: mpsc::Sender<ClientInput>,
    mut output: mpsc::UnboundedReceiver<ServiceOutput>,
) -> Result<()> {
    let (done_tx, mut done_rx) = tokio::sync::oneshot::channel();
    let mut output_task = tokio::spawn(async move {
        let mut done_tx = Some(done_tx);
        while let Some(output) = output.recv().await {
            match output {
                ServiceOutput::Data(data) => write_frame(&mut writer, FRAME_OUTPUT, &data).await?,
                ServiceOutput::Error(message) => {
                    let line = format!("\r\n\x1b[31m{message}\x1b[0m\r\n");
                    write_frame(&mut writer, FRAME_OUTPUT, line.as_bytes()).await?;
                }
                ServiceOutput::Exit => {
                    let _ = write_frame(&mut writer, FRAME_EXIT, &[]).await;
                    if let Some(done) = done_tx.take() {
                        let _ = done.send(());
                    }
                    return Ok::<_, std::io::Error>(());
                }
            }
        }
        if let Some(done) = done_tx.take() {
            let _ = done.send(());
        }
        Ok(())
    });
    loop {
        tokio::select! {
            biased;
            _ = &mut done_rx => break,
            frame = read_frame(&mut reader) => {
                let Ok((kind, payload)) = frame else { break; };
                match kind {
                    FRAME_INPUT => {
                        if input.send(ClientInput::Data(payload)).await.is_err() {
                            break;
                        }
                    }
                    FRAME_RESIZE if payload.len() >= 4 => {
                        let cols = u16::from_le_bytes([payload[0], payload[1]]);
                        let rows = u16::from_le_bytes([payload[2], payload[3]]);
                        if input.send(ClientInput::Resize(cols, rows)).await.is_err() {
                            break;
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    output_task.abort();
    let _ = (&mut output_task).await;
    Ok(())
}

pub(crate) async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    kind: u8,
    payload: &[u8],
) -> std::io::Result<()> {
    let mut header = [0u8; 5];
    header[0] = kind;
    header[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    writer.write_all(&header).await?;
    writer.write_all(payload).await?;
    writer.flush().await
}

pub(crate) async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
) -> std::io::Result<(u8, Vec<u8>)> {
    let mut header = [0u8; 5];
    reader.read_exact(&mut header).await?;
    let length = u32::from_le_bytes(header[1..].try_into().expect("四字节"));
    if length > MAX_FRAME {
        return Err(std::io::Error::new(
            ErrorKind::InvalidData,
            "终端通道帧过大",
        ));
    }
    let mut payload = vec![0u8; length as usize];
    reader.read_exact(&mut payload).await?;
    Ok((header[0], payload))
}

/// CLI 自己起的私有通道；端口和令牌只在本进程里传递。
pub(crate) struct PrivateRelay {
    pub(crate) port: u16,
    pub(crate) token: String,
    stop: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl PrivateRelay {
    pub(crate) async fn shutdown(self) {
        self.stop.cancel();
        let _ = self.task.await;
    }
}

pub(crate) async fn start_private(handle: service::Handle) -> Result<PrivateRelay> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .await
        .context("监听本机终端控制通道失败")?;
    let port = listener.local_addr()?.port();
    let token = uuid::Uuid::new_v4().to_string();
    let stop = CancellationToken::new();
    let task_stop = stop.clone();
    let task_token = token.clone();
    let task = tokio::spawn(async move {
        loop {
            tokio::select! {
                biased;
                _ = task_stop.cancelled() => break,
                accepted = listener.accept() => {
                    let Ok((stream, _)) = accepted else { continue; };
                    let resolver = Resolver::Private(handle.clone());
                    let token = task_token.clone();
                    tokio::spawn(async move {
                        if let Err(error) = handle_connection(stream, resolver, token).await {
                            tracing::debug!(error = %format!("{error:#}"), "private terminal relay request ended");
                        }
                    });
                }
            }
        }
    });
    Ok(PrivateRelay {
        port,
        token,
        stop,
        task,
    })
}

/// 直接连 GUI 的通道；没有控制中心时返回 `None`，由调用方决定是否自己起。
pub(crate) async fn connect_gui() -> Option<(u16, String)> {
    let endpoint = read_endpoint()?;
    TcpStream::connect(("127.0.0.1", endpoint.port))
        .await
        .ok()?;
    Some((endpoint.port, endpoint.token))
}
