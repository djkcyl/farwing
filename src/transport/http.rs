//! One HTTP request/response log for every caller, with bounded body capture.
//! Request semantics, timeouts, body limits and retries remain with the caller.
use bytes::{Bytes, BytesMut};
use reqwest::{Method, RequestBuilder, StatusCode, header::HeaderMap};
use std::collections::VecDeque;
use std::{
    cell::Cell,
    error::Error as _,
    sync::{
        LazyLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Instant,
};

const BODY_LIMIT: usize = 16 * 1024;

#[derive(Clone, Copy, Debug)]
pub(crate) enum Operation {
    Nrd,
    Update,
    WallpaperDownload,
    WallpaperUpload,
    WallpaperVerify,
}

fn credential(name: &str) -> bool {
    let name = name.to_ascii_lowercase().replace(['-', '_'], "");
    matches!(
        name.as_str(),
        "authorization"
            | "proxyauthorization"
            | "cookie"
            | "setcookie"
            | "password"
            | "passwd"
            | "secret"
            | "clientsecret"
            | "privatekey"
            | "sign"
            | "backupsign"
            | "signature"
            | "xparamsign"
            | "token"
            | "ticket"
            | "accesstoken"
            | "refreshtoken"
            | "verificationcode"
            | "verifycode"
            | "connectcode"
            | "authcode"
            | "smscode"
            | "captcha"
            | "qrcode"
            | "credential"
            | "icepwd"
            | "iceufrag"
    )
}

#[derive(Default)]
struct Redaction {
    values: Vec<String>,
}
impl Redaction {
    fn remember(&mut self, value: &str) {
        if value.len() >= 3 && !self.values.iter().any(|v| v == value) {
            self.values.push(value.to_owned());
        }
    }
    fn json(&mut self, value: &mut serde_json::Value, request: bool) {
        match value {
            serde_json::Value::Object(map) => {
                for (key, value) in map {
                    if credential(key) || (request && key.eq_ignore_ascii_case("code")) {
                        if let Some(text) = value.as_str() {
                            self.remember(text);
                        } else if value.is_number() {
                            self.remember(&value.to_string());
                        }
                        *value = serde_json::Value::String("<redacted>".into());
                    } else {
                        self.json(value, request);
                    }
                }
            }
            serde_json::Value::Array(items) => {
                for value in items {
                    self.json(value, request);
                }
            }
            _ => (),
        }
    }
    fn headers(&mut self, headers: &HeaderMap) -> String {
        let mut values = Vec::new();
        for (name, value) in headers {
            let sensitive = value.is_sensitive();
            let value = value.to_str().unwrap_or("<non-text header>");
            if credential(name.as_str()) || sensitive {
                self.remember(value);
                if name.as_str().ends_with("authorization") {
                    if let Some((_, secret)) = value.split_once(' ') {
                        self.remember(secret);
                    }
                }
                values.push((name.as_str(), "<redacted>".to_owned()));
            } else {
                values.push((name.as_str(), self.text(value)));
            }
        }
        serde_json::to_string(&values).unwrap_or_default()
    }
    fn url(&mut self, url: &reqwest::Url) -> String {
        self.remember(url.username());
        if let Some(value) = url.password() {
            self.remember(value);
        }
        let mut safe = url.clone();
        let _ = safe.set_username("");
        let _ = safe.set_password(None);
        safe.set_fragment(None);
        if url.query().is_some() {
            let pairs: Vec<_> = url
                .query_pairs()
                .map(|(key, value)| {
                    if credential(&key) || key.eq_ignore_ascii_case("code") {
                        self.remember(&value);
                        (key.into_owned(), "<redacted>".to_owned())
                    } else {
                        (key.into_owned(), value.into_owned())
                    }
                })
                .collect();
            safe.query_pairs_mut().clear().extend_pairs(pairs);
        }
        safe.to_string()
    }
    fn text(&self, text: &str) -> String {
        // Covers a credential echoed inside an otherwise ordinary server message.
        let mut text = text.to_owned();
        for secret in &self.values {
            text = text.replace(secret, "<redacted>");
        }
        static FIELDS: LazyLock<regex::Regex> = LazyLock::new(|| {
            regex::Regex::new(
            r#"(?i)([\"']?\b(?:password|passwd|access_token|refresh_token|token|ticket|secret|signature|sign|backup_sign|verification_code|verify_code|connect_code|auth_code|authorization|cookie)[\"']?\s*[:=]\s*)(?:\"[^\"]*(?:\"|$)|'[^']*(?:'|$)|[^\s,;}]+)"#).unwrap()
        });
        FIELDS.replace_all(&text, "$1\"<redacted>\"").into_owned()
    }
    fn body(&mut self, bytes: &[u8], request: bool) -> String {
        let bytes = &bytes[..bytes.len().min(BODY_LIMIT)];
        let text = match std::str::from_utf8(bytes) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&bytes[..error.valid_up_to()]).unwrap()
            }
            Err(_) => return "<binary body>".into(),
        };
        if text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\r' | '\n' | '\t'))
        {
            return "<binary body>".into();
        }
        if let Ok(mut value) = serde_json::from_str::<serde_json::Value>(text) {
            self.json(&mut value, request);
            self.text(&value.to_string())
        } else {
            self.text(text)
        }
    }
}

