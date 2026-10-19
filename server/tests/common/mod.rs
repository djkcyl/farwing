//! In-process test server over real HTTPS/WSS with a self-signed certificate
//! that the test clients trust explicitly. Test devices hold an Ed25519
//! device key and sign every request like Farwing Client does.

#![allow(dead_code)]

use farwing_protocol::crypto::{self, DeviceKey};
use farwing_protocol::headers;
use farwing_server::{Config, RunningServer};
use futures_util::{SinkExt, StreamExt};
use reqwest::{Method, RequestBuilder, StatusCode};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode;

pub const PASSWORD: &str = "correct-horse-battery";
/// Protocol version sent in `X-Farwing-Protocol` and realtime envelopes.
pub const V: u32 = 2;

pub struct TestServer {
    pub server: Option<RunningServer>,
    pub cfg: Config,
    pub base: String,
    pub port: u16,
    pub http: reqwest::Client,
    pub tls: Arc<rustls::ClientConfig>,
    pub dir: tempfile::TempDir,
}

#[derive(Clone, Debug)]
pub struct Dev {
    pub id: String,
    pub key: Arc<DeviceKey>,
    pub access: Option<String>,
    pub guest: Option<String>,
    /// Simulated public address (sent as X-Forwarded-For when set).
    pub xff: Option<String>,
    /// Last signing timestamp: kept strictly increasing so that two equal
    /// requests never carry the same (replay-rejected) signature.
    last_ts: Arc<AtomicI64>,
}

impl Default for Dev {
    fn default() -> Self {
        Self {
            id: String::new(),
            key: Arc::new(DeviceKey::generate()),
            access: None,
            guest: None,
            xff: None,
            last_ts: Arc::new(AtomicI64::new(0)),
        }
    }
}

impl Dev {
    /// A fresh signing timestamp (Unix ms), unique for this device.
    pub fn timestamp(&self) -> i64 {
        let now = farwing_server::db::now_ms();
        let prev = self
            .last_ts
            .fetch_max(now, Ordering::SeqCst);
        if prev >= now {
            self.last_ts.fetch_add(1, Ordering::SeqCst) + 1
        } else {
            now
        }
    }

    pub fn bearer(&self) -> Option<&str> {
        self.access.as_deref().or(self.guest.as_deref())
    }
    pub fn public_key(&self) -> String {
        self.key.public_key()
    }
}

pub async fn start() -> TestServer {
    start_with(|_| {}).await
}

