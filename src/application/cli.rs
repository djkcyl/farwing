//! Command-line operations that do not need the device center window:
//! server configuration, account login/logout and headless hosting.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};

use crate::account::client::AuthenticatedClient;
use crate::account::server::ServerConfig;
use crate::session::device_session::DeviceRuntime;
use crate::session::presence::{ActivePresence, PresenceEvent, PresenceState};

/// `server set`: save the Farwing Server of this profile.
pub fn server_set(address: &str, pin: Option<&str>, ca_file: Option<&std::path::Path>) -> Result<()> {
    let ca = ca_file
        .map(|path| std::fs::read_to_string(path).with_context(|| format!("读取 CA 文件 {}", path.display())))
        .transpose()?;
    let config = ServerConfig::new(address, pin, ca)?;
    config.save()?;
    println!("服务器已设置为 {}", config.origin());
    if let Some(pin) = config.pinned_fingerprint() {
        println!("已固定证书指纹 {pin}");
    }
    Ok(())
}

/// `server show`
pub fn server_show() -> Result<()> {
    match ServerConfig::load()? {
        Some(config) => {
            println!("服务器：{}", config.origin());
            println!(
                "证书：{}{}",
                config
                    .pinned_fingerprint()
                    .map(|pin| format!("固定指纹 {pin}"))
                    .unwrap_or_else(|| "系统信任".into()),
                if config.has_custom_ca() { "（含自定义 CA）" } else { "" }
            );
        }
        None => println!("尚未设置服务器"),
    }
    Ok(())
}

/// `server probe`: show the certificate fingerprint and server identity.
pub async fn server_probe(address: &str) -> Result<()> {
    let probe = crate::account::server::probe_certificate(address).await?;
    println!("证书 SHA-256：{}", probe.sha256);
    println!(
        "系统信任：{}",
        if probe.trusted_by_system { "是" } else { "否（如确认无误，可用 --pin 固定此指纹）" }
    );
    let config = ServerConfig::new(address, Some(&probe.sha256), None)?;
    let api = crate::account::api::ServerApi::new(&config)?;
    let info = api.server_info().await?;
    println!(
        "服务器：{} {}（协议 {} v{}..=v{}）",
        info.server.name, info.server.version, info.protocol.name, info.protocol.min, info.protocol.max
    );
    if !info.server.instance_name.is_empty() {
        println!("实例：{}", info.server.instance_name);
    }
    Ok(())
}

/// `logout`
pub async fn logout() -> Result<()> {
    let client = AuthenticatedClient::from_saved_session()?;
    let outcome = client.logout().await;
    if let Some(error) = outcome.local_error {
        anyhow::bail!("本地凭据删除失败：{error}");
    }
    match outcome.remote_error {
        Some(error) => println!("已清除本地登录；服务器退出未确认：{error}"),
        None => println!("已退出登录"),
    }
    Ok(())
}

/// `host`: keep this device online and controllable without a window until
/// Ctrl+C. Uses the saved login of the current profile.
pub async fn host(
    allow: Option<bool>,
    allow_ports: bool,
    assist_code: Option<String>,
    code_rotation: Option<&str>,
    encoder: Option<&str>,
    serverless: bool,
) -> Result<()> {
    use crate::features::host::EncoderMode;
    use crate::session::host_client::HostClient;
    let encoder = encoder
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "auto" => Ok(EncoderMode::Automatic),
            "hardware" => Ok(EncoderMode::Hardware),
            "software" => Ok(EncoderMode::Software),
            other => Err(anyhow::anyhow!("未知的编码方式：{other}（auto、hardware 或 software）")),
        })
        .transpose()?;
    // `--serverless`: no login and no server; only LAN and address direct
    // connections (trusted devices, access code).
    let runtime = if serverless { None } else { Some(DeviceRuntime::start()?) };
    let account = runtime
        .as_ref()
        .map(|runtime| AuthenticatedClient::from_saved_session_with_device(runtime.handle()).map(Arc::new))
        .transpose()?;
    let client = match &account {
        Some(account) => HostClient::from(Arc::clone(account)),
        None => HostClient::local()?,
    };
    let result = async {
        match &account {
            // While the server is unreachable the host still serves direct
            // connections; own devices are authorized from the cached list.
            Some(account) => match tokio::time::timeout(Duration::from_secs(15), account.list_devices()).await {
                Ok(Ok(devices)) => println!(
                    "已登录：{}；本机设备 {}（{}）",
                    account.account_name(),
                    devices.current_device.alias,
                    devices.current_device.device_id
                ),
                Ok(Err(error))
                    if !crate::account::api::failure_code(&error).is_some_and(|code| code.retryable()) =>
                {
                    return Err(error).context("恢复账号会话失败");
                }
                Ok(Err(error)) => println!("服务器暂不可用（{error:#}），先只接受直连"),
                Err(_) => println!("服务器暂不可用，先只接受直连"),
            },
            None => {
                let device = client.direct_device()?;
                println!(
                    "仅直连模式：本机 {}（设备 ID {}）",
                    device.name,
                    crate::account::api::device_id_text(&device.identity.device_id)
                );
            }
        }
        if let Some(allow) = allow {
            client.host.set_allowed(allow);
        }
        if allow_ports {
            client.host.set_port_mapping_allowed(true)?;
        }
        if let Some(code) = &assist_code {
            use crate::features::host::assist::{Mode, Passwords, Rotation, Settings};
            // A code given here stays unless a rotation is given too.
            client.host.set_assistance(Settings {
                enabled: true,
                mode: Mode::Password,
                code: code.clone(),
                code_made_at: chrono::Utc::now().timestamp(),
                rotation: code_rotation.map(Rotation::parse).transpose()?.unwrap_or(Rotation::Never),
                passwords: Passwords::Random,
                custom_code: String::new(),
                grants: None,
            })?;
        }
        if let Some(mode) = encoder {
            let mut settings = client.host.encoding_settings();
            settings.mode = mode;
            if mode == EncoderMode::Software {
                // The software encoder only makes H.264.
                settings.codec = crate::features::host::EncoderCodec::Automatic;
                settings.preferred_codec = Default::default();
            }
            client.host.set_encoding_settings(settings)?;
        }
        if allow.is_some() || allow_ports || assist_code.is_some() || encoder.is_some() {
            client.host.persist_settings().await;
        }
        println!("编码方式：{:?}", client.host.encoding_settings().mode);
        println!(
            "允许被控：{}",
            if client.host.requested() { "是" } else { "否（使用 --allow 开启）" }
        );
        println!(
            "允许端口转发：{}",
            if client.host.port_mapping_allowed() { "是" } else { "否（使用 --allow-ports 开启）" }
        );
        println!(
            "允许远程关机/重启：{}",
            if client.host.power_allowed() { "是" } else { "否（在设备中心连接设置中开启）" }
        );
        println!("正在准备本机采集与编码能力……");
        let mut probe = crate::session::startup::Probe::start(&client.host);
        let mut presence = None::<ActivePresence>;
        let mut assist_id = String::new();
        let mut announced = false;
        let outcome = loop {
            if let Some(Err(error)) = probe.ended().await {
                println!("警告：采集与编码能力预检失败：{error:#}");
            }
            if presence.is_none() && probe.online() {
                presence = Some(ActivePresence::start(client.clone()));
            }
            let assistance = client.host.assistance.snapshot();
            if assistance.connect_id != assist_id {
                assist_id = assistance.connect_id.clone();
                if !assist_id.is_empty() {
                    println!("短号：{assist_id}");
                }
            }
            if !announced && let Some(port) = crate::transport::direct::host::listening_port() {
                announced = true;
                let settings = crate::transport::nat::settings::current();
                println!(
                    "直连已开启：TCP {port}，直连会话共用 UDP {port}；局域网发现{}",
                    if settings.lan_discovery { "已开启" } else { "已关闭" }
                );
            }
            if let Some(presence) = &presence {
                while let Ok(event) = presence.events.try_recv() {
                    match event {
                        // Without a server there is no online state to tell.
                        PresenceEvent::State(_) if account.is_none() => {}
                        PresenceEvent::State(state) => println!(
                            "在线状态：{}",
                            match state {
                                PresenceState::Connecting => "正在连接",
                                PresenceState::Online => "在线",
                                PresenceState::Reconnecting => "正在重连",
                                PresenceState::Offline => "离线",
                            }
                        ),
                        PresenceEvent::Warning(message) => println!("警告：{message}"),
                        PresenceEvent::DeviceChanged(_) | PresenceEvent::DevicesResync => {}
                        PresenceEvent::AccountEnded => {
                            println!("账号会话已结束");
                        }
                    }
                }
                if presence.task.is_finished() {
                    break Ok(());
                }
            }
            tokio::select! {
                _ = tokio::signal::ctrl_c() => break Ok(()),
                _ = tokio::time::sleep(Duration::from_millis(200)) => {}
            }
        };
        probe.cancel().await;
        if let Some(presence) = presence {
            presence.close().await;
        }
        outcome
    }
    .await;
    client.close().await;
    if let Some(runtime) = runtime {
        runtime.close().await;
    }
    result
}

