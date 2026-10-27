//! Server errors, translated here instead of showing the server's text: a
//! text per `ErrorCode` and per `details.reason`
//! (`farwing_protocol::error::reason`); parameters come from `details`.
use farwing_protocol::ErrorCode;
use farwing_protocol::error::reason;
use serde_json::Value;

pub(crate) struct Errors {
    // ------------------------------------------------------------- codes
    pub bad_request: &'static str,
    pub unauthorized: &'static str,
    pub device_unauthorized: &'static str,
    pub device_mismatch: &'static str,
    pub session_revoked: &'static str,
    pub forbidden: &'static str,
    pub not_found: &'static str,
    pub device_not_found: &'static str,
    pub device_offline: &'static str,
    pub host_disabled: &'static str,
    pub busy: &'static str,
    pub conflict: &'static str,
    pub assist_unavailable: &'static str,
    pub assist_code_required: &'static str,
    pub assist_code_invalid: &'static str,
    pub assist_rejected: &'static str,
    pub user_not_found: &'static str,
    pub already_friends: &'static str,
    pub timeout: &'static str,
    pub payload_too_large: &'static str,
    pub rate_limited: &'static str,
    pub protocol_unsupported: &'static str,
    pub registration_disabled: &'static str,
    pub internal: &'static str,
    pub unavailable: &'static str,
    pub setup_required: &'static str,
    pub network: &'static str,
    pub unknown: &'static str,

    // ------------------------------------------- signing in, the account
    pub login_failed: &'static str,
    pub password_incorrect: &'static str,
    pub current_password_incorrect: &'static str,
    pub totp_required: &'static str,
    pub totp_code_incorrect: &'static str,
    pub totp_admin_only: &'static str,
    pub totp_already_enabled: &'static str,
    pub totp_setup_required: &'static str,
    pub admin_required: &'static str,
    pub admin_network_denied: &'static str,
    pub guests_disabled: &'static str,
    pub clock_skew: &'static str,

    // ------------------------------------------------------- input checks
    /// The longest username.
    pub username_invalid: fn(u64) -> String,
    pub username_taken: &'static str,
    /// The shortest password.
    pub password_too_short: fn(u64) -> String,
    /// The longest password.
    pub password_too_long: fn(u64) -> String,
    pub nickname_invalid: &'static str,
    /// The longest device name.
    pub device_name_invalid: fn(u64) -> String,
    pub invite_invalid: &'static str,
    pub invite_expired: &'static str,
    /// Fewest and most uses.
    pub invite_uses_invalid: fn(u64, u64) -> String,
    pub invite_validity_invalid: &'static str,
    pub last_admin: &'static str,
    pub self_demote: &'static str,
    pub self_delete: &'static str,
    pub update_platform_invalid: &'static str,
    pub update_channel_invalid: &'static str,
    pub update_version_invalid: &'static str,
    pub update_url_invalid: &'static str,
    pub update_notes_too_long: &'static str,
    pub setting_invalid: &'static str,
    pub setup_code_invalid: &'static str,
    pub setup_done: &'static str,
    pub admin_login_required: &'static str,
    pub nothing_to_confirm: &'static str,
    pub self_friend: &'static str,
    pub self_connect: &'static str,
    pub forwarding_disabled: &'static str,
    pub forwarding_mode_not_allowed: &'static str,
    pub forwarding_unavailable: &'static str,

    // ------------------------------------ devices, permissions, sessions
    pub not_own_device: &'static str,
    pub not_granted: &'static str,
    pub web_not_allowed: &'static str,
    pub power_not_granted: &'static str,
    pub power_disabled: &'static str,
    pub files_disabled: &'static str,
    pub ports_disabled: &'static str,
    pub wol_unavailable: &'static str,
    pub device_online: &'static str,
    pub not_friends: &'static str,
    pub request_cancelled: &'static str,
    pub session_ended: &'static str,

    // ------------------------------------- a host refusing a direct link
    pub not_trusted: &'static str,
    pub no_access_code: &'static str,
    pub host_busy: &'static str,
    pub host_locked: &'static str,
    pub host_app_closed: &'static str,
    pub pairing_declined: &'static str,
    pub pairing_recently_declined: &'static str,
    /// Minutes locked.
    pub code_locked: &'static str,

    // ------------------------------------------- reaching the server
    pub timed_out: &'static str,
    pub connection_lost: &'static str,
}

