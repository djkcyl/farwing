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
    /// The server has not been set up: only its setup page answers.
    SetupRequired,
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
            Self::SetupRequired => "setup_required",
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
            Self::Unavailable | Self::SetupRequired => 503,
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

    /// `details.reason`, when the error names one ([`reason`]).
    pub fn reason(&self) -> Option<&str> {
        self.details.get("reason").and_then(serde_json::Value::as_str)
    }
}

/// HTTPS error response body: `{"error": {...}}`.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ErrorEnvelope {
    pub error: ErrorBody,
}

/// `details.reason`: which case of an [`ErrorCode`] an error is, where one
/// code covers several (`forbidden`: a wrong password, a missing second
/// factor, …). Errors whose code says everything have none.
///
/// Clients show a text for the code and the reason in their own language;
/// `message` is a short English text for logs and is never shown. A text's
/// parameters are further `details` fields, named next to each reason.
pub mod reason {
    // ---- signing in and the account
    /// `unauthorized`: wrong username or password, or a disabled account.
    pub const LOGIN_FAILED: &str = "login_failed";
    /// `forbidden`
    pub const PASSWORD_INCORRECT: &str = "password_incorrect";
    /// `forbidden`: changing the password.
    pub const CURRENT_PASSWORD_INCORRECT: &str = "current_password_incorrect";
    /// `forbidden`: this login has not passed the second factor.
    pub const TOTP_REQUIRED: &str = "totp_required";
    /// `forbidden`: a wrong TOTP or recovery code.
    pub const TOTP_CODE_INCORRECT: &str = "totp_code_incorrect";
    /// `forbidden`
    pub const TOTP_ADMIN_ONLY: &str = "totp_admin_only";
    /// `conflict`
    pub const TOTP_ALREADY_ENABLED: &str = "totp_already_enabled";
    /// `conflict`: enabling without a started setup.
    pub const TOTP_SETUP_REQUIRED: &str = "totp_setup_required";
    /// `forbidden`
    pub const ADMIN_REQUIRED: &str = "admin_required";
    /// `forbidden`: the client address is outside `[web] admin_allow`.
    pub const ADMIN_NETWORK_DENIED: &str = "admin_network_denied";
    /// `forbidden`: guest sessions are turned off.
    pub const GUESTS_DISABLED: &str = "guests_disabled";
    /// `device_unauthorized`: the request time is too far from the server's.
    pub const CLOCK_SKEW: &str = "clock_skew";

    // ---- input checks (`bad_request` unless noted)
    /// `max`: the longest username.
    pub const USERNAME_INVALID: &str = "username_invalid";
    /// `conflict`
    pub const USERNAME_TAKEN: &str = "username_taken";
    /// `min`: the shortest password.
    pub const PASSWORD_TOO_SHORT: &str = "password_too_short";
    /// `max`: the longest password.
    pub const PASSWORD_TOO_LONG: &str = "password_too_long";
    pub const NICKNAME_INVALID: &str = "nickname_invalid";
    /// `max`: the longest device name.
    pub const DEVICE_NAME_INVALID: &str = "device_name_invalid";
    pub const INVITE_INVALID: &str = "invite_invalid";
    /// Used up, revoked or past its expiry.
    pub const INVITE_EXPIRED: &str = "invite_expired";
    /// `min`, `max`: the allowed number of uses.
    pub const INVITE_USES_INVALID: &str = "invite_uses_invalid";
    /// The validity is not between one hour and one year.
    pub const INVITE_VALIDITY_INVALID: &str = "invite_validity_invalid";
    /// The last administrator would be removed, demoted or disabled.
    pub const LAST_ADMIN: &str = "last_admin";
    /// An administrator demoting or disabling their own account.
    pub const SELF_DEMOTE: &str = "self_demote";
    pub const SELF_DELETE: &str = "self_delete";
    pub const UPDATE_PLATFORM_INVALID: &str = "update_platform_invalid";
    pub const UPDATE_CHANNEL_INVALID: &str = "update_channel_invalid";
    pub const UPDATE_VERSION_INVALID: &str = "update_version_invalid";
    pub const UPDATE_URL_INVALID: &str = "update_url_invalid";
    pub const UPDATE_NOTES_TOO_LONG: &str = "update_notes_too_long";
    /// A setting was refused; `details`: `{field, problem}` where
    /// `problem` is `required`, `invalid`, `out_of_range`, `conflict`,
    /// `unavailable` (a port or file cannot be used) or `locked`.
    pub const SETTING_INVALID: &str = "setting_invalid";
    /// The setup or recovery code is wrong or has expired.
    pub const SETUP_CODE_INVALID: &str = "setup_code_invalid";
    /// The server is already set up.
    pub const SETUP_DONE: &str = "setup_done";
    /// Only a signed-in administrator, not an API token, may do this.
    pub const ADMIN_LOGIN_REQUIRED: &str = "admin_login_required";
    /// No listen change waits for confirmation.
    pub const NOTHING_TO_CONFIRM: &str = "nothing_to_confirm";
    pub const SELF_FRIEND: &str = "self_friend";
    /// A device connecting to (or assisting) itself.
    pub const SELF_CONNECT: &str = "self_connect";

