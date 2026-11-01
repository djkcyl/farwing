//! HTTPS transport to the configured Farwing Server.
//!
//! Request semantics (paths, bodies, errors) belong to `api`. This module owns
//! the TLS trust decision and the shared connection pool for one server.

use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use reqwest::{Client, Method, StatusCode, header::HeaderMap};

use crate::account::server::ServerConfig;

#[derive(Clone)]
pub(crate) struct ServerHttp {
    client: Client,
    server: ServerConfig,
}

pub(crate) struct Response {
    pub status: StatusCode,
    pub body: Bytes,
}

impl ServerHttp {
    pub(crate) fn new(server: &ServerConfig) -> Result<Self> {
        // One pool per configured server. Building a client loads the system
        // roots, so reuse it while the server setting is unchanged.
        static CACHE: Mutex<Option<(ServerConfig, Client)>> = Mutex::new(None);
        let mut cache = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some((cached, client)) = cache.as_ref()
            && cached == server
        {
            return Ok(Self {
                client: client.clone(),
                server: server.clone(),
            });
        }
        let tls = server.tls(true)?;
        let client = crate::transport::http_client()
            .tls_backend_preconfigured((*tls).clone())
            .https_only(true)
            .http1_only()
            .redirect(reqwest::redirect::Policy::none())
            .gzip(true)
            .connect_timeout(Duration::from_secs(10))
            .user_agent(concat!(
                "Farwing/",
                env!("CARGO_PKG_VERSION"),
                " (windows; x86_64)"
            ))
            .build()
            .context(crate::i18n::t().session.https_client_failed)?;
        *cache = Some((server.clone(), client.clone()));
        Ok(Self {
            client,
            server: server.clone(),
        })
    }

    pub(crate) fn server(&self) -> &ServerConfig {
        &self.server
    }

    /// Stream `path` into `dest` (a temporary file renamed when complete).
    pub(crate) async fn download(&self, path: &str, headers: HeaderMap, dest: &std::path::Path) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        let mut response = self
            .client
            .get(self.server.url(path))
            .headers(headers)
            .timeout(Duration::from_secs(60 * 60))
            .send()
            .await
            .context("download")?;
        anyhow::ensure!(response.status().is_success(), "HTTP {}", response.status().as_u16());
        let part = dest.with_extension("part");
        let mut file = tokio::fs::File::create(&part).await.context("download file")?;
        while let Some(chunk) = response.chunk().await.context("download")? {
            file.write_all(&chunk).await.context("download file")?;
        }
        file.flush().await?;
        drop(file);
        tokio::fs::rename(&part, dest).await.context("download file")?;
        Ok(())
    }

    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Option<(Bytes, &'static str)>,
        timeout: Duration,
    ) -> Result<Response, reqwest::Error> {
        // The server moved since this client was built.
        let moved = self.server.current().and_then(|server| Self::new(&server).ok());
        let http = moved.as_ref().unwrap_or(self);
        let mut request = http
            .client
            .request(method.clone(), http.server.url(path))
            .headers(headers)
            .timeout(timeout);
        if let Some((body, content_type)) = body {
            request = request
                .header(reqwest::header::CONTENT_TYPE, content_type)
                .body(body);
        }
        tracing::trace!(path, %method, "sending Farwing HTTPS request");
        let response = request.send().await?;
        let status = response.status();
        let body = response.bytes().await?;
        Ok(Response { status, body })
    }
}