impl Errors {
    /// The text of a code alone.
    pub(crate) fn code(&self, code: ErrorCode) -> &'static str {
        match code {
            ErrorCode::BadRequest => self.bad_request,
            ErrorCode::Unauthorized => self.unauthorized,
            ErrorCode::DeviceUnauthorized => self.device_unauthorized,
            ErrorCode::DeviceMismatch => self.device_mismatch,
            ErrorCode::SessionRevoked => self.session_revoked,
            ErrorCode::Forbidden => self.forbidden,
            ErrorCode::NotFound => self.not_found,
            ErrorCode::DeviceNotFound => self.device_not_found,
            ErrorCode::DeviceOffline => self.device_offline,
            ErrorCode::HostDisabled => self.host_disabled,
            ErrorCode::Busy => self.busy,
            ErrorCode::Conflict => self.conflict,
            ErrorCode::AssistUnavailable => self.assist_unavailable,
            ErrorCode::AssistCodeRequired => self.assist_code_required,
            ErrorCode::AssistCodeInvalid => self.assist_code_invalid,
            ErrorCode::AssistRejected => self.assist_rejected,
            ErrorCode::UserNotFound => self.user_not_found,
            ErrorCode::AlreadyFriends => self.already_friends,
            ErrorCode::Timeout => self.timeout,
            ErrorCode::PayloadTooLarge => self.payload_too_large,
            ErrorCode::RateLimited => self.rate_limited,
            ErrorCode::ProtocolUnsupported => self.protocol_unsupported,
            ErrorCode::RegistrationDisabled => self.registration_disabled,
            ErrorCode::Internal => self.internal,
            ErrorCode::Unavailable => self.unavailable,
            ErrorCode::SetupRequired => self.setup_required,
            ErrorCode::Network => self.network,
            ErrorCode::Unknown => self.unknown,
        }
    }

    /// The text of a reason with its parameters from `details`; `None` for
    /// a reason this build does not know or a missing parameter.
    pub(crate) fn reason(&self, key: &str, details: &Value) -> Option<String> {
        let n = |name: &str| details.get(name).and_then(Value::as_u64);
        let text = |s: &'static str| Some(s.to_owned());
        match key {
            reason::LOGIN_FAILED => text(self.login_failed),
            reason::PASSWORD_INCORRECT => text(self.password_incorrect),
            reason::CURRENT_PASSWORD_INCORRECT => text(self.current_password_incorrect),
            reason::TOTP_REQUIRED => text(self.totp_required),
            reason::TOTP_CODE_INCORRECT => text(self.totp_code_incorrect),
            reason::TOTP_ADMIN_ONLY => text(self.totp_admin_only),
            reason::TOTP_ALREADY_ENABLED => text(self.totp_already_enabled),
            reason::TOTP_SETUP_REQUIRED => text(self.totp_setup_required),
            reason::ADMIN_REQUIRED => text(self.admin_required),
            reason::ADMIN_NETWORK_DENIED => text(self.admin_network_denied),
            reason::GUESTS_DISABLED => text(self.guests_disabled),
            reason::CLOCK_SKEW => text(self.clock_skew),
            reason::USERNAME_INVALID => n("max").map(self.username_invalid),
            reason::USERNAME_TAKEN => text(self.username_taken),
            reason::PASSWORD_TOO_SHORT => n("min").map(self.password_too_short),
            reason::PASSWORD_TOO_LONG => n("max").map(self.password_too_long),
            reason::NICKNAME_INVALID => text(self.nickname_invalid),
            reason::DEVICE_NAME_INVALID => n("max").map(self.device_name_invalid),
            reason::INVITE_INVALID => text(self.invite_invalid),
            reason::INVITE_EXPIRED => text(self.invite_expired),
            reason::INVITE_USES_INVALID => Some((self.invite_uses_invalid)(n("min")?, n("max")?)),
            reason::INVITE_VALIDITY_INVALID => text(self.invite_validity_invalid),
            reason::LAST_ADMIN => text(self.last_admin),
            reason::SELF_DEMOTE => text(self.self_demote),
            reason::SELF_DELETE => text(self.self_delete),
            reason::UPDATE_PLATFORM_INVALID => text(self.update_platform_invalid),
            reason::UPDATE_CHANNEL_INVALID => text(self.update_channel_invalid),
            reason::UPDATE_VERSION_INVALID => text(self.update_version_invalid),
            reason::UPDATE_URL_INVALID => text(self.update_url_invalid),
            reason::UPDATE_NOTES_TOO_LONG => text(self.update_notes_too_long),
            reason::SETTING_INVALID => text(self.setting_invalid),
            reason::SETUP_CODE_INVALID => text(self.setup_code_invalid),
            reason::SETUP_DONE => text(self.setup_done),
            reason::ADMIN_LOGIN_REQUIRED => text(self.admin_login_required),
            reason::NOTHING_TO_CONFIRM => text(self.nothing_to_confirm),
            reason::SELF_FRIEND => text(self.self_friend),
            reason::SELF_CONNECT => text(self.self_connect),
            reason::FORWARDING_DISABLED => text(self.forwarding_disabled),
            reason::FORWARDING_MODE_NOT_ALLOWED => text(self.forwarding_mode_not_allowed),
            reason::FORWARDING_UNAVAILABLE => text(self.forwarding_unavailable),
            reason::NOT_OWN_DEVICE => text(self.not_own_device),
            reason::NOT_GRANTED => text(self.not_granted),
            reason::WEB_NOT_ALLOWED => text(self.web_not_allowed),
            reason::POWER_NOT_GRANTED => text(self.power_not_granted),
            reason::POWER_DISABLED => text(self.power_disabled),
            reason::FILES_DISABLED => text(self.files_disabled),
            reason::PORTS_DISABLED => text(self.ports_disabled),
            reason::WOL_UNAVAILABLE => text(self.wol_unavailable),
            reason::DEVICE_ONLINE => text(self.device_online),
            reason::NOT_FRIENDS => text(self.not_friends),
            reason::REQUEST_CANCELLED => text(self.request_cancelled),
            reason::SESSION_ENDED => text(self.session_ended),
            reason::NOT_TRUSTED => text(self.not_trusted),
            reason::NO_ACCESS_CODE => text(self.no_access_code),
            reason::HOST_BUSY => text(self.host_busy),
            reason::HOST_LOCKED => text(self.host_locked),
            reason::HOST_APP_CLOSED => text(self.host_app_closed),
            reason::PAIRING_DECLINED => text(self.pairing_declined),
            reason::PAIRING_RECENTLY_DECLINED => text(self.pairing_recently_declined),
            reason::CODE_LOCKED => text(self.code_locked),
            reason::TIMED_OUT => text(self.timed_out),
            reason::CONNECTION_LOST => text(self.connection_lost),
            _ => None,
        }
    }

    /// The text of a failure: its reason's when this build knows it, else
    /// its code's. The server's own message is never shown.
    pub(crate) fn text(&self, code: ErrorCode, reason: Option<&str>, details: &Value) -> String {
        reason
            .and_then(|key| self.reason(key, details))
            .unwrap_or_else(|| self.code(code).to_owned())
    }
}

