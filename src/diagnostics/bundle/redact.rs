//! Export-only redaction. Originals are never rewritten; aliases are scoped to one bundle.
use regex::{Captures, Regex};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub(super) struct Redactor {
    salt: [u8; 16],
    secrets: Regex,
    fields: Regex,
    patterns: Vec<(&'static str, Regex)>,
    known: Vec<String>,
}
impl Redactor {
    pub fn new(mut known: Vec<String>) -> anyhow::Result<Self> {
        known.retain(|s| s.len() >= 3);
        known.sort_by_key(|s| std::cmp::Reverse(s.len()));
        known.dedup();
        Ok(Self {
            salt: *uuid::Uuid::new_v4().as_bytes(),
            known,
            secrets: Regex::new(
                r#"(?i)(?:password|passwd|verification[_-]?code|verify[_-]?code|auth[_-]?code|access[_-]?token|refresh[_-]?token|authorization|cookie|secret|private[_-]?key|ice[_-]?(?:pwd|ufrag)|credential|signature|(?:\b|_)sign\b|\btoken\b|\bbody\b|\bpayload\b|\bsdp\b|验证码|密码)"#,
            )?,
            fields: Regex::new(r#"\b([A-Za-z_][A-Za-z0-9_]*)=("(?:\\.|[^"\\])*"|[^\s,;]+)"#)?,
            patterns: vec![
                (
                    "url",
                    Regex::new(r#"(?i)\b(?:https?|wss?|ftp)://[^\s"<>]+"#)?,
                ),
                (
                    "path",
                    Regex::new(r#"(?i)(?:[a-z]:[\\/]|\\\\)[^\r\n"<>|]*"#)?,
                ),
                (
                    "email",
                    Regex::new(r"(?i)\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b")?,
                ),
                ("ip", Regex::new(r"\b(?:\d{1,3}\.){3}\d{1,3}\b")?),
                (
                    "ip",
                    Regex::new(r"(?i)(?:\b[0-9a-f]{1,4}:){2,}[0-9a-f:.%]*|::[0-9a-f:.%]+")?,
                ),
                (
                    "mac",
                    Regex::new(r"(?i)\b(?:[0-9a-f]{2}[:-]){5}[0-9a-f]{2}\b")?,
                ),
                (
                    "id",
                    Regex::new(
                        r"(?i)\b(?:[0-9a-f]{8}-[0-9a-f-]{27,}|[0-9a-f]{32,}|aeaw[a-z0-9]{12})\b",
                    )?,
                ),
                ("number", Regex::new(r"\b(?:1[3-9]\d{9}|\d{9})\b")?),
            ],
        })
    }
    fn alias(&self, kind: &str, value: &str) -> String {
        let mut h = Sha256::new();
        h.update(self.salt);
        h.update(value.as_bytes());
        let d = h.finalize();
        format!(
            "<{kind}:{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}>",
            d[0], d[1], d[2], d[3], d[4], d[5], d[6], d[7]
        )
    }
    pub fn text(&self, text: &str) -> String {
        let mut s = text.to_owned();
        for known in &self.known {
            s = s.replace(known, &self.alias("private", known));
        }
        // Fixed labels retain enum/model diagnostics; arbitrary quoted field values do not.
        s = self
            .fields
            .replace_all(&s, |c: &Captures<'_>| {
                let key = &c[1];
                let lower = key.to_ascii_lowercase();
                let v = &c[2];
                let identity = key.contains("device")
                    || key.contains("account")
                    || key.contains("client_id")
                    || key == "user"
                    || key == "alias"
                    || key == "name"
                    || key == "hostname"
                    || key == "peer"
                    || key.ends_with("_ip")
                    || lower.ends_with("_id")
                    || matches!(
                        lower.as_str(),
                        "path"
                            | "file"
                            | "filename"
                            | "directory"
                            | "phone"
                            | "email"
                            | "mac"
                            | "username"
                    );
                let opaque = v.starts_with('"')
                    && !matches!(
                        key,
                        "codec"
                            | "backend"
                            | "format"
                            | "state"
                            | "capture"
                            | "protocol"
                            | "status"
                            | "level"
                            | "reason"
                    );
                if identity || opaque {
                    format!("{key}={}", self.alias("value", v))
                } else {
                    c[0].to_owned()
                }
            })
            .into_owned();
        for (kind, re) in &self.patterns {
            s = re
                .replace_all(&s, |c: &Captures<'_>| {
                    if *kind == "ip" && c[0].parse::<std::net::IpAddr>().is_err() {
                        c[0].to_owned()
                    } else {
                        self.alias(kind, &c[0])
                    }
                })
                .into_owned();
        }
        s
    }
    pub fn line(&self, line: &str) -> String {
        if self.secrets.is_match(line) || line.contains("{\"") || line.contains(r#"{\""#) {
            // Raw protocol/authentication dumps are excluded as a whole, including unquoted tails.
            return "[已移除认证或原始协议内容]\n".into();
        }
        if !line.starts_with("session=") && !line.starts_with("pid=") {
            return "[已移除非结构化日志内容]\n".into();
        }
        let mut s = self.text(line);
        s.push('\n');
        s
    }
    pub fn json(&self, value: &mut Value) {
        match value {
            Value::Object(map) => {
                for (key, v) in map {
                    if self.secrets.is_match(key) {
                        *v = Value::String("<redacted>".into());
                    } else if matches!(
                        key.as_str(),
                        "device_id"
                            | "client_id"
                            | "account_id"
                            | "alias"
                            | "name"
                            | "device_name"
                            | "identity"
                            | "endpoint_id"
                    ) {
                        *v = Value::String(self.alias("id", &v.to_string()));
                    } else {
                        self.json(v);
                    }
                }
            }
            Value::Array(values) => {
                for v in values {
                    self.json(v);
                }
            }
            Value::String(s) => {
                *s = if self.secrets.is_match(s) {
                    "<redacted>".into()
                } else {
                    self.text(s)
                }
            }
            _ => {}
        }
    }
}
