//! HTTPS error responses (`{"error": {...}}`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use farwing_protocol::{ErrorBody, ErrorCode, ErrorEnvelope};
use serde_json::{Value, json};

/// A failed request. `message` is a short English text for logs and
/// `curl`; clients show their own text for `code` and `details.reason`.
#[derive(Debug)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub details: Value,
}

pub type ApiResult<T> = Result<T, ApiError>;

impl ApiError {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            details: Value::Null,
        }
    }

    /// One of several cases of `code` (`farwing_protocol::error::reason`).
    pub fn reason(code: ErrorCode, reason: &'static str, message: impl Into<String>) -> Self {
        Self::new(code, message).with_detail("reason", reason)
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    /// Add one `details` field (a parameter of the client's text).
    pub fn with_detail(mut self, key: &str, value: impl Into<Value>) -> Self {
        if !self.details.is_object() {
            self.details = json!({});
        }
        if let Some(map) = self.details.as_object_mut() {
            map.insert(key.to_owned(), value.into());
        }
        self
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, msg)
    }
    pub fn unauthorized() -> Self {
        Self::new(ErrorCode::Unauthorized, "missing or invalid access token")
    }
    pub fn device_unauthorized() -> Self {
        Self::new(ErrorCode::DeviceUnauthorized, "invalid device signature")
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, msg)
    }
    pub fn not_found() -> Self {
        Self::new(ErrorCode::NotFound, "not found")
    }
    pub fn device_not_found() -> Self {
        Self::new(ErrorCode::DeviceNotFound, "device not found")
    }
    pub fn device_offline() -> Self {
        Self::new(ErrorCode::DeviceOffline, "target device is offline")
    }
    pub fn internal() -> Self {
        Self::new(ErrorCode::Internal, "internal server error")
    }
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        Self::new(ErrorCode::RateLimited, "too many requests")
            .with_details(json!({ "retry_after_ms": retry_after_ms }))
    }
    pub fn payload_too_large() -> Self {
        Self::new(ErrorCode::PayloadTooLarge, "request body too large")
    }
    pub fn timeout() -> Self {
        Self::new(ErrorCode::Timeout, "timed out waiting for the peer")
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone()).with_details(self.details.clone())
    }
}

impl From<ErrorCode> for ApiError {
    fn from(code: ErrorCode) -> Self {
        let msg = match code {
            ErrorCode::Busy => "host has reached its participant limit",
            ErrorCode::HostDisabled => "target does not accept this connection",
            ErrorCode::DeviceOffline => "target device is offline",
            ErrorCode::Forbidden => "forbidden",
            ErrorCode::NotFound => "not found",
            ErrorCode::DeviceNotFound => "device not found",
            ErrorCode::Conflict => "state has changed",
            _ => code.as_str(),
        };
        Self::new(code, msg)
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        tracing::error!(error = %format!("{err:#}"), "internal error");
        Self::internal()
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = StatusCode::from_u16(self.code.http_status())
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let retry_after = (self.code == ErrorCode::RateLimited)
            .then(|| self.details.get("retry_after_ms").and_then(Value::as_u64))
            .flatten();
        let envelope = ErrorEnvelope { error: self.body() };
        let mut resp = (status, axum::Json(envelope)).into_response();
        if let Some(ms) = retry_after
            && let Ok(v) = HeaderValue::from_str(&ms.div_ceil(1000).max(1).to_string())
        {
            resp.headers_mut().insert(header::RETRY_AFTER, v);
        }
        resp
    }
}
