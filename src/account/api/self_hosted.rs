use super::*;
use crate::account::self_hosted::SelfHosted;
use crate::session::local_host;

#[derive(Clone)]
pub(super) struct DesktopApi {
    client: reqwest::Client,
    settings: SelfHosted,
}

impl DesktopApi {
    pub(super) fn new(settings: SelfHosted) -> Result<Self> {
        Ok(Self {
            client: local_host::http_client()?,
            settings,
        })
    }

    pub(super) fn signaling_url(&self) -> Result<String> {
        let mut url = self.settings.origin.join("/socket.io/")?;
        let scheme = if url.scheme() == "https" { "wss" } else { "ws" };
        url.set_scheme(scheme)
            .map_err(|()| anyhow::anyhow!("自建服务器信令地址无效"))?;
        Ok(url.into())
    }

    pub(super) fn origin(&self) -> &url::Url {
        &self.settings.origin
    }

    pub(super) async fn send<T: DeserializeOwned>(
        &self,
        contract: Contract,
        path: &str,
        body: Vec<u8>,
    ) -> Result<ApiEnvelope<T>> {
        let method = match contract.method {
            Method::Get => HttpMethod::GET,
            Method::Post => HttpMethod::POST,
            Method::Put => HttpMethod::PUT,
            Method::Delete => HttpMethod::DELETE,
        };
        let url = self.settings.origin.join(path)?;
        anyhow::ensure!(
            url.origin() == self.settings.origin.origin(),
            "自建服务器 API 地址无效"
        );
        let mut request = self
            .client
            .request(method, url)
            .bearer_auth(&self.settings.token)
            .timeout(contract.timeout);
        if !body.is_empty() {
            request = request.header(CONTENT_TYPE, "application/json").body(body);
        }
        let response =
            crate::transport::http::send(crate::transport::http::Operation::Nrd, request).await?;
        let status = response.status();
        let envelope: ApiEnvelope<serde_json::Value> =
            serde_json::from_slice(&response.bytes().await?)
                .with_context(|| format!("自建服务器返回了无效的 API 响应（HTTP {status}）"))?;
        let code = if !status.is_success() && envelope.code == 0 {
            i32::from(status.as_u16())
        } else {
            envelope.code
        };
        let data = if code == 0 || (code == 1136 && assist::is_join(contract)) {
            envelope
                .data
                .map(serde_json::from_value)
                .transpose()
                .context("自建服务器返回了无效的业务数据")?
        } else {
            None
        };
        Ok(ApiEnvelope {
            code,
            msg: envelope.msg,
            data,
        })
    }
}
