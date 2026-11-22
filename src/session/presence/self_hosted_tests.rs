use super::*;
use crate::{
    account::{
        api::{DeviceInfo, RoomSession},
        auth::KeyringIdentityStore,
        client::AuthenticatedClient,
        self_hosted::SelfHosted,
    },
    features::{file_transfer::service as files, host::assist, port_mapping},
    media::{CodecPreference, ConnectionMediaOptions, FrameRateChoice, LocalDisplayInfo},
    session::{
        controller::ControllerConnection,
        device_session::DeviceRuntime,
        negotiation::{ControlConnectType, ControlPurpose},
    },
};
use anyhow::Context;
use sha2::{Digest, Sha256};
use std::{
    io::{BufRead, BufReader},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
};

pub(crate) struct TestServer {
    child: Child,
    directory: PathBuf,
}

impl TestServer {
    pub(crate) async fn start(ice_servers: &serde_json::Value) -> Result<(Self, url::Url)> {
        let directory =
            std::env::temp_dir().join(format!("farwing-windows-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory)?;
        let child = Command::new("node")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .arg("server/index.js")
            .env("HOST", "127.0.0.1")
            .env("PORT", "0")
            .env("OPENUUYC_DB_PATH", directory.join("devices.sqlite"))
            .env("PUBLIC_ORIGIN", "")
            .env("DESKTOP_CLIENT_NETWORKS", "127.0.0.1")
            .env("SELF_HOST_ICE_SERVERS", ice_servers.to_string())
            .env_remove("TLS_CERT_FILE")
            .env_remove("TLS_KEY_FILE")
            .stdout(Stdio::piped())
            .spawn();
        let mut server = match child {
            Ok(child) => Self { child, directory },
            Err(error) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(error).context("start the Farwing Node.js server");
            }
        };
        let output = server
            .child
            .stdout
            .take()
            .context("missing server output")?;
        let origin = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::task::spawn_blocking(move || {
                let mut line = String::new();
                BufReader::new(output).read_line(&mut line)?;
                let origin = line
                    .strip_prefix("OpenUUYC self-host server listening at ")
                    .and_then(|address| address.split_whitespace().next())
                    .context("missing server origin")?;
                crate::account::self_hosted::parse_origin(origin)
            }),
        )
        .await???;
        Ok((server, origin))
    }
}

async fn test_turn() -> Result<(webrtc::turn::server::Server, serde_json::Value)> {
    use webrtc::turn::{
        auth::{LongTermAuthHandler, generate_long_term_credentials},
        relay::relay_static::RelayAddressGeneratorStatic,
        server::{
            Server,
            config::{ConnConfig, ServerConfig},
        },
    };
    let route = tokio::net::UdpSocket::bind("0.0.0.0:0").await?;
    // UDP connect selects a source interface without sending a packet. The
    // ICE agent binds TURN clients to that interface, so use the same address.
    route.connect("192.0.2.1:9").await?;
    let address = route.local_addr()?.ip();
    let socket = tokio::net::UdpSocket::bind((address, 0)).await?;
    let endpoint = socket.local_addr()?;
    let secret = uuid::Uuid::new_v4().to_string();
    let (username, credential) = generate_long_term_credentials(&secret, Duration::from_secs(300))?;
    let turn = Server::new(ServerConfig {
        conn_configs: vec![ConnConfig {
            conn: Arc::new(socket),
            relay_addr_generator: Box::new(RelayAddressGeneratorStatic {
                relay_address: address,
                address: address.to_string(),
                net: Arc::new(webrtc::util::vnet::net::Net::new(None)),
            }),
        }],
        realm: "farwing-test".to_owned(),
        auth_handler: Arc::new(LongTermAuthHandler::new(secret)),
        channel_bind_timeout: Duration::from_secs(600),
        alloc_close_notify: None,
    })
    .await?;
    Ok((
        turn,
        serde_json::json!([{
            "urls": format!("turn:{endpoint}?transport=udp"),
            "username": username,
            "credential": credential,
        }]),
    ))
}

