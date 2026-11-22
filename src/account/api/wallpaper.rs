//! GameViewerServer: ABD3E0 -> ACADA0 -> B27100.
use super::*;

pub(super) const TOKEN: Contract = Contract {
    path: "/api/v1/tool/fp/token?filetype=1",
    ..DEVICE_LIST
};
pub(super) const BIND: Contract = Contract {
    retry: RetryPolicy::ConnectOnly,
    path: "/api/v1/device/wallpaper",
    ..ROOM_CREATE
};

// Deliberately no Debug: the upload grant is a credential.
#[derive(Deserialize)]
pub(crate) struct WallpaperGrant {
    pub token: String,
    pub req_url: String,
    pub ttl: u64,
    #[serde(default)]
    pub max_size: u64,
}

#[derive(Deserialize)]
pub(crate) struct UploadedWallpaper {
    pub url: String,
    pub fsize: u64,
    pub md5: String,
    pub mime: String,
}

impl NrdApi {
    pub(crate) async fn wallpaper_grant(&self) -> Result<WallpaperGrant> {
        self.send::<WallpaperGrant>(TOKEN, TOKEN.path, Vec::new())
            .await?
            .into_data()
    }

    pub(crate) async fn bind_wallpaper(&self, wallpaper_url: &str) -> Result<()> {
        let response = self
            .post_json::<_, serde_json::Value>(
                BIND,
                BIND.path,
                &serde_json::json!({ "wallpaper_url": wallpaper_url }),
            )
            .await?;
        if response.code != 0 {
            bail!("wallpaper binding rejected (code {})", response.code);
        }
        Ok(())
    }

    pub(crate) async fn upload_wallpaper(
        &self,
        grant: WallpaperGrant,
        image: bytes::Bytes,
    ) -> Result<UploadedWallpaper> {
        let origin = match &self.backend {
            Backend::SelfHosted(api) => Some(api.origin()),
            Backend::Official(_) => None,
        };
        upload_wallpaper(grant, image, origin).await
    }
}

fn local_wallpaper_url(origin: &reqwest::Url, url: &reqwest::Url) -> bool {
    let loopback_pair = origin.host_str().zip(url.host_str()).is_some_and(|(a, b)| {
        crate::transport::signal::is_loopback_host(a) && crate::transport::signal::is_loopback_host(b)
    });
    (url.origin() == origin.origin() || (loopback_pair && url.scheme() == origin.scheme()
        && url.port_or_known_default() == origin.port_or_known_default()))
        && url.username().is_empty() && url.password().is_none()
        && url.query().is_none() && url.fragment().is_none()
}

/// Raw bytes and the file grant only: no NRD identity, signature or user token.
async fn upload_wallpaper(
    grant: WallpaperGrant,
    image: bytes::Bytes,
    self_hosted: Option<&reqwest::Url>,
) -> Result<UploadedWallpaper> {
    let url = reqwest::Url::parse(&grant.req_url)
        .map_err(|_| anyhow::anyhow!("invalid wallpaper upload URL"))?;
    let allowed = match self_hosted {
        Some(origin) => local_wallpaper_url(origin, &url) && url.path() == "/v1/wallpapers/upload",
        None => url.scheme() == "https" && url.host_str() == Some("fp.ps.netease.com")
            && url.username().is_empty() && url.password().is_none(),
    };
    anyhow::ensure!(allowed, "invalid wallpaper upload endpoint");
    if grant.ttl == 0 || grant.token.is_empty() {
        bail!("wallpaper upload grant is empty or expired");
    }
    let limit = if grant.max_size == 0 {
        100 * 1024 * 1024
    } else {
        grant.max_size
    };
    if image.is_empty() || image.len() as u64 >= limit || image.len() > 10 * 1024 * 1024 {
        bail!("wallpaper exceeds upload size limit");
    }
    let mut authorization = HeaderValue::from_str(&grant.token)
        .map_err(|_| anyhow::anyhow!("invalid wallpaper upload token"))?;
    authorization.set_sensitive(true);
    let client = if self_hosted.is_some() {
        crate::session::local_host::http_client()?
    } else {
        crate::transport::http_client()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .build()?
    };
    let timeout = Duration::from_secs(30).min(Duration::from_secs(grant.ttl));
    let mut response = crate::transport::http::send(
        crate::transport::http::Operation::WallpaperUpload,
        client
            .post(url)
            .header(AUTHORIZATION, authorization)
            .timeout(timeout)
            .body(image.clone()),
    )
    .await
    .map_err(|_| anyhow::anyhow!("wallpaper file upload transport failed"))?;
    if response.status() != reqwest::StatusCode::OK {
        bail!(
            "wallpaper file upload rejected (HTTP {})",
            response.status().as_u16()
        );
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("wallpaper upload response interrupted"))?
    {
        if bytes.len() + chunk.len() > 64 * 1024 {
            bail!("wallpaper upload response too large");
        }
        bytes.extend_from_slice(&chunk);
    }
    let mut uploaded: UploadedWallpaper = serde_json::from_slice(&bytes)
        .map_err(|_| anyhow::anyhow!("invalid wallpaper upload response"))?;
    let mut url = reqwest::Url::parse(&uploaded.url)
        .map_err(|_| anyhow::anyhow!("invalid uploaded wallpaper URL"))?;
    tracing::debug!(
        scheme = url.scheme(),
        file_size = uploaded.fsize,
        digest_length = uploaded.md5.len(),
        mime_length = uploaded.mime.len(),
        "wallpaper upload metadata received"
    );
    if !matches!(url.scheme(), "http" | "https")
        || url.host_str().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || uploaded.fsize == 0
        || uploaded.fsize > 10 * 1024 * 1024
        || uploaded.md5.is_empty()
        || uploaded.mime.is_empty()
    {
        bail!("invalid uploaded wallpaper metadata");
    }
    if let Some(origin) = self_hosted {
        let id = url.path().strip_prefix("/v1/wallpapers/").and_then(|id| uuid::Uuid::parse_str(id).ok());
        anyhow::ensure!(local_wallpaper_url(origin, &url) && id.is_some(), "invalid self-hosted wallpaper publication URL");
    } else {
        // Some Filepicker deployments return an HTTP publication URL.
        url.set_scheme("https")
            .map_err(|_| anyhow::anyhow!("invalid wallpaper publication scheme"))?;
    }
    let mut verification = crate::transport::http::send(
        crate::transport::http::Operation::WallpaperVerify,
        client.get(url.clone()).timeout(timeout),
    )
    .await
    .map_err(|_| anyhow::anyhow!("uploaded wallpaper HTTPS verification failed"))?;
    if verification.status() != reqwest::StatusCode::OK {
        bail!("uploaded wallpaper HTTPS verification rejected");
    }
    let mut offset = 0;
    while let Some(chunk) = verification
        .chunk()
        .await
        .map_err(|_| anyhow::anyhow!("wallpaper verification interrupted"))?
    {
        let end = offset + chunk.len();
        if end > image.len() || image[offset..end] != chunk[..] {
            bail!("uploaded wallpaper bytes differ from the approved image");
        }
        offset = end;
    }
    if offset != image.len() {
        bail!("uploaded wallpaper is incomplete");
    }
    uploaded.url = url.into();
    Ok(uploaded)
}