/// Load the saved login of this profile, run `body`, then release the
/// device runtime. Shared by the headless developer checks below.
async fn with_client<T, F, Fut>(body: F) -> Result<T>
where
    F: FnOnce(Arc<AuthenticatedClient>) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let runtime = DeviceRuntime::start()?;
    let client = Arc::new(AuthenticatedClient::from_saved_session_with_device(
        runtime.handle(),
    )?);
    let result = body(Arc::clone(&client)).await;
    client.close().await;
    runtime.close().await;
    result
}

async fn account_device(
    client: &AuthenticatedClient,
    device_id: &str,
) -> Result<crate::account::api::DeviceInfo> {
    let list = client.list_devices().await?;
    list.devices
        .into_iter()
        .chain(list.shared)
        .find(|device| device.device_id == device_id)
        .with_context(|| format!("设备 {device_id} 不在当前账号或好友共享中"))
}

pub enum FriendCommand {
    List,
    Add(String),
    Answer(String, bool),
    Cancel(String),
    Remove(String),
}

fn user_line(user: &farwing_protocol::api::UserSummary) -> String {
    if user.nickname.is_empty() || user.nickname == user.username {
        format!("{} [{}]", user.username, user.user_id)
    } else {
        format!("{}（{}） [{}]", user.nickname, user.username, user.user_id)
    }
}

/// `friend ...`: friends and friend requests of the logged-in account.
pub async fn friend(command: FriendCommand) -> Result<()> {
    with_client(|client| async move {
        match command {
            FriendCommand::List => {
                let (friends, requests) =
                    tokio::try_join!(client.friends(), client.friend_requests())?;
                println!("好友：");
                for friend in &friends.friends {
                    println!("  {}", user_line(&friend.user));
                }
                if friends.friends.is_empty() {
                    println!("  （无）");
                }
                if !requests.incoming.is_empty() {
                    println!("\n收到的申请：");
                    for request in &requests.incoming {
                        println!("  {} — {}", request.id, user_line(&request.user));
                    }
                }
                if !requests.outgoing.is_empty() {
                    println!("\n发出的申请：");
                    for request in &requests.outgoing {
                        println!("  {} — {}", request.id, user_line(&request.user));
                    }
                }
            }
            FriendCommand::Add(username) => {
                match client.send_friend_request(&username).await? {
                    farwing_protocol::api::FriendRequestOutcome::Sent { request } => {
                        println!("已发送好友申请 {}，等待对方接受", request.id)
                    }
                    farwing_protocol::api::FriendRequestOutcome::Accepted { friend } => {
                        println!("对方已向你申请，现已成为好友：{}", user_line(&friend.user))
                    }
                }
            }
            FriendCommand::Answer(id, accept) => {
                // The command line shares the new-friend default.
                let shares = accept.then(|| {
                    farwing_protocol::caps::normalize(farwing_protocol::caps::DEFAULT_FRIEND)
                });
                client.answer_friend_request(&id, shares).await?;
                println!("{}", if accept { "已接受好友申请" } else { "已拒绝好友申请" });
            }
            FriendCommand::Cancel(id) => {
                client.cancel_friend_request(&id).await?;
                println!("已撤回好友申请");
            }
            FriendCommand::Remove(user_id) => {
                client.remove_friend(&user_id).await?;
                println!("已解除好友关系");
            }
        }
        Ok(())
    })
    .await
}

fn caps_line(caps: &[String]) -> String {
    if caps.is_empty() {
        "（无）".into()
    } else {
        caps.join(",")
    }
}

fn parse_caps(text: &str) -> Result<Vec<String>> {
    let caps: Vec<String> = text
        .split(',')
        .map(str::trim)
        .filter(|cap| !cap.is_empty())
        .map(str::to_owned)
        .collect();
    if let Some(unknown) = caps.iter().find(|cap| !farwing_protocol::caps::is_known(cap)) {
        anyhow::bail!(
            "未知能力 `{unknown}`；可用：{}",
            farwing_protocol::caps::ALL.join(",")
        );
    }
    Ok(farwing_protocol::caps::normalize(&caps))
}

/// `grant show`: what this account grants a friend.
pub async fn grant_show(user_id: &str) -> Result<()> {
    with_client(|client| async move {
        let grants = client.grants(user_id).await?;
        println!("默认：{}", caps_line(&grants.default));
        for (device, caps) in &grants.devices {
            println!("设备 {device}：{}", caps_line(caps));
        }
        Ok(())
    })
    .await
}

/// `grant set`: replace what this account grants a friend.
pub async fn grant_set(
    user_id: &str,
    default: Option<String>,
    devices: Vec<String>,
    clear: Vec<String>,
) -> Result<()> {
    with_client(|client| async move {
        let mut grants = client.grants(user_id).await?;
        if let Some(default) = default {
            grants.default = parse_caps(&default)?;
        }
        for entry in devices {
            let (device, caps) = entry
                .split_once('=')
                .context("单独设置的格式为 设备ID=能力列表")?;
            crate::account::api::validate_device_id(device.trim())?;
            grants
                .devices
                .insert(device.trim().to_owned(), parse_caps(caps)?);
        }
        for device in clear {
            grants.devices.remove(device.trim());
        }
        client.set_grants(user_id, grants).await?;
        let saved = client.grants(user_id).await?;
        println!("已保存。默认：{}", caps_line(&saved.default));
        for (device, caps) in &saved.devices {
            println!("设备 {device}：{}", caps_line(caps));
        }
        Ok(())
    })
    .await
}