impl Drop for TestServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Ok(directory) = self.directory.canonicalize()
            && let Ok(temp) = std::env::temp_dir().canonicalize()
            && directory.parent() == Some(temp.as_path())
            && directory.file_name() == self.directory.file_name()
        {
            let _ = std::fs::remove_dir_all(directory);
        }
    }
}

struct TestPeer(Child);

impl TestPeer {
    fn start(origin: &url::Url, directory: &Path, role: &str) -> Result<Self> {
        let module = module_path!()
            .split_once("::")
            .context("missing native test module path")?
            .1;
        let test =
            format!("{module}::two_windows_peers_decode_video_through_self_hosted_signaling");
        let profile = directory.join(format!("{role}-profile"));
        std::fs::create_dir(&profile)?;
        Ok(Self(
            Command::new(std::env::current_exe()?)
                .args(["--ignored", "--nocapture", "--exact", &test])
                .env("FARWING_TEST_PEER_ROLE", role)
                .env("FARWING_TEST_PEER_ORIGIN", origin.as_str())
                .env("FARWING_TEST_PEER_DIRECTORY", directory)
                .env("LOCALAPPDATA", profile)
                .stdin(Stdio::piped())
                .spawn()?,
        ))
    }

    async fn wait_for_registration(&mut self, ready_file: &Path) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(60), async {
            loop {
                if let Some(status) = self.0.try_wait()? {
                    anyhow::bail!("native host exited before registration: {status}");
                }
                if let Ok(id) = std::fs::read_to_string(ready_file)
                    && crate::account::api::validate_device_id(&id).is_ok()
                {
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("native host did not register")?
    }

    async fn wait(mut self, timeout: Duration) -> Result<()> {
        tokio::time::timeout(timeout, async {
            loop {
                if let Some(status) = self.0.try_wait()? {
                    anyhow::ensure!(status.success(), "native peer failed: {status}");
                    return Ok(());
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("native peer did not stop")?
    }

    async fn stop(mut self) -> Result<()> {
        drop(self.0.stdin.take());
        self.wait(Duration::from_secs(15)).await
    }
}

impl Drop for TestPeer {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

struct TestIdentity(KeyringIdentityStore);

impl Drop for TestIdentity {
    fn drop(&mut self) {
        self.0.clear_test_identity();
    }
}

fn client(origin: url::Url) -> Result<(TestIdentity, DeviceRuntime, Arc<AuthenticatedClient>)> {
    let token = format!("{:x}", Sha256::digest(rand::random::<[u8; 32]>()));
    let device_id = format!("{:x}", Sha256::digest(token.as_bytes()))[..16].to_owned();
    let identity = TestIdentity(KeyringIdentityStore::for_self_hosted(&device_id)?);
    let runtime = DeviceRuntime::start_configured(Some(SelfHosted {
        origin,
        device_id,
        token,
    }))?;
    let client = AuthenticatedClient::from_saved_session_with_device(runtime.handle())?;
    Ok((identity, runtime, Arc::new(client)))
}

async fn run_test_host(origin: url::Url, directory: &Path) -> Result<()> {
    let (_identity, runtime, host) = client(origin)?;
    host.account_info().await?;
    host.host.set_allowed(true);
    host.host.set_file_transfer_allowed(true)?;
    host.host.set_port_mapping_allowed(true)?;
    let mut settings = assist::Settings {
        enabled: true,
        mode: assist::Mode::PasswordConfirmation,
        ..Default::default()
    };
    settings.refresh_code();
    host.host.set_assistance(settings)?;
    let (events, _updates) = std::sync::mpsc::channel();
    let stop = CancellationToken::new();
    let _stop_presence = stop.clone().drop_guard();
    host.host.prepare_startup(stop.clone()).await?;
    std::fs::write(directory.join("host-id"), host.device_id())?;
    let presence = run_presence(host.clone().into(), events, stop.clone());
    let shutdown = tokio::task::spawn_blocking(|| {
        use std::io::Read;
        std::io::stdin().read(&mut [0]).map(|_| ())
    });
    tokio::pin!(presence);
    tokio::pin!(shutdown);
    let mut confirmation = tokio::time::interval(Duration::from_millis(100));
    let mut published = false;
    let mut answered = None;
    let result = loop {
        tokio::select! {
            result = &mut presence => break result,
            stopped = &mut shutdown => {
                stopped??;
                stop.cancel();
                break presence.await;
            }
            _ = confirmation.tick() => {
                let assistance = &host.host.assistance;
                assistance.touch_ui();
                let snapshot = assistance.snapshot();
                if !published && snapshot.ready && !snapshot.connect_id.is_empty() {
                    std::fs::write(
                        directory.join("assistance.json"),
                        serde_json::to_vec(&(snapshot.connect_id, snapshot.settings.code))?,
                    )?;
                    published = true;
                }
                if let Some(pending) = snapshot.pending
                    && answered.as_ref() != Some(&pending.id)
                {
                    answered = Some(pending.id.clone());
                    assistance.answer(pending.id, pending.token, true)?;
                }
            }
        }
    };
    host.close().await;
    runtime.close().await;
    result
}

async fn test_tcp_forwarding(connection: &ControllerConnection) -> Result<()> {
    let target = TcpListener::bind("127.0.0.1:0").await?;
    let target_addr = target.local_addr()?;
    let reserved = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = reserved.local_addr()?;
    drop(reserved);
    let stop = CancellationToken::new();
    let status = Arc::new(std::sync::Mutex::new(port_mapping::RuleStatus::default()));
    let rule = port_mapping::Rule {
        id: 1,
        name: "Farwing test".to_owned(),
        local_addr: local_addr.ip(),
        local_port: local_addr.port(),
        target: target_addr.ip(),
        remote_port: target_addr.port(),
        enabled: true,
        protocol: port_mapping::Protocol::Tcp,
    };
    let forwarding = port_mapping::run_rule(
        rule,
        connection.port_mapping_transport(),
        stop.clone(),
        status.clone(),
        Arc::new(tokio::sync::Notify::new()),
    );
    let verify = async {
        let _stop_forwarding = stop.drop_guard();
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let current = status.lock().unwrap().clone();
                if let Some(error) = current.error {
                    anyhow::bail!("TCP forwarding failed: {error}");
                }
                if current.listening {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            let echo = async {
                let mut connections = tokio::task::JoinSet::new();
                loop {
                    let (mut socket, _) = target.accept().await?;
                    connections.spawn(async move {
                        let (mut reader, mut writer) = socket.split();
                        tokio::io::copy(&mut reader, &mut writer).await
                    });
                }
            };
            let round_trip = async {
                let mut request = [0; 8192];
                rand::fill(&mut request);
                let mut socket = TcpStream::connect(local_addr).await?;
                socket.write_all(&request).await?;
                let mut reply = [0; 8192];
                socket.read_exact(&mut reply).await?;
                anyhow::ensure!(reply == request, "TCP forwarding changed the payload");
                Ok(())
            };
            tokio::select! {
                result = echo => result,
                result = round_trip => result,
            }
        })
        .await
        .context("TCP forwarding round trip timed out")?
    };
    let (_, result) = tokio::join!(forwarding, verify);
    result
}

async fn test_file_transfer(
    directory: &Path,
    controller: Arc<AuthenticatedClient>,
    device: DeviceInfo,
    options: ConnectionMediaOptions,
) -> Result<()> {
    let upload = directory.join("upload");
    let remote = directory.join("remote");
    let download = directory.join("download");
    for directory in [&upload, &remote, &download] {
        std::fs::create_dir(directory)?;
    }
    let name = "payload.bin";
    let mut payload = vec![0; 512 * 1024];
    rand::fill(payload.as_mut_slice());
    std::fs::write(upload.join(name), &payload)?;
    let handle = files::start(controller, device, options);
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let state = handle.snapshot();
            if let Some(error) = state.error {
                anyhow::bail!("file connection failed: {error}");
            }
            if state.connected {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("file connection did not start")??;
    for (direction, source, destination) in [
        (files::Direction::Upload, upload.join(name), remote.clone()),
        (files::Direction::Download, remote.join(name), download),
    ] {
        let record = files::Record::new(
            direction,
            source
                .to_str()
                .context("source path is invalid")?
                .to_owned(),
            destination
                .to_str()
                .context("destination path is invalid")?
                .to_owned(),
            2,
        );
        let key = record.key.clone();
        handle.send(files::Command::Add(vec![record]))?;
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let state = handle.snapshot();
                if let Some(error) = state.error {
                    anyhow::bail!("file transfer failed: {error}");
                }
                if let Some(record) = state.records.iter().find(|record| record.key == key) {
                    match record.state {
                        files::TaskState::Done => return Ok(()),
                        files::TaskState::Queued | files::TaskState::Running => {}
                        state => anyhow::bail!(
                            "file transfer ended with {}: {}",
                            state.label(),
                            record.error.as_deref().unwrap_or_default()
                        ),
                    }
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("file transfer timed out")??;
        anyhow::ensure!(
            std::fs::read(destination.join(name))? == payload,
            "transferred file content differs from the source"
        );
    }
    Ok(())
}

async fn check_video(
    connection: &mut ControllerConnection,
    force_relay: bool,
    check_tcp: bool,
) -> Result<String> {
    let (_viewer, summary) = connection.start_native_viewer("Self-hosted test").await?;
    anyhow::ensure!(
        summary.codec == "H.264",
        "unexpected negotiated video codec"
    );
    let monitor = connection.performance_monitor();
    let route = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let snapshot = monitor.snapshot();
            if snapshot.total_decoded_frames > 0
                && !snapshot.connection.is_empty()
                && snapshot.connection != "连接中"
            {
                break snapshot.connection.clone();
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .context("no decoded frame was delivered with a selected route")?;
    anyhow::ensure!(
        force_relay == route.ends_with(" relay"),
        "selected unexpected route {route} (force_relay={force_relay})"
    );
    if check_tcp {
        connection.wait_port_mapping_ready().await?;
        test_tcp_forwarding(connection).await?;
    }
    Ok(route)
}

async fn test_viewing(
    controller: &AuthenticatedClient,
    device: &DeviceInfo,
    options: ConnectionMediaOptions,
    stop: &CancellationToken,
    assistance: Option<&Path>,
) -> Result<()> {
    let (room, connect_type) = if let Some(file) = assistance {
        let (id, code) = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                if let Ok(bytes) = std::fs::read(file)
                    && let Ok(credentials) = serde_json::from_slice::<(String, String)>(&bytes)
                {
                    break credentials;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .context("remote assistance ID was not announced")?;
        let mode = controller.assist_mode(&id).await?;
        anyhow::ensure!(mode.can_remote_control && mode.control_mode == "password_confirmation");
        let challenge = controller.assist_join(&id, &code).await?;
        anyhow::ensure!(
            challenge.code == 1136,
            "expected local confirmation (code {})",
            challenge.code
        );
        let challenge = challenge.data.context("missing confirmation challenge")?;
        let reply = controller
            .assist_confirm(&id, &challenge.control_id)
            .await?
            .into_data()?;
        reply.validate(&controller.device_id())?;
        anyhow::ensure!(reply.publisher_device_id == device.device_id);
        (
            RoomSession::from_assist(&reply, true),
            ControlConnectType::Assistance,
        )
    } else {
        (
            controller.join_device(&device.device_id, false).await?,
            ControlConnectType::Normal,
        )
    };
    let policy = controller
        .controlling_features()
        .policy(device.platform, &device.version_name);
    let mut connection = ControllerConnection::establish(
        room,
        None,
        controller,
        &controller.device_id(),
        options.resolve(LocalDisplayInfo::FALLBACK)?,
        policy,
        connect_type,
        options.transport,
        None,
        crate::media::audio::AudioSettings {
            volume: 0,
            muted: true,
        },
        None,
        stop,
        ControlPurpose::Viewing,
    )
    .await?;
    let force_relay = options.transport == crate::media::TransportChoice::Relay;
    let playback = check_video(&mut connection, force_relay, assistance.is_none()).await;
    let closed = connection.close().await;
    let route = playback?;
    closed?;
    if assistance.is_some() {
        println!("decoded H.264 on {route} after assistance password and local confirmation");
    } else {
        println!("decoded H.264 on {route} and verified TCP forwarding");
    }
    Ok(())
}

async fn run_test_controller(origin: url::Url, directory: &Path, force_relay: bool) -> Result<()> {
    let host_id = std::fs::read_to_string(directory.join("host-id"))?;
    let (_controller_identity, controller_runtime, controller) = client(origin)?;
    let stop = CancellationToken::new();

    let result = async {
        let device = tokio::time::timeout(Duration::from_secs(20), async {
            loop {
                let list = controller.list_devices().await?;
                if let Some(device) = list.my_binded_devices.into_iter().find(|device| {
                    device.device_id == host_id && device.is_connected() && device.controllable
                }) {
                    break Ok::<_, anyhow::Error>(device);
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
        .await
        .context("host did not publish its online state")??;
        let options = ConnectionMediaOptions {
            frame_rate: FrameRateChoice::Fps30,
            codec: CodecPreference::H264,
            muted: true,
            transport: if force_relay {
                crate::media::TransportChoice::Relay
            } else {
                crate::media::TransportChoice::P2p
            },
            ..Default::default()
        };
        test_viewing(&controller, &device, options, &stop, None).await?;
        test_file_transfer(directory, controller.clone(), device.clone(), options).await?;
        println!("verified file upload and download byte for byte");
        files::shutdown_all().await;
        test_viewing(
            &controller,
            &device,
            options,
            &stop,
            Some(&directory.join("assistance.json")),
        )
        .await?;
        Ok(())
    }
    .await;
    stop.cancel();
    files::shutdown_all().await;
    controller.close().await;
    controller_runtime.close().await;
    result
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires Node.js dependencies and an interactive Windows desktop with capture and decoding"]
async fn two_windows_peers_decode_video_through_self_hosted_signaling() -> Result<()> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();
    let force_relay = std::env::var_os("FARWING_TEST_FORCE_RELAY").is_some();
    if let Ok(role) = std::env::var("FARWING_TEST_PEER_ROLE") {
        let origin =
            crate::account::self_hosted::parse_origin(&std::env::var("FARWING_TEST_PEER_ORIGIN")?)?;
        let directory = PathBuf::from(
            std::env::var_os("FARWING_TEST_PEER_DIRECTORY").context("missing test directory")?,
        );
        return match role.as_str() {
            "host" => run_test_host(origin, &directory).await,
            "controller" => run_test_controller(origin, &directory, force_relay).await,
            _ => anyhow::bail!("invalid test peer role"),
        };
    }
    let (turn, ice_servers) = if force_relay {
        let (turn, ice_servers) = test_turn().await?;
        (Some(turn), ice_servers)
    } else {
        (None, serde_json::json!([]))
    };
    let (server, origin) = TestServer::start(&ice_servers).await?;
    let mut host = TestPeer::start(&origin, &server.directory, "host")?;
    host.wait_for_registration(&server.directory.join("host-id"))
        .await?;
    let result = TestPeer::start(&origin, &server.directory, "controller")?
        .wait(Duration::from_secs(120))
        .await;
    let host_result = host.stop().await;
    if let Some(turn) = turn {
        turn.close().await?;
    }
    result.and(host_result)
}