pub(crate) const ZH_CN: Errors = Errors {
    bad_request: "请求无效",
    unauthorized: "需要登录",
    device_unauthorized: "设备验证失败",
    device_mismatch: "登录与本设备不匹配",
    session_revoked: "登录已失效",
    forbidden: "无权访问",
    not_found: "不存在",
    device_not_found: "设备不存在",
    device_offline: "设备离线",
    host_disabled: "对方不允许连接",
    busy: "对方的连接数已达上限",
    conflict: "状态已变化",
    assist_unavailable: "对方未开启访问码，也未信任本设备",
    assist_code_required: "需要访问码",
    assist_code_invalid: "访问码不正确",
    assist_rejected: "对方拒绝了连接",
    user_not_found: "用户不存在",
    already_friends: "已经是好友",
    timeout: "等待对方响应超时",
    payload_too_large: "请求数据过大",
    rate_limited: "操作过于频繁",
    protocol_unsupported: "客户端与服务器版本不兼容",
    registration_disabled: "服务器未开放注册",
    internal: "服务器内部错误",
    unavailable: "服务不可用",
    setup_required: "服务器尚未初始化",
    network: "无法连接服务器",
    unknown: "未知错误",

    login_failed: "用户名或密码不正确，或账号已停用",
    password_incorrect: "密码不正确",
    current_password_incorrect: "当前密码不正确",
    totp_required: "需要二次验证",
    totp_code_incorrect: "验证码不正确",
    totp_admin_only: "二次验证仅用于管理员账号",
    totp_already_enabled: "二次验证已开启",
    totp_setup_required: "未开始设置二次验证",
    admin_required: "需要管理员权限",
    admin_network_denied: "不允许从此网络访问管理后台",
    guests_disabled: "服务器未开启游客访问",
    clock_skew: "本机时间与服务器时间相差过大",

    username_invalid: |max| format!("用户名应为 1–{max} 位字母、数字或 . _ - @"),
    username_taken: "用户名已被使用",
    password_too_short: |min| format!("密码至少 {min} 位"),
    password_too_long: |max| format!("密码不能超过 {max} 位"),
    nickname_invalid: "昵称不能包含控制字符",
    device_name_invalid: |max| format!("名称应为 1–{max} 个可打印字符"),
    invite_invalid: "邀请码无效",
    invite_expired: "邀请码已失效",
    invite_uses_invalid: |min, max| format!("可用次数应为 {min}–{max}"),
    invite_validity_invalid: "有效期应为 1 小时到 1 年",
    last_admin: "至少需要保留一个管理员",
    self_demote: "不能降级或禁用自己的账号",
    self_delete: "不能删除自己的账号",
    update_platform_invalid: "平台无效",
    update_channel_invalid: "渠道应为 stable 或 beta",
    update_version_invalid: "版本号无效",
    update_url_invalid: "下载地址应为 http(s) 链接",
    update_notes_too_long: "说明过长",
    setting_invalid: "设置无效",
    setup_code_invalid: "初始化码错误或已过期",
    setup_done: "服务器已初始化",
    admin_login_required: "需要管理员登录操作",
    nothing_to_confirm: "没有待确认的更改",
    self_friend: "不能添加自己为好友",
    self_connect: "不能连接本机",
    forwarding_disabled: "服务器未开启多人转发",
    forwarding_mode_not_allowed: "服务器不允许兼容模式转发",
    forwarding_unavailable: "多人转发暂不可用",

    not_own_device: "只能管理自己账号的设备",
    not_granted: "没有该设备的此项权限",
    web_not_allowed: "服务器不允许网页端使用此功能",
    power_not_granted: "没有该设备的电源权限",
    power_disabled: "对方未允许远程关机和重启",
    files_disabled: "对方未允许文件传输",
    ports_disabled: "对方未允许端口转发",
    wol_unavailable: "设备未启用网络唤醒",
    device_online: "设备已在线",
    not_friends: "对方不是好友",
    request_cancelled: "请求已取消",
    session_ended: "会话已结束",

    not_trusted: "对方未信任本设备",
    no_access_code: "对方未开启访问码",
    host_busy: "对方正在处理另一个连接请求",
    host_locked: "对方的电脑已锁定",
    host_app_closed: "对方未打开 Farwing 设备中心",
    pairing_declined: "对方未同意配对",
    pairing_recently_declined: "对方刚拒绝过配对",
    code_locked: "访问码错误次数过多，暂时无法连接",

    timed_out: "连接服务器超时",
    connection_lost: "与服务器的连接已中断",
};