/// `sessions`: the account's logins, optionally revoking one.
pub async fn auth_sessions(revoke: Option<String>) -> Result<()> {
    with_client(|client| async move {
        if let Some(device) = revoke {
            client.revoke_auth_session(&device).await?;
            println!("已注销设备 {device} 上的登录");
        }
        let sessions = client.auth_sessions().await?;
        for session in &sessions.sessions {
            println!(
                "{} [{}] {}{}",
                session.device_alias,
                session.device_id,
                crate::account::api::platform_label(session.platform),
                if session.current { "（本机）" } else { "" }
            );
        }
        Ok(())
    })
    .await
}


/// Sleep for `duration`, returning `false` if Ctrl+C arrived first.
async fn pause(duration: Duration) -> bool {
    tokio::select! {
        _ = tokio::signal::ctrl_c() => false,
        _ = tokio::time::sleep(duration) => true,
    }
}

/// `host:port` of a forwarding target; the host may be a name resolved on
/// the controlled device.
fn forward_target(value: &str) -> Result<(String, u16)> {
    if let Ok(address) = value.parse::<std::net::SocketAddr>() {
        return Ok((address.ip().to_string(), address.port()));
    }
    let (host, port) = value
        .rsplit_once(':')
        .filter(|(host, _)| !host.is_empty() && !host.contains(':'))
        .context("目标应为 主机:端口")?;
    Ok((host.to_owned(), port.parse().context("目标端口无效")?))
}

/// `dev ports`: run one port-forwarding rule through the real controller
/// service (the same one the device center uses) and print its status.
pub async fn dev_ports(
    device_id: &str,
    local: std::net::SocketAddr,
    remote: &str,
    udp: bool,
    seconds: u64,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::features::port_mapping::{
        Protocol, Rule,
        service::{self, Command},
    };
    let (target, remote_port) = forward_target(remote)?;
    let result = with_client(|client| async move {
        let device = account_device(&client, device_id).await?;
        let handle = service::start(Arc::clone(&client), device, options);
        let rule = Rule {
            id: service::new_id(),
            name: "dev".into(),
            local_addr: local.ip(),
            local_port: local.port(),
            target,
            remote_port,
            enabled: true,
            protocol: if udp { Protocol::Udp } else { Protocol::Tcp },
        };
        handle.send(Command::Save(rule.clone()))?;
        handle.set_enabled(true);
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        while std::time::Instant::now() < deadline {
            let snapshot = handle.snapshot();
            let status = snapshot
                .rules
                .iter()
                .find(|(r, _)| r.id == rule.id)
                .map(|(_, s)| {
                    format!(
                        "listening={} connections={} sent={} received={} error={}",
                        s.listening,
                        s.connections,
                        s.total_sent.max(s.sent),
                        s.total_received.max(s.received),
                        s.error.as_deref().unwrap_or("-")
                    )
                })
                .unwrap_or_else(|| "rule pending".into());
            println!(
                "connected={} busy={} error={} {status}",
                snapshot.connected,
                snapshot.busy,
                snapshot.error.as_deref().unwrap_or("-")
            );
            if !pause(Duration::from_secs(1)).await {
                break;
            }
        }
        handle.send(Command::Delete(rule.id))?;
        handle.set_enabled(false);
        pause(Duration::from_millis(500)).await;
        Ok(())
    })
    .await;
    service::shutdown_all().await;
    result
}

async fn wait_files(
    handle: &crate::features::file_transfer::service::Handle,
    deadline: std::time::Instant,
    done: &dyn Fn(&crate::features::file_transfer::service::Snapshot) -> bool,
) -> Result<crate::features::file_transfer::service::Snapshot> {
    loop {
        let snapshot = handle.snapshot();
        if done(&snapshot) {
            return Ok(snapshot);
        }
        anyhow::ensure!(
            std::time::Instant::now() < deadline,
            "等待超时：{}",
            snapshot.error.as_deref().unwrap_or("-")
        );
        anyhow::ensure!(pause(Duration::from_millis(200)).await, "已取消");
    }
}

/// `dev files`: browse a remote folder and optionally download/upload one
/// entry through the real file-transfer service, reporting the throughput.
pub async fn dev_files(
    device_id: &str,
    list: &str,
    download: Option<(String, String)>,
    upload: Option<(String, String)>,
    policy: &str,
    seconds: u64,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::features::file_transfer::service::{self, Command, Direction, Record, TaskState};
    let policy = match policy {
        "overwrite" => 1,
        "rename" => 2,
        "skip" => 3,
        other => anyhow::bail!("未知的同名处理方式：{other}（overwrite、rename 或 skip）"),
    };
    with_client(|client| async move {
        let device = account_device(&client, device_id).await?;
        let handle = service::start(Arc::clone(&client), device, options);
        handle.send(Command::Connect)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(seconds);
        // A refused connection (no permission, device offline) is final.
        let connected = wait_files(&handle, deadline, &|s| s.connected || (!s.connecting && s.error.is_some())).await?;
        if !connected.connected {
            anyhow::bail!("连接失败：{}", connected.error.as_deref().unwrap_or("-"));
        }
        println!("文件服务已连接");
        let before = handle.snapshot().remote.revision;
        handle.send(Command::Browse(true, list.to_owned()))?;
        let listed = wait_files(&handle, deadline, &|s| s.remote.revision != before && !s.remote.busy).await?;
        if let Some(error) = &listed.remote.error {
            anyhow::bail!("浏览远端目录失败：{error}");
        }
        println!("远端目录 {}（{} 项）", listed.remote.path, listed.remote.entries.len());
        for entry in listed.remote.entries.iter().take(50) {
            println!(
                "  {}\t{}\t{}\t{}",
                entry.entry_type, entry.size, entry.name, entry.full_path
            );
        }
        let mut keys = Vec::new();
        for (direction, source, destination) in download
            .map(|(s, d)| (Direction::Download, s, d))
            .into_iter()
            .chain(upload.map(|(s, d)| (Direction::Upload, s, d)))
        {
            // An interrupted (paused) task of the same source and destination
            // resumes; a failed one is retried as a new task.
            let existing = handle.snapshot().records.iter().find_map(|r| {
                (r.direction == direction
                    && r.source == source
                    && r.destination == destination
                    && r.state == TaskState::Paused)
                .then(|| r.key.clone())
            });
            if let Some(key) = existing {
                println!("继续未完成的任务 {key}");
                handle.send(Command::Resume(key.clone()))?;
                wait_files(&handle, deadline, &|s| {
                    s.records.iter().any(|r| {
                        r.key == key && !matches!(r.state, TaskState::Paused | TaskState::Failed)
                    })
                })
                .await?;
                keys.push(key);
                continue;
            }
            let record = Record::new(direction, source, destination, policy);
            keys.push(record.key.clone());
            handle.send(Command::Add(vec![record]))?;
        }
        if keys.is_empty() {
            return Ok(());
        }
        let started = std::time::Instant::now();
        // The first progress a task reports is where it resumed.
        let mut resumed = std::collections::HashMap::<String, u64>::new();
        let mut finished = std::collections::HashMap::<String, Duration>::new();
        let mut last_print = std::time::Instant::now();
        let snapshot = loop {
            let snapshot = handle.snapshot();
            for key in &keys {
                let done = snapshot.records.iter().any(|r| {
                    &r.key == key
                        && matches!(r.state, TaskState::Done | TaskState::Failed | TaskState::Skipped | TaskState::Cancelled)
                });
                // A task that finished before the first poll moved everything.
                if let Some(p) = snapshot.progress.get(key)
                    && p.bytes > 0
                    && !done
                {
                    resumed.entry(key.clone()).or_insert(p.bytes);
                }
                if done {
                    finished.entry(key.clone()).or_insert_with(|| started.elapsed());
                }
            }
            if finished.len() == keys.len() {
                break snapshot;
            }
            if last_print.elapsed() >= Duration::from_secs(2) {
                last_print = std::time::Instant::now();
                for record in snapshot.records.iter().filter(|r| keys.contains(&r.key)) {
                    let p = snapshot.progress.get(&record.key);
                    println!(
                        "  进度 {}：{} / {} 字节，{:.1} MB/s",
                        record.source,
                        p.map_or(0, |p| p.bytes),
                        record.total,
                        p.map_or(0., |p| p.speed) / 1e6
                    );
                }
            }
            anyhow::ensure!(
                std::time::Instant::now() < deadline,
                "等待超时：{}",
                snapshot.error.as_deref().unwrap_or("-")
            );
            anyhow::ensure!(pause(Duration::from_millis(200)).await, "已取消");
        };
        for record in snapshot.records.iter().filter(|r| keys.contains(&r.key)) {
            let elapsed = finished.get(&record.key).copied().unwrap_or_default();
            let from = resumed.get(&record.key).copied().unwrap_or(0).min(record.total);
            let moved = record.total.saturating_sub(from);
            println!(
                "{} {} -> {}：{}（{} 字节，续传起点约 {} 字节，用时 {:.1} 秒，{:.2} MB/s）{}",
                if record.direction == Direction::Download { "下载" } else { "上传" },
                record.source,
                record.destination,
                record.state.label(),
                record.total,
                from,
                elapsed.as_secs_f64(),
                moved as f64 / elapsed.as_secs_f64().max(0.001) / 1e6,
                record.error.as_deref().unwrap_or("")
            );
        }
        Ok(())
    })
    .await
}

