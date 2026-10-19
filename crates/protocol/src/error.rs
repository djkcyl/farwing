//! Error codes shared by the HTTPS API and the realtime plane.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Machine-readable error code. Clients branch on the code, never on the
/// human-readable message.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    BadRequest,
    Unauthorized,
    DeviceUnauthorized,
    DeviceMismatch,
    SessionRevoked,
    Forbidden,
    NotFound,
    DeviceNotFound,
    DeviceOffline,
    HostDisabled,
    Busy,
    Conflict,
    AssistUnavailable,
    AssistCodeRequired,
    AssistCodeInvalid,
    AssistRejected,
    UserNotFound,
    AlreadyFriends,
    Timeout,
    PayloadTooLarge,
    RateLimited,
    ProtocolUnsupported,
    RegistrationDisabled,
    Internal,
    Unavailable,
    /// Client-local: the request did not reach the server or no response was
    /// received. Never sent on the wire by a server.
    Network,
    /// Forward compatibility: a code this build does not know.
    #[serde(other)]
    Unknown,
}

impl ErrorCode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::BadRequest => "bad_request",
            Self::Unauthorized => "unauthorized",
            Self::DeviceUnauthorized => "device_unauthorized",
            Self::DeviceMismatch => "device_mismatch",
            Self::SessionRevoked => "session_revoked",
            Self::Forbidden => "forbidden",
            Self::NotFound => "not_found",
            Self::DeviceNotFound => "device_not_found",
            Self::DeviceOffline => "device_offline",
            Self::HostDisabled => "host_disabled",
            Self::Busy => "busy",
            Self::Conflict => "conflict",
            Self::AssistUnavailable => "assist_unavailable",
            Self::AssistCodeRequired => "assist_code_required",
            Self::AssistCodeInvalid => "assist_code_invalid",
            Self::UserNotFound => "user_not_found",
            Self::AlreadyFriends => "already_friends",
            Self::AssistRejected => "assist_rejected",
            Self::Timeout => "timeout",
            Self::PayloadTooLarge => "payload_too_large",
            Self::RateLimited => "rate_limited",
            Self::ProtocolUnsupported => "protocol_unsupported",
            Self::RegistrationDisabled => "registration_disabled",
            Self::Internal => "internal",
            Self::Unavailable => "unavailable",
            Self::Network => "network",
            Self::Unknown => "unknown",
        }
    }

    /// HTTP status used by the server for this code.
    pub fn http_status(self) -> u16 {
        match self {
            Self::BadRequest => 400,
            Self::Unauthorized
            | Self::DeviceUnauthorized
            | Self::DeviceMismatch
            | Self::SessionRevoked => 401,
            Self::Forbidden
            | Self::AssistCodeInvalid
            | Self::AssistRejected
            | Self::RegistrationDisabled => 403,
            Self::NotFound | Self::DeviceNotFound | Self::UserNotFound => 404,
            Self::Timeout => 408,
            Self::DeviceOffline
            | Self::HostDisabled
            | Self::Busy
            | Self::Conflict
            | Self::AssistUnavailable
            | Self::AssistCodeRequired
            | Self::AlreadyFriends => 409,
            Self::PayloadTooLarge => 413,
            Self::ProtocolUnsupported => 426,
            Self::RateLimited => 429,
            Self::Unavailable => 503,
            Self::Internal | Self::Network | Self::Unknown => 500,
        }
    }

    /// Whether a client may automatically retry an idempotent request.
    pub fn retryable(self) -> bool {
        matches!(
            self,
            Self::Timeout | Self::RateLimited | Self::Internal | Self::Unavailable | Self::Network
        )
    }

    /// Codes that mean the saved account credentials are no longer valid.
    pub fn invalidates_login(self) -> bool {
        matches!(
            self,
            Self::Unauthorized | Self::DeviceMismatch | Self::SessionRevoked
        )
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// `{"code","message","retryable","details"}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    #[serde(default)]
    pub message: String,
    #[serde(default)]
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub details: serde_json::Value,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            retryable: code.retryable(),
            details: serde_json::Value::Null,
        }
    }

    pub fn with_details(mut self, details: serde_json::Value) -> Self {
        self.details = details;
        self
    }
}

/// HTTPS error response body: `{"error": {...}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}