    // ---- devices, permissions and sessions
    /// `forbidden`: a friend's device where only own devices are allowed.
    pub const NOT_OWN_DEVICE: &str = "not_own_device";
    /// `forbidden`: the grants do not include what the purpose needs.
    pub const NOT_GRANTED: &str = "not_granted";
    /// `forbidden`: the server does not allow it to browsers.
    pub const WEB_NOT_ALLOWED: &str = "web_not_allowed";
    /// `forbidden`
    pub const POWER_NOT_GRANTED: &str = "power_not_granted";
    /// `host_disabled`: the host does not allow shutdown and restart.
    pub const POWER_DISABLED: &str = "power_disabled";
    /// `host_disabled`: the host does not allow file transfer.
    pub const FILES_DISABLED: &str = "files_disabled";
    /// `host_disabled`: the host does not allow port forwarding.
    pub const PORTS_DISABLED: &str = "ports_disabled";
    /// `forbidden`: the target has no Wake-on-LAN set up.
    pub const WOL_UNAVAILABLE: &str = "wol_unavailable";
    /// `conflict`: waking a device that is online.
    pub const DEVICE_ONLINE: &str = "device_online";
    /// `not_found`
    pub const NOT_FRIENDS: &str = "not_friends";
    /// `conflict`: an assistance request withdrawn while it waited.
    pub const REQUEST_CANCELLED: &str = "request_cancelled";
    /// `not_found`: the session is gone (realtime plane).
    pub const SESSION_ENDED: &str = "session_ended";

    // ---- a host refusing a direct (LAN) connection
    /// `forbidden`: the host does not trust this device.
    pub const NOT_TRUSTED: &str = "not_trusted";
    /// `assist_unavailable`: the host has no access code.
    pub const NO_ACCESS_CODE: &str = "no_access_code";
    /// `assist_unavailable`: the host is answering another request.
    pub const HOST_BUSY: &str = "host_busy";
    /// `assist_unavailable`: the host's computer is locked.
    pub const HOST_LOCKED: &str = "host_locked";
    /// `assist_unavailable`: nobody can answer on the host (no window open).
    pub const HOST_APP_CLOSED: &str = "host_app_closed";
    /// `assist_rejected`
    pub const PAIRING_DECLINED: &str = "pairing_declined";
    /// `rate_limited`
    pub const PAIRING_RECENTLY_DECLINED: &str = "pairing_recently_declined";
    /// `rate_limited`, `minutes`: too many wrong access codes.
    pub const CODE_LOCKED: &str = "code_locked";

    // ---- client-local (`network`), never sent by a server
    pub const TIMED_OUT: &str = "timed_out";
    pub const CONNECTION_LOST: &str = "connection_lost";

    /// Every reason, for checking that translations are complete.
    pub const ALL: &[&str] = &[
        LOGIN_FAILED,
        PASSWORD_INCORRECT,
        CURRENT_PASSWORD_INCORRECT,
        TOTP_REQUIRED,
        TOTP_CODE_INCORRECT,
        TOTP_ADMIN_ONLY,
        TOTP_ALREADY_ENABLED,
        TOTP_SETUP_REQUIRED,
        ADMIN_REQUIRED,
        ADMIN_NETWORK_DENIED,
        GUESTS_DISABLED,
        CLOCK_SKEW,
        USERNAME_INVALID,
        USERNAME_TAKEN,
        PASSWORD_TOO_SHORT,
        PASSWORD_TOO_LONG,
        NICKNAME_INVALID,
        DEVICE_NAME_INVALID,
        INVITE_INVALID,
        INVITE_EXPIRED,
        INVITE_USES_INVALID,
        INVITE_VALIDITY_INVALID,
        LAST_ADMIN,
        SELF_DEMOTE,
        SELF_DELETE,
        UPDATE_PLATFORM_INVALID,
        UPDATE_CHANNEL_INVALID,
        UPDATE_VERSION_INVALID,
        UPDATE_URL_INVALID,
        UPDATE_NOTES_TOO_LONG,
        SETTING_INVALID,
        SETUP_CODE_INVALID,
        SETUP_DONE,
        ADMIN_LOGIN_REQUIRED,
        NOTHING_TO_CONFIRM,
        SELF_FRIEND,
        SELF_CONNECT,
        NOT_OWN_DEVICE,
        NOT_GRANTED,
        WEB_NOT_ALLOWED,
        POWER_NOT_GRANTED,
        POWER_DISABLED,
        FILES_DISABLED,
        PORTS_DISABLED,
        WOL_UNAVAILABLE,
        DEVICE_ONLINE,
        NOT_FRIENDS,
        REQUEST_CANCELLED,
        SESSION_ENDED,
        NOT_TRUSTED,
        NO_ACCESS_CODE,
        HOST_BUSY,
        HOST_LOCKED,
        HOST_APP_CLOSED,
        PAIRING_DECLINED,
        PAIRING_RECENTLY_DECLINED,
        CODE_LOCKED,
        TIMED_OUT,
        CONNECTION_LOST,
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reasons_are_distinct_snake_case() {
        let mut seen = std::collections::HashSet::new();
        for key in reason::ALL {
            assert!(
                !key.is_empty() && key.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'),
                "{key}"
            );
            assert!(seen.insert(key), "{key} twice");
        }
    }

    #[test]
    fn reason_is_read_from_details() {
        let body = ErrorBody::new(ErrorCode::Forbidden, "x")
            .with_details(serde_json::json!({ "reason": reason::TOTP_REQUIRED }));
        assert_eq!(body.reason(), Some(reason::TOTP_REQUIRED));
        assert_eq!(ErrorBody::new(ErrorCode::Forbidden, "x").reason(), None);
    }
}