/// `assist`: connect through the server to a host by its short number or
/// device ID, with the same preflight as the device center. Works without
/// an account: the host admits this device if it trusts it, or as a
/// visitor with its access code or its user's confirmation.
pub async fn assist(
    target: &str,
    code: Option<String>,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::account::api::ErrorCode;
    let mut request =
        crate::account::assist::AssistRequest::new(target, code.unwrap_or_default())?;
    if !request.has_code() {
        // A host that trusts this device, or only confirms, answers with an
        // open request.
        let client = match AuthenticatedClient::from_saved_session() {
            Ok(client) => client,
            Err(error)
                if error
                    .downcast_ref::<crate::account::client::NoSavedSession>()
                    .is_some() =>
            {
                AuthenticatedClient::guest_with_runtime()?
            }
            Err(error) => return Err(error),
        };
        let preflight = client.assist_join(&request.target, Vec::new()).await;
        if let Ok(pending) = &preflight
            && !pending.trusted
            && pending.mode.needs_password()
        {
            let _ = client.assist_cancel(&pending.request_id).await;
        }
        client.close().await;
        match preflight {
            Ok(pending) if pending.trusted || !pending.mode.needs_password() => {
                request.pending = Some(pending)
            }
            Ok(_) => anyhow::bail!("对方要求访问码，请使用 --code 提供"),
            Err(error) => match crate::account::api::failure_code(&error) {
                Some(ErrorCode::AssistCodeRequired) => {
                    anyhow::bail!("对方要求访问码，请使用 --code 提供")
                }
                Some(ErrorCode::AssistUnavailable) => {
                    anyhow::bail!(crate::i18n::t().app.assist_unavailable)
                }
                _ => return Err(error),
            },
        }
    }
    crate::session::controller::run_assist_viewer_window(
        crate::session::controller::assist_target_label(&request.target),
        options,
        request,
    )
    .await
}

/// `dev power`: send one power request to an account device through the
/// same checks as the device center, and print the server receipt. The
/// target host decides; it refuses unless remote power is allowed there.
pub async fn dev_power(device_id: &str, shutdown: bool) -> Result<()> {
    use crate::account::power::PowerAction;
    with_client(|client| async move {
        let device = account_device(&client, device_id).await?;
        let action = if shutdown { PowerAction::Shutdown } else { PowerAction::Reboot };
        let receipt = client.power_owned_device(&device, action, || {}).await?;
        println!("{}", receipt.summary(action));
        Ok(())
    })
    .await
}

/// `dev diagnostics`: fetch the redacted diagnostic bundle of an account
/// device over the DIAG channel, as the device center does.
pub async fn dev_diagnostics(
    device_id: &str,
    seconds: u64,
    cancel_after_ms: Option<u64>,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::diagnostics::remote::service;
    let result = with_client(|client| async move {
        let device = account_device(&client, device_id).await?;
        service::start(Arc::clone(&client), device, options);
        let started = std::time::Instant::now();
        let deadline = started + Duration::from_secs(seconds);
        let mut cancel_at = cancel_after_ms.map(|ms| started + Duration::from_millis(ms));
        let mut stage = String::new();
        loop {
            if cancel_at.is_some_and(|at| std::time::Instant::now() >= at) {
                cancel_at = None;
                println!("取消（{} ms）", started.elapsed().as_millis());
                service::cancel(&client.device_id(), device_id);
            }
            let snapshot = service::snapshot(&client.device_id(), device_id)
                .context("诊断任务未启动")?;
            if !snapshot.busy {
                if let Some(error) = snapshot.error {
                    anyhow::bail!("获取远端诊断失败：{error}");
                }
                if snapshot.cancelled {
                    println!("已取消，没有保存诊断包");
                    return Ok(());
                }
                println!(
                    "远端诊断包：{}（警告 {} 项）",
                    snapshot
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "-".into()),
                    snapshot.warnings
                );
                return Ok(());
            }
            if snapshot.progress.stage != stage {
                stage = snapshot.progress.stage.clone();
                println!("{stage}");
            }
            anyhow::ensure!(std::time::Instant::now() < deadline, "等待超时");
            anyhow::ensure!(pause(Duration::from_millis(50)).await, "已取消");
        }
    })
    .await;
    service::shutdown_all().await;
    result
}

