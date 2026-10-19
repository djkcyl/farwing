//! HTTPS error responses (`{"error": {...}}`).

use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use farwing_protocol::{ErrorBody, ErrorCode, ErrorEnvelope};
use serde_json::{Value, json};

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

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = details;
        self
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::BadRequest, msg)
    }
    pub fn unauthorized() -> Self {
        Self::new(ErrorCode::Unauthorized, "缺少或无效的登录凭据")
    }
    pub fn device_unauthorized() -> Self {
        Self::new(ErrorCode::DeviceUnauthorized, "设备凭据无效")
    }
    pub fn forbidden(msg: impl Into<String>) -> Self {
        Self::new(ErrorCode::Forbidden, msg)
    }
    pub fn not_found() -> Self {
        Self::new(ErrorCode::NotFound, "不存在")
    }
    pub fn device_not_found() -> Self {
        Self::new(ErrorCode::DeviceNotFound, "设备不存在")
    }
    pub fn device_offline() -> Self {
        Self::new(ErrorCode::DeviceOffline, "目标设备不在线")
    }
    pub fn internal() -> Self {
        Self::new(ErrorCode::Internal, "服务器内部错误")
    }
    pub fn rate_limited(retry_after_ms: u64) -> Self {
        Self::new(ErrorCode::RateLimited, "请求过于频繁")
            .with_details(json!({ "retry_after_ms": retry_after_ms }))
    }
    pub fn payload_too_large() -> Self {
        Self::new(ErrorCode::PayloadTooLarge, "请求体过大")
    }
    pub fn timeout() -> Self {
        Self::new(ErrorCode::Timeout, "等待对端超时")
    }

    pub fn body(&self) -> ErrorBody {
        ErrorBody::new(self.code, self.message.clone()).with_details(self.details.clone())
    }
}

impl From<ErrorCode> for ApiError {
    fn from(code: ErrorCode) -> Self {
        let msg = match code {
            ErrorCode::Busy => "被控端同时连接人数已达上限",
            ErrorCode::HostDisabled => "目标未开启允许被控",
            ErrorCode::DeviceOffline => "目标设备不在线",
            ErrorCode::Forbidden => "无权访问",
            ErrorCode::NotFound => "不存在",
            ErrorCode::DeviceNotFound => "设备不存在",
            ErrorCode::Conflict => "状态已变化",
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