pub async fn start_with(customize: impl FnOnce(&mut Config)) -> TestServer {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_test_writer()
        .try_init();
    let dir = tempfile::tempdir().unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let data = dir
        .path()
        .join("data")
        .display()
        .to_string()
        .replace('\\', "/");
    let text = format!(
        r#"
listen = "127.0.0.1:0"
public_url = "https://127.0.0.1:{port}"
data_dir = "{data}"
instance_name = "test"
trusted_proxies = ["127.0.0.1/32"]

[tls]
self_signed = true
hostnames = ["127.0.0.1", "localhost"]

[turn]
mode = "external"
urls = ["stun:turn.test:3478", "turn:turn.test:3478?transport=udp", "turn:turn.test:3478?transport=tcp"]
shared_secret = "test-shared-secret"

[limits]
device_init_per_ip_hour = 100000
"#
    );
    let mut cfg = Config::from_toml_str(&text, dir.path()).unwrap();
    customize(&mut cfg);
    let server = farwing_server::start(cfg.clone(), Some(listener))
        .await
        .expect("server starts");

    // Trust exactly the generated self-signed certificate.
    let proxy = cfg.tls.mode == farwing_server::config::TlsMode::Proxy;
    let mut roots = rustls::RootCertStore::empty();
    if proxy {
        assert!(server.fingerprint.is_none());
    } else {
        let cert = CertificateDer::from_pem_file(cfg.tls_dir().join("cert.pem")).unwrap();
        let fingerprint = farwing_server::tls::fingerprint(cert.as_ref());
        assert_eq!(server.fingerprint.as_deref(), Some(fingerprint.as_str()));
        roots.add(cert).unwrap();
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let http = reqwest::Client::builder()
        .use_preconfigured_tls(tls.clone())
        .timeout(Duration::from_secs(90))
        .build()
        .unwrap();
    TestServer {
        server: Some(server),
        cfg,
        base: format!(
            "{}://127.0.0.1:{port}",
            if proxy { "http" } else { "https" }
        ),
        port,
        http,
        tls: Arc::new(tls),
        dir,
    }
}

pub struct Resp {
    pub status: StatusCode,
    pub headers: reqwest::header::HeaderMap,
    pub body: Value,
}

impl Resp {
    pub fn code(&self) -> &str {
        self.body["error"]["code"].as_str().unwrap_or("")
    }
    #[track_caller]
    pub fn ok(&self) -> &Value {
        assert!(
            self.status.is_success(),
            "unexpected {}: {}",
            self.status,
            self.body
        );
        &self.body
    }
    #[track_caller]
    pub fn err(&self, status: u16, code: &str) {
        assert_eq!(
            (self.status.as_u16(), self.code()),
            (status, code),
            "body: {}",
            self.body
        );
    }
}

/// Device policy used by test devices: hosts allow everything but Wake-on-LAN.
pub fn host_policy(host: bool) -> Value {
    json!({
        "control": host, "files": true, "ports": host, "power": host,
        "wol": false, "assist": host, "max_participants": 8
    })
}

impl TestServer {
    pub fn app(&self) -> &farwing_server::http::App {
        &self.server.as_ref().unwrap().app
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Request with the protocol header only.
    pub fn raw(&self, method: Method, path: &str) -> RequestBuilder {
        self.http
            .request(method, self.url(path))
            .header(headers::PROTOCOL, V.to_string())
    }

    /// Request signed by `dev` at `timestamp` over `body`, with its bearer
    /// token (when present).
    pub fn signed_at(
        &self,
        method: Method,
        path: &str,
        dev: &Dev,
        body: Vec<u8>,
        timestamp: i64,
    ) -> RequestBuilder {
        let signature = dev.key.sign(&crypto::request_message(
            method.as_str(),
            path,
            timestamp,
            &body,
        ));
        let mut b = self
            .raw(method, path)
            .header(headers::DEVICE_ID, &dev.id)
            .header(headers::TIMESTAMP, timestamp.to_string())
            .header(headers::SIGNATURE, signature);
        if let Some(t) = dev.bearer() {
            b = b.header("authorization", format!("Bearer {t}"));
        }
        if let Some(x) = &dev.xff {
            b = b.header("x-forwarded-for", x);
        }
        if !body.is_empty() {
            b = b.body(body);
        }
        b
    }

    /// Request signed by `dev` now.
    pub fn signed(&self, method: Method, path: &str, dev: &Dev, body: Vec<u8>) -> RequestBuilder {
        self.signed_at(method, path, dev, body, dev.timestamp())
    }

    /// Signed request without a body.
    pub fn req(&self, method: Method, path: &str, dev: &Dev) -> RequestBuilder {
        self.signed(method, path, dev, Vec::new())
    }

    pub async fn send(&self, b: RequestBuilder) -> Resp {
        let r = b.send().await.expect("request");
        let status = r.status();
        let headers = r.headers().clone();
        let bytes = r.bytes().await.unwrap();
        let body = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes)
                .unwrap_or(Value::String(String::from_utf8_lossy(&bytes).into()))
        };
        Resp {
            status,
            headers,
            body,
        }
    }

    pub async fn call(&self, method: Method, path: &str, dev: &Dev, body: Option<Value>) -> Resp {
        let b = match body {
            Some(v) => self
                .signed(method, path, dev, serde_json::to_vec(&v).unwrap())
                .header("content-type", "application/json"),
            None => self.req(method, path, dev),
        };
        self.send(b).await
    }

    pub async fn get(&self, path: &str, dev: &Dev) -> Resp {
        self.call(Method::GET, path, dev, None).await
    }

    pub async fn post(&self, path: &str, dev: &Dev, body: Value) -> Resp {
        self.call(Method::POST, path, dev, Some(body)).await
    }

    /// POST without a body (e.g. the presence ticket).
    pub async fn post_empty(&self, path: &str, dev: &Dev) -> Resp {
        self.call(Method::POST, path, dev, None).await
    }

    pub async fn put(&self, path: &str, dev: &Dev, body: Value) -> Resp {
        self.call(Method::PUT, path, dev, Some(body)).await
    }

    pub async fn delete(&self, path: &str, dev: &Dev) -> Resp {
        self.call(Method::DELETE, path, dev, None).await
    }

    pub fn create_user(&self, username: &str) -> String {
        farwing_server::admin::user_create(
            &self.cfg,
            username,
            Some(&format!("{username}-nick")),
            PASSWORD,
        )
        .unwrap()
    }

    /// `POST /devices/init` body for `key` with a fresh proof.
    pub fn init_body(key: &DeviceKey, name: &str, platform: &str, host: bool) -> Value {
        let timestamp = farwing_server::db::now_ms();
        let public_key = key.public_key();
        let proof = key.sign(&crypto::init_proof_message(&public_key, timestamp));
        let mut caps = vec!["files", "ports", "power", "wake_on_lan"];
        if host {
            caps.insert(0, "host");
            caps.push("assist");
        }
        json!({
            "installation_id": uuid::Uuid::new_v4().to_string(),
            "name": name,
            "platform": platform,
            "os": "Windows 11 Pro",
            "version": "0.1.0",
            "public_key": public_key,
            "timestamp": timestamp,
            "proof": proof,
            "capabilities": caps,
            "policy": host_policy(host),
            "hardware": {
                "cpu": "Test CPU", "base_board": "Test Board", "video": ["Test GPU"],
                "memory": 34359738368i64, "screen": "2560x1440", "mac": "00:11:22:33:44:55"
            },
        })
    }

    /// Register a new device (new key) on `platform`.
    pub async fn init_device(&self, name: &str, platform: &str, host: bool) -> Dev {
        let key = DeviceKey::generate();
        let body = Self::init_body(&key, name, platform, host);
        let r = self
            .send(self.raw(Method::POST, "/api/v2/devices/init").json(&body))
            .await;
        let v = r.ok();
        Dev {
            id: v["device_id"].as_str().unwrap().to_owned(),
            key: Arc::new(key),
            ..Dev::default()
        }
    }

    pub async fn login(&self, dev: &mut Dev, username: &str) {
        dev.access = None;
        let r = self
            .post(
                "/api/v2/auth/login",
                dev,
                json!({"username": username, "password": PASSWORD}),
            )
            .await;
        dev.access = Some(r.ok()["access_token"].as_str().unwrap().to_owned());
    }

    /// Device registered and logged in as `username`.
    pub async fn account_device(&self, username: &str, name: &str, host: bool) -> Dev {
        let mut d = self.init_device(name, "windows", host).await;
        self.login(&mut d, username).await;
        d
    }

    pub async fn guest_device(&self, name: &str) -> Dev {
        let mut d = self.init_device(name, "windows", true).await;
        let r = self.post_empty("/api/v2/guest/session", &d).await;
        d.guest = Some(r.ok()["guest_token"].as_str().unwrap().to_owned());
        d
    }

    /// Replace the device's policy.
    pub async fn set_policy(&self, dev: &Dev, policy: Value) {
        self.put("/api/v2/devices/self/policy", dev, policy)
            .await
            .ok();
    }

    pub async fn ws_connect(
        &self,
        header: &str,
        value: &str,
        xff: Option<&str>,
    ) -> Result<Ws, tokio_tungstenite::tungstenite::Error> {
        let url = format!("wss://127.0.0.1:{}/api/v2/ws", self.port);
        let mut req = url.into_client_request()?;
        req.headers_mut()
            .insert(headers::PROTOCOL, V.to_string().parse().unwrap());
        req.headers_mut().insert(
            reqwest::header::HeaderName::from_bytes(header.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
        if let Some(x) = xff {
            req.headers_mut()
                .insert("x-forwarded-for", x.parse().unwrap());
        }
        let tcp = TcpStream::connect(("127.0.0.1", self.port)).await.unwrap();
        let connector = TlsConnector::from(Arc::clone(&self.tls));
        let tls = connector
            .connect(ServerName::try_from("127.0.0.1").unwrap(), tcp)
            .await
            .unwrap();
        let (stream, _) = tokio_tungstenite::client_async(req, tls).await?;
        Ok(Ws { stream })
    }

    /// Open the device's presence connection and return it with its hello.
    pub async fn presence(&self, dev: &Dev) -> (Ws, Value) {
        let r = self.post_empty("/api/v2/presence/ticket", dev).await;
        let v = r.ok();
        assert_eq!(
            v["endpoints"][0].as_str().unwrap(),
            format!("wss://127.0.0.1:{}/api/v2/ws", self.port)
        );
        let ticket = v["ticket"].as_str().unwrap().to_owned();
        let mut ws = self
            .ws_connect(headers::TICKET, &ticket, dev.xff.as_deref())
            .await
            .expect("presence connects");
        let hello = ws.recv_type("hello").await;
        assert_eq!(hello["data"]["role"], "host");
        (ws, hello)
    }

    /// Wait until the hub sees the device online/offline.
    pub async fn wait_online(&self, device_id: &str, online: bool) {
        for _ in 0..200 {
            if self.app().hub.is_online(device_id) == online {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("device {device_id} online != {online}");
    }

    /// Create a session to `target` and connect its controller link; returns
    /// the link, the session ID and the `POST /sessions` response.
    pub async fn session_link(&self, ctrl: &Dev, target: &str, purpose: &str) -> (Ws, String, Value) {
        let r = self
            .post(
                "/api/v2/sessions",
                ctrl,
                json!({"target_device_id": target, "purpose": purpose}),
            )
            .await;
        let v = r.ok().clone();
        let sid = v["session_id"].as_str().unwrap().to_owned();
        let mut ws = self
            .ws_connect(headers::TICKET, v["ticket"].as_str().unwrap(), None)
            .await
            .expect("controller connects");
        let hello = ws.recv_type("hello").await;
        assert_eq!(hello["data"]["role"], "controller");
        assert_eq!(hello["data"]["session_id"], sid.as_str());
        (ws, sid, v)
    }

    /// Run a session up to `session.start`; return the session ID and the
    /// `session.incoming` and `ack` bodies (with their ICE servers).
    pub async fn ice_for_session(&self) -> (String, Value, Value) {
        self.create_user("alice");
        let host = self.account_device("alice", "HOST", true).await;
        let ctrl = self.account_device("alice", "CTRL", false).await;
        let (mut hw, _) = self.presence(&host).await;
        let (mut cw, sid, _) = self.session_link(&ctrl, &host.id, "desktop").await;
        cw.send(json!({"v": V, "type": "session.start", "id": "s", "data": {
            "attempt_id": "a", "options": {}
        }}))
        .await;
        let incoming = hw.recv_type("session.incoming").await;
        let ack = cw.recv_type("ack").await;
        (sid, incoming["data"].clone(), ack["data"].clone())
    }

    pub async fn shutdown(mut self) {
        if let Some(s) = self.server.take() {
            s.shutdown().await;
        }
    }
}

/// `session.start` frame with request id `id`.
pub fn start_frame(id: &str, sid: &str, attempt: &str) -> Value {
    json!({"v": V, "type": "session.start", "id": id, "sid": sid, "data": {
        "attempt_id": attempt,
        "options": {"screen_id": -1, "version": "0.1.0"},
        "capability": {"codecs": ["h265"]}
    }})
}

pub struct Ws {
    pub stream: WebSocketStream<TlsStream<TcpStream>>,
}

impl Ws {
    pub async fn send(&mut self, v: Value) {
        self.stream
            .send(Message::Text(v.to_string().into()))
            .await
            .expect("ws send");
    }

    /// Send without failing when the server already closed the connection.
    pub async fn try_send(&mut self, v: Value) -> bool {
        self.stream
            .send(Message::Text(v.to_string().into()))
            .await
            .is_ok()
    }

    pub async fn send_text(&mut self, t: &str) {
        self.stream
            .send(Message::Text(t.to_owned().into()))
            .await
            .expect("ws send");
    }

    /// Next JSON frame (server pings included), or `None` on close.
    pub async fn next(&mut self, wait: Duration) -> Option<Result<Value, Option<u16>>> {
        loop {
            let msg = tokio::time::timeout(wait, self.stream.next()).await.ok()?;
            match msg {
                Some(Ok(Message::Text(t))) => return Some(Ok(serde_json::from_str(&t).unwrap())),
                Some(Ok(Message::Close(frame))) => {
                    return Some(Err(frame.map(|f| u16::from(f.code))));
                }
                Some(Ok(_)) => continue,
                Some(Err(_)) | None => return Some(Err(None)),
            }
        }
    }

    /// Next frame of `kind`, skipping server pings and device/friend pushes.
    pub async fn recv_type(&mut self, kind: &str) -> Value {
        loop {
            match self.next(Duration::from_secs(10)).await {
                Some(Ok(v)) if v["type"] == kind => return v,
                Some(Ok(v))
                    if v["type"] == "ping"
                        || v["type"] == "device.changed"
                        || v["type"] == "friend.changed" =>
                {
                    continue;
                }
                Some(Ok(v)) => panic!("expected {kind}, got {v}"),
                Some(Err(code)) => panic!("expected {kind}, connection closed with {code:?}"),
                None => panic!("timed out waiting for {kind}"),
            }
        }
    }

    /// Next frame of `kind`; other frames (except pings) are returned too.
    pub async fn recv_until(&mut self, kind: &str) -> (Value, Vec<Value>) {
        let mut skipped = Vec::new();
        loop {
            match self.next(Duration::from_secs(10)).await {
                Some(Ok(v)) if v["type"] == kind => return (v, skipped),
                Some(Ok(v)) if v["type"] == "ping" => continue,
                Some(Ok(v)) => skipped.push(v),
                Some(Err(code)) => panic!("expected {kind}, connection closed with {code:?}"),
                None => panic!("timed out waiting for {kind}"),
            }
        }
    }

    /// Next frame matching `pred` (pings and other frames are skipped).
    pub async fn recv_matching(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        loop {
            match self.next(Duration::from_secs(10)).await {
                Some(Ok(v)) if pred(&v) => return v,
                Some(Ok(_)) => continue,
                Some(Err(code)) => panic!("expected {what}, connection closed with {code:?}"),
                None => panic!("timed out waiting for {what}"),
            }
        }
    }

    /// Next non-ping frame.
    pub async fn recv(&mut self) -> Value {
        loop {
            match self.next(Duration::from_secs(10)).await {
                Some(Ok(v)) if v["type"] == "ping" => continue,
                Some(Ok(v)) => return v,
                Some(Err(code)) => panic!("connection closed with {code:?}"),
                None => panic!("timed out waiting for a frame"),
            }
        }
    }

    /// Read until the close frame; return its code. Fails on other frames
    /// unless `skip` matches their type.
    pub async fn expect_close(&mut self, skip: &[&str]) -> Option<u16> {
        loop {
            match self.next(Duration::from_secs(10)).await {
                Some(Ok(v)) => {
                    let t = v["type"].as_str().unwrap_or("");
                    assert!(
                        t == "ping"
                            || t == "device.changed"
                            || t == "friend.changed"
                            || skip.contains(&t),
                        "unexpected frame before close: {v}"
                    );
                }
                Some(Err(code)) => return code,
                None => panic!("timed out waiting for close"),
            }
        }
    }

    /// No frame (other than pings and device pushes) arrives within `wait`.
    pub async fn expect_silence(&mut self, wait: Duration) {
        let deadline = tokio::time::Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                return;
            }
            match self.next(left).await {
                None => return,
                Some(Ok(v)) if v["type"] == "ping" || v["type"] == "device.changed" => continue,
                Some(other) => panic!("expected silence, got {other:?}"),
            }
        }
    }

    pub async fn close(mut self) {
        let _ = self
            .stream
            .close(Some(tokio_tungstenite::tungstenite::protocol::CloseFrame {
                code: CloseCode::Normal,
                reason: "".into(),
            }))
            .await;
    }

    /// Drop the TCP connection without a close handshake.
    pub fn abort(self) {
        drop(self);
    }
}

/// `v` envelope helper.
pub fn env(kind: &str, data: Value) -> Value {
    json!({"v": V, "type": kind, "data": data})
}