/// `dev video`: decode the host's first screen for `seconds` without a
/// window, optionally under injected loss and delay (`--loss`, see
/// `transport::rtc::impair`), and print the recovery report as JSON
/// (`docs/protocol/rfi-ltr-brief.md` §6.10).
/// `dev video --no-recovery`: keyframes only, for A/B runs.
/// `connect --dev-loss`: the `dev video --loss` rules for a viewer window.
pub fn dev_impair(spec: &str) -> Result<()> {
    use crate::transport::rtc::impair;
    impair::configure(impair::Config::parse(spec).context("--dev-loss 无效")?)?;
    tracing::warn!(%spec, "video loss injection is on for this viewer");
    Ok(())
}

/// `connect --dev-cinema`: start in or out of cinema mode (not saved).
pub fn dev_cinema(cinema: bool) {
    crate::features::stream_control::set_dev_cinema(cinema);
}

/// `connect --dev-virtual-screen`: add a virtual screen once connected.
pub fn dev_virtual_screen() {
    crate::features::stream_control::set_dev_virtual_screen();
}

/// `connect --dev-audio-only A,B`: audio only at A seconds, the picture
/// again at B seconds.
pub fn dev_audio_only(spec: &str) -> Result<()> {
    let (on, off) = spec.split_once(',').context("expected A,B")?;
    let (on, off): (u64, u64) = (on.trim().parse()?, off.trim().parse()?);
    anyhow::ensure!(off > on, "B must come after A");
    crate::features::stream_control::set_dev_audio_switch(on, off);
    Ok(())
}

/// `connect --dev-quality`: start at this tier (not saved).
pub fn dev_quality(quality: &str) -> Result<()> {
    use crate::features::stream_control::StreamQuality;
    let quality = match quality {
        "auto" => StreamQuality::Auto,
        "original" => StreamQuality::Original,
        "high" => StreamQuality::High,
        "clear" => StreamQuality::Clear,
        "fast" => StreamQuality::Fast,
        other => anyhow::bail!("unknown quality {other}"),
    };
    crate::features::stream_control::set_dev_quality(quality);
    Ok(())
}

pub fn dev_video_without_recovery() {
    crate::protocol::peer::disable_controller_recovery();
}

/// `dev plugins`: load the installed plugins and check the bundled effects.
pub fn dev_plugins() -> Result<()> {
    crate::application::dev_plugins::run()
}

pub fn dev_camera_list() -> Result<()> {
    crate::application::dev_camera::list()
}

pub fn dev_camera_probe(
    camera: Option<&str>,
    mode: Option<&str>,
    seconds: u64,
    snapshot: Option<&std::path::Path>,
) -> Result<()> {
    crate::application::dev_camera::probe(camera, mode, seconds, snapshot)
}

pub fn dev_camera_adjust(camera: Option<&str>, control: &str, value: i32, auto: bool) -> Result<()> {
    crate::application::dev_camera::adjust(camera, control, value, auto)
}

/// `dev video --recovery-late`: no early judgment, for A/B runs.
pub fn dev_video_late_recovery() {
    crate::transport::rtc::recovery::disable_early();
}

pub async fn dev_video(
    device_id: &str,
    seconds: u64,
    loss: Option<&str>,
    camera: bool,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::transport::rtc::impair;
    if let Some(spec) = loss {
        impair::configure(impair::Config::parse(spec).context("--loss 无效")?)?;
    }
    with_client(|client| async move {
        let device = account_device(&client, device_id).await?;
        let policy = client.feature_catalog().policy_for(&device);
        let cancel = tokio_util::sync::CancellationToken::new();
        let mut connection = if camera {
            crate::session::controller::ControllerConnection::connect_headless_camera(
                &client, &device, policy, options, &cancel,
            )
            .await
        } else {
            crate::session::controller::ControllerConnection::connect_headless(
                &client, &device, policy, options, &cancel,
            )
            .await
        }
        .context("连接设备失败")?;
        let (observer, observations) = std::sync::mpsc::channel();
        let started = std::time::Instant::now();
        let result = async {
            let (decoder, codec, performance) = connection.start_headless_video(observer).await?;
            eprintln!("视频已开始，测量 {seconds} 秒");
            let deadline = started + Duration::from_secs(seconds);
            let mut frames = Vec::new();
            while std::time::Instant::now() < deadline {
                tokio::time::sleep(Duration::from_millis(200)).await;
                frames.extend(observations.try_iter());
                if let Some(error) = decoder.error() {
                    anyhow::bail!("解码失败：{error}");
                }
            }
            frames.extend(observations.try_iter());
            let elapsed = started.elapsed();
            drop(decoder);
            let events = impair::events();
            Ok(crate::application::dev_video::report(&crate::application::dev_video::Run {
                codec: match codec {
                    crate::media::VideoCodec::H264 => "H.264",
                    crate::media::VideoCodec::H265 => "H.265",
                    crate::media::VideoCodec::Av1 => "AV1",
                },
                loss,
                recovery_declared: crate::protocol::peer::controller_recovery(),
                started,
                elapsed,
                events: &events,
                frames: &frames,
                keyframe_requests: impair::keyframe_requests(),
                received_rtp_bytes: performance.snapshot().total_received_rtp_bytes,
                queue_drops: impair::queue_stats().0,
                queue_max: impair::queue_stats().1,
                network_drops: impair::network_drops(),
            }))
        }
        .await;
        if let Err(error) = connection.close().await {
            tracing::debug!(%error, "dev video connection close");
        }
        println!("{}", serde_json::to_string_pretty(&result?)?);
        Ok(())
    })
    .await
}

/// `dev clipboard`: check CLIPBOARD v2 end to end against a real host on
/// this machine. The controller side runs an in-memory exchange (it never
/// touches the Windows clipboard); this process reads and writes the shared
/// Windows clipboard directly to see what the host renders and offers.
/// Overwrites the clipboard of the current Windows user.
///
/// `pull_only`: this participant has `clipboard` but no automatic sync (a
/// shared session without `control`): check that copies are not exchanged
/// automatically and that `Pull` still copies the host clipboard.
pub async fn dev_clipboard(
    device_id: &str,
    scratch: std::path::PathBuf,
    pull_only: bool,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::features::clipboard::{self, Clipboard};
    let exchange = Clipboard::memory(crate::transport::transfer::kind::CLIPBOARD);
    clipboard::install_dev_exchange(exchange.clone());
    with_client(|client| async move {
        let device = account_device(&client, device_id).await?;
        let policy = client.feature_catalog().policy_for(&device);
        let cancel = tokio_util::sync::CancellationToken::new();
        let connection = crate::session::controller::ControllerConnection::connect_files(
            &client, &device, policy, options, &cancel,
        )
        .await
        .context("连接设备失败")?;
        println!("会话已建立，等待剪贴板通道就绪");
        tokio::time::sleep(Duration::from_secs(3)).await;
        let result = tokio::task::spawn_blocking(move || {
            if pull_only {
                clipboard_pull_checks(&exchange)
            } else {
                clipboard_checks(&exchange, &scratch)
            }
        })
            .await
            .context("剪贴板检查线程异常结束")?;
        if let Err(error) = connection.close().await {
            tracing::debug!(%error, "dev clipboard connection close");
        }
        result
    })
    .await
}

