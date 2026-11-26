//! HTTPS-only NRD endpoint recovery. Request semantics are defined in api.rs;
//! DNS, TLS and HTTP framing remain the HTTP library's responsibility.

use std::{
    error::Error as _,
    io,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result};
use bytes::Bytes;
use reqwest::{Client, Method, StatusCode, header::HeaderMap};
use tracing::Instrument;

pub(crate) const PRIMARY: &str = "https://api.nrd.nie.163.com";
const SECONDARY: &str = "https://api-dcdn.nrd.nie.163.com";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RetryPolicy {
    /// Queries and protocol replies which permit resending the same body.
    Replayable,
    /// Recover only when DNS/TCP/TLS failed before an HTTP request was sent.
    ConnectOnly,
}

#[derive(Default)]
struct Route {
    index: usize,
    generation: u64,
    requests: u32,
}

#[derive(Clone, Copy)]
struct RouteAttempt {
    index: usize,
    generation: u64,
}

#[derive(Clone)]
pub(crate) struct NrdHttp {
    client: Client,
    endpoints: Vec<String>,
    route: Arc<Mutex<Route>>,
}

impl NrdHttp {
    pub(crate) fn new() -> Result<Self> {
        // Only routing values are process-wide. The HTTP client/connection pool
        // is owned by the API caller, not leaked through a global client.
        static ROUTE: OnceLock<Arc<Mutex<Route>>> = OnceLock::new();
        Ok(Self {
            client: crate::transport::http_client()
                .https_only(true)
                .http1_only()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                // Leave time in the 30s API budget for another HTTPS endpoint
                // when DNS, TCP or TLS on the preferred route stops responding.
                .connect_timeout(Duration::from_secs(5))
                .gzip(true)
                .user_agent(concat!("OpenUUYC/", env!("CARGO_PKG_VERSION")))
                .build()
                .context("build NRD HTTPS client")?,
            endpoints: vec![PRIMARY.to_owned(), SECONDARY.to_owned()],
            route: Arc::clone(ROUTE.get_or_init(Default::default)),
        })
    }

    pub(crate) fn ipv4_route(&self) -> Self {
        Self {
            client: self.client.clone(),
            endpoints: vec!["https://api-ipv4.nrd.nie.163.com".into()],
            route: Arc::new(Mutex::new(Route::default())),
        }
    }