pub(crate) const EN: Errors = Errors {
    bad_request: "Invalid request",
    unauthorized: "Sign-in required",
    device_unauthorized: "Device verification failed",
    device_mismatch: "The sign-in doesn't match this device",
    session_revoked: "Sign-in expired",
    forbidden: "Access denied",
    not_found: "Not found",
    device_not_found: "Device not found",
    device_offline: "The device is offline",
    host_disabled: "The device doesn't allow connections",
    busy: "The device has reached its connection limit",
    conflict: "The state has changed",
    assist_unavailable: "The device has no access code and doesn't trust this device",
    assist_code_required: "Access code required",
    assist_code_invalid: "Incorrect access code",
    assist_rejected: "The connection was declined",
    user_not_found: "User not found",
    already_friends: "Already friends",
    timeout: "Timed out waiting for a response",
    payload_too_large: "Request too large",
    rate_limited: "Too many requests",
    protocol_unsupported: "This version isn't compatible with the server",
    registration_disabled: "Registration is closed on this server",
    internal: "Internal server error",
    unavailable: "Service unavailable",
    setup_required: "This server has not been set up",
    network: "Can't connect to the server",
    unknown: "Unknown error",

    login_failed: "Incorrect username or password, or the account is disabled",
    password_incorrect: "Incorrect password",
    current_password_incorrect: "Incorrect current password",
    totp_required: "Two-step verification required",
    totp_code_incorrect: "Incorrect verification code",
    totp_admin_only: "Two-step verification is only for administrator accounts",
    totp_already_enabled: "Two-step verification is already on",
    totp_setup_required: "Two-step verification setup hasn't started",
    admin_required: "Administrator access required",
    admin_network_denied: "The admin console isn't available from this network",
    guests_disabled: "Guest access is off on this server",
    clock_skew: "This device's clock is too far off the server's",

    username_invalid: |max| format!("Username must be 1–{max} letters, digits or . _ - @"),
    username_taken: "Username already taken",
    password_too_short: |min| format!("Password must be at least {min} characters"),
    password_too_long: |max| format!("Password must be at most {max} characters"),
    nickname_invalid: "Nickname can't contain control characters",
    device_name_invalid: |max| format!("Name must be 1–{max} printable characters"),
    invite_invalid: "Invalid invitation code",
    invite_expired: "Invitation code is no longer valid",
    invite_uses_invalid: |min, max| format!("Number of uses must be {min}–{max}"),
    invite_validity_invalid: "Validity must be between 1 hour and 1 year",
    last_admin: "At least one administrator is required",
    self_demote: "Can't demote or disable your own account",
    self_delete: "Can't delete your own account",
    update_platform_invalid: "Invalid platform",
    update_channel_invalid: "Channel must be stable or beta",
    update_version_invalid: "Invalid version",
    update_url_invalid: "Download URL must be an http(s) link",
    update_notes_too_long: "Notes are too long",
    setting_invalid: "Invalid setting",
    setup_code_invalid: "The setup code is wrong or has expired",
    setup_done: "This server is already set up",
    admin_login_required: "An administrator must sign in to do this",
    nothing_to_confirm: "Nothing waits for confirmation",
    self_friend: "Can't add yourself as a friend",
    self_connect: "Can't connect a device to itself",
    forwarding_disabled: "This server doesn't forward to multiple viewers",
    forwarding_mode_not_allowed: "This server doesn't allow compatible forwarding",
    forwarding_unavailable: "Forwarding is unavailable right now",

    not_own_device: "Only your own devices can be managed",
    not_granted: "No permission for this on the device",
    web_not_allowed: "The server doesn't allow this in the browser",
    power_not_granted: "No power permission for this device",
    power_disabled: "The device doesn't allow remote shutdown and restart",
    files_disabled: "The device doesn't allow file transfer",
    ports_disabled: "The device doesn't allow port forwarding",
    wol_unavailable: "Wake-on-LAN isn't set up on the device",
    device_online: "The device is already online",
    not_friends: "Not friends with this user",
    request_cancelled: "Request cancelled",
    session_ended: "Session ended",

    not_trusted: "The device doesn't trust this device",
    no_access_code: "The device has no access code",
    host_busy: "The device is handling another connection request",
    host_locked: "The other computer is locked",
    host_app_closed: "Farwing isn't open on the other device",
    pairing_declined: "Pairing was declined",
    pairing_recently_declined: "Pairing was declined recently",
    code_locked: "Too many incorrect access codes; try again later",

    timed_out: "Connection to the server timed out",
    connection_lost: "Connection to the server was lost",
};

#[cfg(test)]
mod tests {
    use super::*;

    /// Every reason a server or host may send has a text in both languages.
    #[test]
    fn every_reason_has_a_text() {
        let details = serde_json::json!({ "min": 1, "max": 64, "minutes": 5 });
        for errors in [&ZH_CN, &EN] {
            for key in reason::ALL {
                assert!(errors.reason(key, &details).is_some(), "{key}");
            }
        }
    }

    #[test]
    fn reason_wins_over_code_and_unknown_falls_back() {
        let details = serde_json::json!({ "reason": reason::PASSWORD_TOO_SHORT, "min": 8 });
        let text = EN.text(ErrorCode::BadRequest, Some(reason::PASSWORD_TOO_SHORT), &details);
        assert_eq!(text, "Password must be at least 8 characters");
        let text = ZH_CN.text(ErrorCode::Forbidden, Some("from_a_newer_server"), &Value::Null);
        assert_eq!(text, ZH_CN.forbidden);
        // A missing parameter: the code's text, not a broken sentence.
        let text = EN.text(ErrorCode::BadRequest, Some(reason::PASSWORD_TOO_SHORT), &Value::Null);
        assert_eq!(text, EN.bad_request);
    }
}