fn clipboard_pull_checks(exchange: &crate::features::clipboard::Clipboard) -> Result<()> {
    use crate::features::clipboard::{system, wire::Format};
    let utf16 = |text: &str| -> Vec<u8> {
        text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    };
    let starts = |got: &[u8], want: &[u8]| got.len() >= want.len() && got[..want.len()] == *want;
    println!("1. 主控的复制不会自动写入被控剪贴板");
    let ours = utf16("来自主控、不应同步的文本");
    exchange.memory_offer(vec![(Format { id: 13, name: String::new() }, ours.clone())], Vec::new())?;
    std::thread::sleep(Duration::from_millis(2000));
    let current = system::data(13).unwrap_or_default();
    anyhow::ensure!(!starts(&current, &ours), "被控端应用了不自动同步的参与者的复制");
    println!("  被控剪贴板未被改写");
    println!("2. 被控的复制不会自动发给本参与者");
    let theirs = utf16("被控端的文本，只能手动获取");
    system::set(vec![(13, theirs.clone())], Vec::new())?;
    anyhow::ensure!(
        exchange.memory_wait_offer(0, Duration::from_secs(3)).is_err(),
        "被控端向不自动同步的参与者发送了复制"
    );
    println!("  3 秒内没有收到被控端的复制");
    println!("3. 手动获取被控剪贴板（Pull）");
    let count = exchange.pull()?;
    let (offer, formats) = exchange.memory_wait_offer(0, Duration::from_secs(5))?;
    let text = formats
        .iter()
        .find(|f| f.id == 13)
        .cloned()
        .context("获取结果没有 Unicode 文本")?;
    let got = exchange.memory_fetch(offer, &text)?;
    anyhow::ensure!(starts(&got, &theirs), "获取的文本不一致");
    println!("  获取结果：{count} 种格式，文本一致");
    println!("全部手动获取检查通过");
    Ok(())
}

fn clipboard_checks(
    exchange: &crate::features::clipboard::Clipboard,
    scratch: &std::path::Path,
) -> Result<()> {
    use crate::features::clipboard::{system, wire::{FileItem, Format}};
    use sha2::{Digest, Sha256};
    let pattern = |len: usize, seed: u8| -> Vec<u8> {
        (0..len)
            .map(|i| ((i as u32).wrapping_mul(2_654_435_761) >> 13) as u8 ^ seed)
            .collect()
    };
    let digest = |data: &[u8]| format!("{:x}", Sha256::digest(data));
    let utf16 = |text: &str| -> Vec<u8> {
        text.encode_utf16().chain([0]).flat_map(u16::to_le_bytes).collect()
    };
    // HGLOBAL sizes may be rounded up: compare the prefix and allow padding.
    let same = |label: &str, got: &[u8], want: &[u8]| -> Result<()> {
        anyhow::ensure!(
            got.len() >= want.len() && got.len() - want.len() < 64 && got[..want.len()] == *want,
            "{label}：内容不一致（收到 {} 字节，应为 {} 字节）",
            got.len(),
            want.len()
        );
        println!("  {label}：一致（{} 字节）", want.len());
        Ok(())
    };
    let timed = |label: &str, bytes: usize, started: std::time::Instant| {
        let seconds = started.elapsed().as_secs_f64();
        println!(
            "  {label}：{bytes} 字节，用时 {seconds:.2} 秒，{:.1} MB/s",
            bytes as f64 / seconds.max(0.001) / 1e6
        );
    };

    // The host announces an offer on the system clipboard (rendered on
    // demand); under load that can take longer than usual.
    let wait_format = |id: u32| -> Result<()> {
        let started = std::time::Instant::now();
        while unsafe { windows::Win32::System::DataExchange::IsClipboardFormatAvailable(id) }.is_err() {
            anyhow::ensure!(started.elapsed() < Duration::from_secs(15), "被控剪贴板 15 秒内没有出现格式 {id}");
            std::thread::sleep(Duration::from_millis(50));
        }
        if started.elapsed() > Duration::from_millis(500) {
            println!("  被控剪贴板就绪：{:.1} 秒", started.elapsed().as_secs_f64());
        }
        Ok(())
    };
    println!("1. 主控 → 被控：文本、HTML、图像（DIB）与 24 MiB 自定义格式");
    let text = utf16("Farwing 剪贴板 v2 测试 ✓ 0123456789");
    let html_id = system::register("HTML Format");
    let html = b"Version:0.9\r\nStartHTML:0000000105\r\nEndHTML:0000000175\r\nStartFragment:0000000141\r\nEndFragment:0000000139\r\n<html><body><!--StartFragment--><b>rich</b><!--EndFragment--></body></html>\0".to_vec();
    let mut dib = Vec::new();
    for value in [40u32, 256, 256] {
        dib.extend_from_slice(&value.to_le_bytes());
    }
    dib.extend_from_slice(&1u16.to_le_bytes());
    dib.extend_from_slice(&32u16.to_le_bytes());
    dib.extend_from_slice(&[0; 24]);
    dib.extend(pattern(256 * 256 * 4, 7));
    let blob_id = system::register("Farwing.Test.Blob");
    let blob = pattern(24 * 1024 * 1024 + 11, 3);
    exchange.memory_offer(
        vec![
            (Format { id: 13, name: String::new() }, text.clone()),
            (Format { id: html_id, name: "HTML Format".into() }, html.clone()),
            (Format { id: 8, name: String::new() }, dib.clone()),
            (Format { id: blob_id, name: "Farwing.Test.Blob".into() }, blob.clone()),
        ],
        Vec::new(),
    )?;
    wait_format(blob_id)?;
    same("文本", &system::data(13)?, &text)?;
    same("HTML", &system::data(html_id)?, &html)?;
    same("图像", &system::data(8)?, &dib)?;
    let started = std::time::Instant::now();
    same("自定义格式", &system::data(blob_id)?, &blob)?;
    timed("自定义格式传输", blob.len(), started);

    println!("2. 主控 → 被控：文件（含空文件、空目录与 40 MiB 文件）");
    let files = vec![
        (FileItem { path: "Farwing剪贴板".into(), attributes: 0x10, mtime: 133_700_000_000_000_000, size: 0 }, Vec::new()),
        (FileItem { path: "Farwing剪贴板\\空目录".into(), attributes: 0x10, mtime: 133_700_000_000_000_000, size: 0 }, Vec::new()),
        (FileItem { path: "Farwing剪贴板\\空文件.txt".into(), attributes: 0x20, mtime: 133_700_000_000_000_000, size: 0 }, Vec::new()),
        (FileItem { path: "Farwing剪贴板\\小文件.txt".into(), attributes: 0x20, mtime: 133_700_000_000_000_000, size: 13 }, b"hello, world!".to_vec()),
        (FileItem { path: "Farwing剪贴板\\大文件.bin".into(), attributes: 0x20, mtime: 133_700_000_000_000_000, size: 40 * 1024 * 1024 + 5 }, pattern(40 * 1024 * 1024 + 5, 9)),
    ];
    exchange.memory_offer(Vec::new(), files.clone())?;
    wait_format(system::register("FileGroupDescriptorW"))?;
    let started = std::time::Instant::now();
    let rendered = system::files()?;
    timed("文件读取", files.iter().map(|(_, data)| data.len()).sum(), started);
    anyhow::ensure!(rendered.len() == files.len(), "文件数量不一致：{} / {}", rendered.len(), files.len());
    for ((want, data), (got, contents)) in files.iter().zip(&rendered) {
        anyhow::ensure!(
            want.path == got.path && want.size == got.size && (want.attributes & 0x10) == (got.attributes & 0x10),
            "文件清单不一致：{} / {}",
            want.path,
            got.path
        );
        anyhow::ensure!(digest(data) == digest(contents), "文件内容不一致：{}", want.path);
    }
    println!("  文件清单与内容：一致（{} 项）", files.len());

    println!("3. 被控 → 主控：文本与 8 MiB 自定义格式");
    let (before, _) = exchange
        .memory_wait_offer(0, Duration::from_millis(1))
        .unwrap_or((0, Vec::new()));
    let text = utf16("来自被控端的文本 ✓");
    let blob = pattern(8 * 1024 * 1024 + 3, 5);
    system::set(vec![(13, text.clone()), (blob_id, blob.clone())], Vec::new())?;
    let (offer, formats) = exchange.memory_wait_offer(before, Duration::from_secs(10))?;
    let format = |id: u32| {
        formats
            .iter()
            .find(|f| f.id == id)
            .cloned()
            .with_context(|| format!("被控端未提供格式 {id}"))
    };
    same("文本", &exchange.memory_fetch(offer, &format(13)?)?, &text)?;
    let started = std::time::Instant::now();
    same("自定义格式", &exchange.memory_fetch(offer, &format(blob_id)?)?, &blob)?;
    timed("自定义格式传输", blob.len(), started);

    println!("4. 被控 → 主控：文件（CF_HDROP，含子目录与 30 MiB 文件）");
    let root = scratch.join("clipboard-source");
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("子目录").join("空目录"))?;
    let sources = [
        ("a.txt", b"first file".to_vec()),
        ("子目录\\b.bin", pattern(30 * 1024 * 1024 + 7, 1)),
        ("子目录\\空.txt", Vec::new()),
    ];
    for (name, data) in &sources {
        std::fs::write(root.join(name), data)?;
    }
    system::set(Vec::new(), vec![root.clone()])?;
    let (offer, formats) = exchange.memory_wait_offer(offer, Duration::from_secs(10))?;
    anyhow::ensure!(
        formats.iter().any(|f| f.name == "FileGroupDescriptorW"),
        "被控端未提供文件列表"
    );
    let list = exchange.memory_list(offer)?;
    let started = std::time::Instant::now();
    let mut total = 0;
    for (name, data) in &sources {
        let path = format!("clipboard-source\\{name}");
        let (index, item) = list
            .iter()
            .enumerate()
            .find(|(_, item)| item.path == path)
            .with_context(|| format!("文件清单缺少 {path}"))?;
        anyhow::ensure!(item.size == data.len() as u64, "{path} 大小不一致");
        let got = exchange.memory_read(offer, index as u32, item.size)?;
        anyhow::ensure!(digest(&got) == digest(data), "{path} 内容不一致");
        total += data.len();
    }
    anyhow::ensure!(
        list.iter().any(|item| item.path == "clipboard-source\\子目录\\空目录" && item.attributes & 0x10 != 0),
        "文件清单缺少空目录"
    );
    timed("文件读取", total, started);
    println!("  文件清单与内容：一致（{} 项）", list.len());
    exchange.memory_release(offer);

    println!("5. 手动获取被控剪贴板（Pull）");
    let count = exchange.pull()?;
    let (pulled, formats) = exchange.memory_wait_offer(offer, Duration::from_secs(5))?;
    anyhow::ensure!(
        formats.iter().any(|f| f.name == "FileGroupDescriptorW"),
        "获取结果没有文件列表"
    );
    let list = exchange.memory_list(pulled)?;
    let (index, item) = list
        .iter()
        .enumerate()
        .find(|(_, item)| item.path == "clipboard-source\\a.txt")
        .context("获取结果缺少 a.txt")?;
    anyhow::ensure!(
        exchange.memory_read(pulled, index as u32, item.size)? == b"first file",
        "获取的文件内容不一致"
    );
    exchange.memory_release(pulled);
    println!("  获取结果：{count} 种格式，文件内容一致");
    let _ = std::fs::remove_dir_all(&root);
    println!("全部剪贴板检查通过");
    Ok(())
}