    pub(crate) async fn send(
        &self,
        method: Method,
        path: &str,
        headers: HeaderMap,
        body: Vec<u8>,
        timeout: Duration,
        retry: RetryPolicy,
    ) -> Result<(StatusCode, Bytes, HeaderMap)> {
        static NEXT_REQUEST: AtomicU64 = AtomicU64::new(1);
        let request_group = NEXT_REQUEST.fetch_add(1, Ordering::Relaxed);
        let deadline = Instant::now() + timeout;
        let mut selected = {
            let mut route = self
                .route
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            route.requests = route.requests.wrapping_add(1);
            if route.requests != 0 && route.requests.is_multiple_of(100) && route.index != 0 {
                route.index = 0;
                route.generation = route.generation.wrapping_add(1);
                tracing::info!("NRD HTTPS routing returned to primary endpoint");
            }
            RouteAttempt {
                index: route.index,
                generation: route.generation,
            }
        };
        let mut visited = vec![false; self.endpoints.len()];
        for attempt in 0..self.endpoints.len() {
            let index = selected.index;
            visited[index] = true;
            let endpoint = &self.endpoints[index];
            let mut request = self
                .client
                .request(method.clone(), format!("{endpoint}{path}"))
                .headers(headers.clone())
                .timeout(deadline.saturating_duration_since(Instant::now()));
            if !body.is_empty() || method == Method::POST || method == Method::PUT {
                request = request.body(body.clone());
            }
            let span = tracing::info_span!(
                "nrd_http",
                request_group,
                route_index = index,
                attempt,
                ?retry
            );
            let response =
                crate::transport::http::send(crate::transport::http::Operation::Nrd, request)
                    .instrument(span.clone())
                    .await;
            let outcome = match response {
                Ok(response) => {
                    let status = response.status();
                    let response_headers = response.headers().clone();
                    match response.bytes().instrument(span).await {
                        Ok(bytes) => {
                            if status == StatusCode::NOT_FOUND {
                                let next = self.failed(selected, &visited);
                                if retry == RetryPolicy::Replayable
                                    && Instant::now() < deadline
                                    && let Some(next) = next
                                {
                                    tracing::warn!(
                                        request_group,
                                        path,
                                        route_index = index,
                                        next_route = next.index,
                                        attempt,
                                        "NRD HTTPS 404; trying another endpoint"
                                    );
                                    selected = next;
                                    continue;
                                }
                            } else {
                                self.succeeded(selected);
                            }
                            return Ok((status, bytes, response_headers));
                        }
                        Err(error) => (error, false),
                    }
                }
                Err(error) => (error, true),
            };
            let (error, before_response) = outcome;
            if retryable_transport_error(&error, before_response) {
                // Repair the route for subsequent operations even when this
                // particular business operation cannot safely be sent again.
                let next = self.failed(selected, &visited);
                if (retry == RetryPolicy::Replayable || (before_response && error.is_connect()))
                    && Instant::now() < deadline
                    && let Some(next) = next
                {
                    tracing::warn!(
                        request_group,
                        path,
                        route_index = index,
                        next_route = next.index,
                        attempt,
                        "NRD HTTPS transport failed; trying another endpoint"
                    );
                    selected = next;
                    continue;
                }
            }
            return Err(error).with_context(|| {
                format!(
                    "NRD HTTPS request failed (route {index}, attempt {})",
                    attempt + 1
                )
            });
        }
        unreachable!("the last attempt always returns its response or error")
    }

    fn failed(&self, selected: RouteAttempt, visited: &[bool]) -> Option<RouteAttempt> {
        let mut route = self
            .route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let current = route.generation == selected.generation && route.index == selected.index;
        if current {
            route.index = (selected.index + 1) % self.endpoints.len();
            route.generation = route.generation.wrapping_add(1);
        }
        // A concurrent request may have found a working route. Prefer it, but
        // never revisit an endpoint already attempted by this logical request.
        (0..self.endpoints.len())
            .map(|offset| (route.index + offset) % self.endpoints.len())
            .find(|&index| !visited[index])
            .map(|index| RouteAttempt {
                index,
                // Once superseded, this operation may finish using another
                // endpoint, but must not republish an obsolete route decision.
                generation: if current {
                    route.generation
                } else {
                    selected.generation
                },
            })
    }

    fn succeeded(&self, selected: RouteAttempt) {
        let mut route = self
            .route
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if route.generation == selected.generation {
            route.index = selected.index;
            // Also protect success on the same endpoint from older failures.
            route.generation = route.generation.wrapping_add(1);
        }
    }
}

fn retryable_transport_error(error: &reqwest::Error, before_response: bool) -> bool {
    if error.is_connect() || error.is_timeout() {
        return true;
    }
    let mut source = error.source();
    while let Some(cause) = source {
        if let Some(http) = cause.downcast_ref::<hyper::Error>() {
            // An incomplete body is not an empty response. Never turn a body
            // parse/decompression failure into a replay of a state-changing POST.
            if http.is_parse() || http.is_user() {
                return false;
            }
            if before_response && (http.is_incomplete_message() || http.is_closed()) {
                return true;
            }
        }
        if let Some(io) = cause.downcast_ref::<io::Error>()
            && (matches!(
                io.kind(),
                io::ErrorKind::ConnectionReset
                    | io::ErrorKind::ConnectionAborted
                    | io::ErrorKind::BrokenPipe
                    | io::ErrorKind::NotConnected
            ) || (before_response && io.kind() == io::ErrorKind::UnexpectedEof))
        {
            return true;
        }
        source = cause.source();
    }
    false
}