struct Observation {
    operation: Operation,
    sequence: u64,
    started: Instant,
    method: Option<Method>,
    target: String,
    redaction: Redaction,
    sending: Cell<bool>,
}
impl Observation {
    fn failure(&self, stage: &'static str, error: reqwest::Error) -> reqwest::Error {
        self.sending.set(false);
        let error = error.without_url();
        let mut causes = vec![error.to_string()];
        let mut source = error.source();
        while let Some(cause) = source {
            if causes.len() >= 8 {
                break;
            }
            causes.push(cause.to_string());
            source = cause.source();
        }
        let reason = self.redaction.text(&causes.join(" -> "));
        tracing::warn!(operation=?self.operation, request_seq=self.sequence, method=?self.method,
            target=%self.target, stage, elapsed_ms=self.started.elapsed().as_millis(),
            reason=?reason, "HTTP request failed");
        error
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        if self.sending.get() {
            tracing::info!(operation=?self.operation,request_seq=self.sequence,method=?self.method,
                target=%self.target,elapsed_ms=self.started.elapsed().as_millis(),
                "HTTP request cancelled before response headers");
        }
    }
}

pub(crate) struct Response {
    inner: reqwest::Response,
    observation: Observation,
    preview: Vec<u8>,
    received: u64,
    complete: bool,
    buffered: VecDeque<Bytes>,
    pending_error: Option<reqwest::Error>,
}

pub(crate) async fn send(
    operation: Operation,
    builder: RequestBuilder,
) -> reqwest::Result<Response> {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let mut observation = Observation {
        operation,
        sequence: NEXT.fetch_add(1, Ordering::Relaxed),
        started: Instant::now(),
        method: None,
        target: String::new(),
        redaction: Redaction::default(),
        sending: Cell::new(true),
    };
    let (client, request) = builder.build_split();
    let request = request.map_err(|e| observation.failure("build", e))?;
    observation.method = Some(request.method().clone());
    observation.target = observation.redaction.url(request.url());
    let headers = observation.redaction.headers(request.headers());
    let bytes = request.body().and_then(|b| b.as_bytes());
    let body = bytes
        .map(|b| observation.redaction.body(b, true))
        .unwrap_or_else(|| {
            if request.body().is_some() {
                "<streamed request body>".into()
            } else {
                String::new()
            }
        });
    tracing::info!(?operation,request_seq=observation.sequence,method=?observation.method,
        target=%observation.target,http_headers=%headers,request_body=?body,
        body_bytes=bytes.map(|b|b.len()),truncated=bytes.is_some_and(|b|b.len()>BODY_LIMIT),
        "HTTP request");
    let inner = client
        .execute(request)
        .await
        .map_err(|e| observation.failure("send", e))?;
    observation.sending.set(false);
    let headers = observation.redaction.headers(inner.headers());
    tracing::info!(?operation,request_seq=observation.sequence,method=?observation.method,
        target=%observation.target,http_status=inner.status().as_u16(),http_headers=%headers,
        elapsed_ms=observation.started.elapsed().as_millis(),"HTTP response headers");
    let mut response = Response {
        inner,
        observation,
        preview: Vec::new(),
        received: 0,
        complete: false,
        buffered: VecDeque::new(),
        pending_error: None,
    };
    // Error status callers may return without reading a body. Capture a bounded
    // prefix now and replay it to readers, preserving both status and read errors.
    if response.status().is_client_error() || response.status().is_server_error() {
        while response.preview.len() < BODY_LIMIT && !response.complete {
            match response.read_chunk().await {
                Ok(Some(chunk)) => response.buffered.push_back(chunk),
                Ok(None) => break,
                Err(error) => {
                    response.pending_error = Some(error);
                    break;
                }
            }
        }
    }
    Ok(response)
}

impl Response {
    pub(crate) fn status(&self) -> StatusCode {
        self.inner.status()
    }
    pub(crate) fn headers(&self) -> &HeaderMap {
        self.inner.headers()
    }
    pub(crate) fn content_length(&self) -> Option<u64> {
        self.inner.content_length()
    }
    pub(crate) fn error_for_status_ref(&self) -> reqwest::Result<&Self> {
        self.inner
            .error_for_status_ref()
            .map_err(|e| self.observation.failure("status", e))?;
        Ok(self)
    }
    pub(crate) fn error_for_status(self) -> reqwest::Result<Self> {
        self.error_for_status_ref()?;
        Ok(self)
    }
    pub(crate) async fn bytes(mut self) -> reqwest::Result<Bytes> {
        let mut body = BytesMut::new();
        while let Some(chunk) = self.chunk().await? {
            body.extend_from_slice(&chunk);
        }
        Ok(body.freeze())
    }
    pub(crate) async fn chunk(&mut self) -> reqwest::Result<Option<Bytes>> {
        if let Some(bytes) = self.buffered.pop_front() {
            return Ok(Some(bytes));
        }
        if let Some(error) = self.pending_error.take() {
            return Err(error);
        }
        if self.complete {
            return Ok(None);
        }
        self.read_chunk().await
    }
    async fn read_chunk(&mut self) -> reqwest::Result<Option<Bytes>> {
        let chunk = self
            .inner
            .chunk()
            .await
            .map_err(|e| self.observation.failure("body", e))?;
        if let Some(bytes) = &chunk {
            self.received = self.received.saturating_add(bytes.len() as u64);
            let keep = bytes.len().min(BODY_LIMIT - self.preview.len());
            self.preview.extend_from_slice(&bytes[..keep]);
        } else {
            self.complete = true;
        }
        Ok(chunk)
    }
}
impl Drop for Response {
    fn drop(&mut self) {
        let body = self.observation.redaction.body(&self.preview, false);
        tracing::info!(operation=?self.observation.operation,request_seq=self.observation.sequence,
            method=?self.observation.method,target=%self.observation.target,
            http_status=self.inner.status().as_u16(),elapsed_ms=self.observation.started.elapsed().as_millis(),
            response_body=?body,received_bytes=self.received,complete=self.complete,
            truncated=self.received>self.preview.len() as u64 || (!self.complete && self.preview.len()==BODY_LIMIT),"HTTP response body");
    }
}
