//! Account rights returned by the 4.48.1 endpoint, with nonce-bound server authentication.
use super::*;
use crate::protocol::entitlements::{Schedule, Snapshot};
use anyhow::ensure;
use base64::Engine as _;
use std::collections::BTreeMap;

const PATH: &str = "/api/v1/member/entitlements";
// Public verification key embedded in the official 4.48.1 HTTP response verifier.
const PUBLIC_KEY: &str = "MIIBCgKCAQEAxFwOA5SFhnnn3AbEa9IJt3dd7wuyMj3vuowmxM6xHogzjFY96OuwUmyGaBKd91nZla5/lbUZ/6rnsOQGo3TqBbiA4arexZULy129zwvTsA0jz6m6h/xjIxho3YkmAHKcK+AuGXPsy3r9fIqAqRx/VGB5vRtnJ0Hz/K1BZIUJgQAm0zhwqjPPTpQSgi7kWm61sNXwIrsYwetf1sZm0KEevUeBYZvkHCdFvTb0NqsS45VR+XaczUScssMdwkRJSIV8MAjdvx0ldM8u15HdynNUN7zFMuR7buqigCdJAhuVp+ZUFcXT34W05neUR8M4743qVU2Gn7HgpvTvQlyTzDYCGwIDAQAB";

#[derive(Deserialize)]
struct Table {
    user_id: String,
    platform: i32,
    version: u64,
    expired_at: i64,
    checkpoint_at: Option<i64>,
    features: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone)]
pub(crate) struct Entitlements {
    pub version: u64,
    pub expired_at: i64,
    pub checkpoint_at: Option<i64>,
    pub snapshot: Snapshot,
}

impl Entitlements {
    pub fn allows(&self, feature: &str, value: Option<serde_json::Value>) -> bool {
        let now = self.server_time();
        if self.checkpoint_at.is_none_or(|t| now < t) || now >= self.expired_at {
            return true;
        }
        self.snapshot.allows(feature, value)
    }
    pub fn server_time(&self) -> i64 {
        self.snapshot.server_time()
    }
    pub fn refresh_after(&self) -> Duration {
        Duration::from_secs(self.expired_at.saturating_sub(self.server_time()).max(0) as u64)
    }
}

impl NrdApi {
    pub(crate) async fn query_entitlements(&self) -> Result<Entitlements> {
        let api = self;
        let user = api.user_id.as_deref().context("权益查询需要登录账号")?;
        let authorization = api.authorization.clone().context("权益查询缺少账号凭据")?;
        let nonce = uuid::Uuid::new_v4().simple().to_string();
        let mut headers = api.signed_headers_with_nonce(
            "GET",
            PATH,
            &[],
            IdentityScope::AccountDevice,
            Some(&nonce),
        )?;
        headers.insert(AUTHORIZATION, authorization);
        let (status, bytes, headers) = api
            .official_http()?
            .send(
                HttpMethod::GET,
                PATH,
                headers,
                Vec::new(),
                Duration::from_secs(3),
                RetryPolicy::Replayable,
            )
            .await?;
        let envelope: ApiEnvelope<serde_json::Value> =
            serde_json::from_slice(&bytes).context("权益响应不是有效 JSON")?;
        if status != reqwest::StatusCode::OK || envelope.code != 0 {
            return Err(ApiFailure {
                code: if envelope.code != 0 {
                    envelope.code
                } else {
                    i32::from(status.as_u16())
                },
                message: format!("权益查询 HTTP {}，{}", status.as_u16(), envelope.msg),
            }
            .into());
        }
        verify_response(&headers, &bytes, &nonce)?;
        let server_time = headers
            .get("x-param-ts")
            .and_then(|h| h.to_str().ok())
            .and_then(|s| s.parse::<i64>().ok())
            .filter(|t| *t > 0)
            .context("权益响应缺少有效服务器时间")?;
        let table: Table = serde_json::from_value(envelope.data.context("权益响应缺少 data")?)?;
        ensure!(
            table.user_id == user && table.platform == 1,
            "权益响应账号或平台不匹配"
        );
        ensure!(table.expired_at > server_time, "权益表已过期");
        ensure!(
            !table.features.is_empty() && table.features.len() <= 512,
            "权益表功能数量无效"
        );
        Ok(Entitlements {
            version: table.version,
            expired_at: table.expired_at,
            checkpoint_at: table.checkpoint_at.filter(|t| *t > 0),
            snapshot: Snapshot::new(Schedule::from_features(table.features)?, server_time)?,
        })
    }
}

fn verify_response(headers: &HeaderMap, body: &[u8], nonce: &str) -> Result<()> {
    ensure!(
        headers.get("x-param-nonce").and_then(|v| v.to_str().ok()) == Some(nonce),
        "权益响应 nonce 不匹配"
    );
    let signature = headers
        .get("x-param-sign")
        .context("权益响应缺少签名")?
        .to_str()?;
    ensure!(
        signature.len() == 512 && signature.bytes().all(|b| b.is_ascii_hexdigit()),
        "权益响应签名编码无效"
    );
    let signature = (0..signature.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&signature[i..i + 2], 16))
        .collect::<Result<Vec<_>, _>>()?;
    let mut signed = BTreeMap::new();
    for (key, value) in headers {
        if key.as_str().starts_with("x-param-") && key != "x-param-sign" {
            ensure!(
                signed.insert(key.as_str(), value.to_str()?).is_none(),
                "权益响应签名头重复"
            );
        }
    }
    let mut data = signed
        .into_iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
        .into_bytes();
    data.extend_from_slice(body);
    let key = base64::engine::general_purpose::STANDARD.decode(PUBLIC_KEY)?;
    ring::signature::UnparsedPublicKey::new(
        &ring::signature::RSA_PKCS1_2048_8192_SHA1_FOR_LEGACY_USE_ONLY,
        key,
    )
    .verify(&data, &signature)
    .map_err(|_| anyhow::anyhow!("权益响应签名校验失败"))
}