// ------------------------------------------------- direct connections (§12)

/// `id`: this device's ID and key.
pub fn id() -> Result<()> {
    let device = crate::transport::direct::local_device()?;
    println!("设备名称：{}", device.name);
    println!("设备 ID：{}", device.identity.device_id);
    println!("设备公钥：{}", device.identity.public_key());
    println!(
        "对方信任本机：Farwing trusted add {} --key {} --name \"{}\"",
        device.identity.device_id,
        device.identity.public_key(),
        device.name
    );
    Ok(())
}

/// `lan`: hosts that answer on the local network.
pub async fn lan(seconds: u64) -> Result<()> {
    use crate::transport::direct::store;
    let found = crate::transport::direct::lan::discover(Duration::from_secs(seconds.clamp(1, 30))).await?;
    if found.is_empty() {
        println!("局域网中没有发现 Farwing 设备");
        return Ok(());
    }
    let own = store::own_devices_account()
        .map(|account| store::own_devices(&account))
        .unwrap_or_default();
    for host in found {
        let announce = &host.announce;
        let relation = if own.iter().any(|d| d.public_key == announce.public_key) {
            "本账号设备"
        } else if store::known_host(&announce.device_id).is_some_and(|h| h.trusted) {
            "对方信任本机"
        } else if announce
            .methods
            .iter()
            .any(|m| m == farwing_protocol::direct::method::ASSIST)
        {
            // As the device centre's "附近" says it.
            "需要访问码或确认"
        } else {
            "未开启访问码"
        };
        let accepts = if announce.methods.is_empty() {
            String::new()
        } else {
            format!("，接受：{}", announce.methods.join(","))
        };
        println!(
            "{} [{}] {} — {}{accepts}",
            announce.name,
            announce.device_id,
            host.target(),
            relation,
        );
    }
    Ok(())
}

/// The host side of this device: the background service's (which hosts in
/// service mode, reached over its pipe) or this profile's.
fn direct_host() -> Result<(Option<String>, crate::transport::direct::managed::HostDirect)> {
    use crate::transport::direct::managed;
    if managed::managed() {
        let (account, state) = managed::query().context("无法读取后台服务的直连状态")?;
        Ok((Some(account), state))
    } else {
        Ok((None, managed::HostDirect::current()))
    }
}

/// Apply a host-side change: in the background service when it hosts,
/// otherwise in this profile.
fn host_action(action: crate::transport::direct::managed::Action) -> Result<()> {
    use crate::transport::direct::managed;
    match direct_host()? {
        (Some(account), _) => managed::request(account, action),
        (None, _) => managed::perform(None, action),
    }
}

/// `trusted list`
pub fn trusted_list() -> Result<()> {
    use crate::transport::direct::store;
    let (service, host) = direct_host()?;
    let time = |seconds: i64| {
        chrono::DateTime::from_timestamp(seconds, 0)
            .map(|t| t.with_timezone(&chrono::Local).format("%Y-%m-%d %H:%M").to_string())
            .unwrap_or_else(|| "-".into())
    };
    println!(
        "本机信任的设备{}：",
        if service.is_some() { "（后台服务）" } else { "" }
    );
    for device in &host.trusted {
        println!(
            "  {} [{}]，能力 {}，信任于 {}，最近连接 {}",
            device.name,
            device.device_id,
            caps_line(&device.grants),
            time(device.trusted_at),
            if device.last_seen > 0 { time(device.last_seen) } else { "-".into() }
        );
    }
    if host.trusted.is_empty() {
        println!("  （无）");
    }
    let hosts: Vec<_> = store::known_hosts().into_iter().filter(|h| h.trusted).collect();
    println!("信任本机的设备：");
    for peer in &hosts {
        println!(
            "  {} [{}]，地址 {}{}，最近连接 {}",
            peer.name,
            peer.device_id,
            if peer.address.is_empty() { "-" } else { &peer.address },
            if peer.connect_id.is_empty() { String::new() } else { format!("，短号 {}", peer.connect_id) },
            time(peer.last_seen)
        );
    }
    if hosts.is_empty() {
        println!("  （无）");
    }
    Ok(())
}

/// `trusted add`
pub fn trusted_add(device_id: &str, key: &str, name: &str, grants: Option<&str>) -> Result<()> {
    let device_id = crate::account::api::parse_device_id(device_id).context("设备 ID 无效")?;
    let grants = match grants {
        Some(text) => parse_caps(text)?,
        None => farwing_protocol::caps::DEFAULT_ASSIST
            .iter()
            .map(|cap| (*cap).to_owned())
            .collect(),
    };
    let name = if name.trim().is_empty() { device_id.clone() } else { name.trim().to_owned() };
    host_action(crate::transport::direct::managed::Action::Trust {
        paired: false,
        device_id: device_id.clone(),
        public_key: key.trim().to_owned(),
        name,
        grants: grants.clone(),
    })?;
    println!(
        "已信任设备 {}：{}（它可以直接连接本机，无需访问码）",
        crate::account::api::device_id_text(&device_id),
        caps_line(&crate::transport::direct::store::with_view(&grants))
    );
    Ok(())
}

/// `trusted remove`: stop trusting a device here, and forget that it
/// trusted this one.
pub fn trusted_remove(device_id: &str) -> Result<()> {
    let device_id = crate::account::api::parse_device_id(device_id).context("设备 ID 无效")?;
    let (_, host) = direct_host()?;
    let trusted = host.trusted.iter().any(|d| d.device_id == device_id);
    if trusted {
        host_action(crate::transport::direct::managed::Action::Remove {
            device_id: device_id.clone(),
        })?;
    }
    let known = crate::transport::direct::store::forget_host(&device_id)?;
    match (trusted, known) {
        (false, false) => println!("本机没有信任设备 {device_id}，也没有它的记录"),
        (true, _) => println!("已不再信任设备 {device_id}，它与本机的现有连接已断开"),
        (false, true) => println!("已删除设备 {device_id} 的记录"),
    }
    Ok(())
}

/// `trusted grant`
pub fn trusted_grant(device_id: &str, grants: &str) -> Result<()> {
    let device_id = crate::account::api::parse_device_id(device_id).context("设备 ID 无效")?;
    let caps = parse_caps(grants)?;
    let (_, host) = direct_host()?;
    anyhow::ensure!(
        host.trusted.iter().any(|d| d.device_id == device_id),
        "本机没有信任设备 {device_id}"
    );
    host_action(crate::transport::direct::managed::Action::SetGrants {
        device_id,
        grants: caps.clone(),
    })?;
    println!(
        "已保存：{}（下次连接时生效）",
        caps_line(&crate::transport::direct::store::with_view(&caps))
    );
    Ok(())
}

/// `dev input`: run a script of keyboard and mouse steps through a real
/// controller session (the viewer's input path, without a window). The
/// controller side keeps an in-memory clipboard; it never touches the
/// Windows clipboard of this machine. Everything held is released on exit,
/// including Ctrl+C and errors.
pub async fn dev_input(
    device_id: Option<&str>,
    direct: Option<&str>,
    script: &std::path::Path,
    screen: Option<i32>,
    hold_ms: u64,
    allow_disruptive: bool,
    options: crate::media::ConnectionMediaOptions,
) -> Result<()> {
    use crate::application::dev_input::{self, Runner};
    use crate::features::clipboard::{self, Clipboard};
    let text = std::fs::read_to_string(script)
        .with_context(|| format!("无法读取脚本 {}", script.display()))?;
    let steps = dev_input::parse_script(&text, allow_disruptive)?;
    anyhow::ensure!(!steps.is_empty(), "脚本没有任何步骤");
    let exchange = Clipboard::memory(crate::transport::transfer::kind::CLIPBOARD);
    clipboard::install_dev_exchange(exchange.clone());
    let run = |connection: crate::session::controller::ControllerConnection| async move {
        println!("会话已建立，等待键鼠通道就绪");
        let runner_options = dev_input::Options {
            screen,
            hold: Duration::from_millis(hold_ms),
        };
        let result = async {
            let runner = Runner::start(
                connection.stream_control_handle(),
                exchange,
                &runner_options,
            )
            .await;
            let runner = match runner {
                Ok(runner) => runner,
                Err(error) => return Err(error),
            };
            let total = steps.len();
            let script = async {
                for (index, step) in steps.iter().enumerate() {
                    runner
                        .run(step)
                        .await
                        .with_context(|| format!("第 {} 步（{step}）失败", index + 1))?;
                    println!("sent {}/{total}: {step}", index + 1);
                }
                Ok::<_, anyhow::Error>(())
            };
            let result = tokio::select! {
                result = script => result,
                _ = tokio::signal::ctrl_c() => Err(anyhow::anyhow!("已中断")),
            };
            // Key and button ups for anything still held, before closing.
            runner.finish().await;
            match &result {
                Ok(()) => println!("完成：{total} 步全部送出"),
                Err(error) => println!("中止：{error:#}"),
            }
            result
        }
        .await;
        if let Err(error) = connection.close().await {
            tracing::debug!(%error, "dev input connection close");
        }
        result
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    if let Some(address) = direct {
        let request = crate::session::controller::direct::DirectRequest::parse(address, None, false)?;
        let (connection, host) =
            crate::session::controller::direct::connect_headless(&request, options, &cancel)
                .await
                .context("直连被控端失败")?;
        println!("被控端：{}（{}）", host.name, host.device_id);
        return run(connection).await;
    }
    let device_id = device_id.context("需要 --device-id 或 --direct")?.to_owned();
    with_client(|client| async move {
        let device = account_device(&client, &device_id).await?;
        let policy = client.feature_catalog().policy_for(&device);
        let connection = crate::session::controller::ControllerConnection::connect_headless(
            &client, &device, policy, options, &cancel,
        )
        .await
        .context("连接设备失败")?;
        println!("被控端：{}（{}）", device.alias, device.device_id);
        run(connection).await
    })
    .await
}
