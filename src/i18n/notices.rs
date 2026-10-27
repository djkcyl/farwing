//! Notifications (`docs/notifications.md`): the page, the settings, and
//! each kind's words from its data.
use serde_json::Value;

pub(crate) struct Notices {
    pub page: &'static str,
    pub empty: &'static str,
    pub loading: &'static str,
    pub load_more: &'static str,
    pub settings: &'static str,
    /// Settings: what each kind is.
    pub kind: fn(&str) -> &'static str,
    /// Settings: the group of kinds (`security`, `connections`, …).
    pub group: fn(&str) -> &'static str,
    /// Title and line of a notification from its kind and data.
    pub text: fn(&str, &Value) -> (String, String),
}

fn s(d: &Value, key: &str) -> String {
    match d.get(key) {
        Some(Value::String(v)) => v.clone(),
        Some(Value::Number(v)) => v.to_string(),
        _ => String::new(),
    }
}

fn or(value: String, fallback: &str) -> String {
    if value.is_empty() { fallback.to_owned() } else { value }
}

fn zh_text(kind: &str, d: &Value) -> (String, String) {
    let by = s(d, "by");
    let admin_line = if by.is_empty() { String::new() } else { format!("管理员：{by}") };
    match kind {
        "login.new_device" => (
            "新设备登录了你的账号".into(),
            [s(d, "device"), s(d, "ip")].iter().filter(|v| !v.is_empty()).cloned().collect::<Vec<_>>().join(" · "),
        ),
        "login.failures" => (
            "账号登录连续失败".into(),
            format!("10 分钟内失败 {} 次 · 最近来自 {}", s(d, "count"), s(d, "ip")),
        ),
        "security.changed" => (
            match s(d, "what").as_str() {
                "totp_on" => "已开启二次验证",
                "totp_off" => "已关闭二次验证",
                "totp_reset" => "管理员重置了你的二次验证",
                _ if !by.is_empty() => "管理员重置了你的密码",
                _ => "账号密码已修改",
            }
            .into(),
            admin_line,
        ),
        "device.removed" => {
            let device = s(d, "device");
            (
                match s(d, "how").as_str() {
                    "signed_out" => format!("{device} 已退出登录"),
                    "unbound" => format!("管理员解绑了 {device}"),
                    "disconnected" => format!("管理员让 {device} 下线"),
                    _ => format!("{device} 已从账号移除"),
                },
                admin_line,
            )
        }
        "connection.incoming" => (
            format!("{} 连接了 {}", or(s(d, "who"), "访客"), s(d, "host")),
            s(d, "controller"),
        ),
        "connection.own" => (format!("{} 连接了 {}", s(d, "controller"), s(d, "host")), String::new()),
        "connection.refused" => (
            format!("{} 连接 {} 未成功", or(s(d, "who"), "访客"), s(d, "host")),
            s(d, "controller"),
        ),
        "power.remote" => {
            let action = match s(d, "action").as_str() {
                "reboot" => "重启",
                "shutdown" => "关闭",
                "wake" => "唤醒",
                _ => "远程操作",
            };
            (format!("{} {action}了 {}", s(d, "who"), s(d, "device")), String::new())
        }
        "friend.request" => (format!("{} 请求添加你为好友", s(d, "who")), String::new()),
        "friend.accepted" => (format!("{} 已成为你的好友", s(d, "who")), String::new()),
        "friend.grants" => (format!("{} 修改了你在其设备上的权限", s(d, "who")), String::new()),
        "device.offline" => (format!("{} 已离线 {} 分钟", s(d, "device"), s(d, "minutes")), String::new()),
        "access.changed" => {
            let (device, other) = (s(d, "device"), s(d, "other"));
            (
                match s(d, "what").as_str() {
                    "access_code.on" => format!("{device} 开启了访问码"),
                    "access_code.off" => format!("{device} 关闭了访问码"),
                    "trusted.add" => format!("{device} 信任了 {other}"),
                    _ => format!("{device} 不再信任 {other}"),
                },
                String::new(),
            )
        }
        "admin.certificate" => ("证书自动申请失败".into(), s(d, "error")),
        "admin.node_offline" => (format!("中转节点 {} 已离线", s(d, "node")), String::new()),
        "admin.registered" => {
            let invite = s(d, "invite");
            (
                format!("{} 注册了账号", s(d, "who")),
                if invite.is_empty() { String::new() } else { format!("邀请码：{invite}") },
            )
        }
        other => (other.to_owned(), String::new()),
    }
}

fn en_text(kind: &str, d: &Value) -> (String, String) {
    let by = s(d, "by");
    let admin_line = if by.is_empty() { String::new() } else { format!("Administrator: {by}") };
    match kind {
        "login.new_device" => (
            "A new device signed in to your account".into(),
            [s(d, "device"), s(d, "ip")].iter().filter(|v| !v.is_empty()).cloned().collect::<Vec<_>>().join(" · "),
        ),
        "login.failures" => (
            "Repeated failed sign-ins".into(),
            format!("{} failures in 10 minutes · last from {}", s(d, "count"), s(d, "ip")),
        ),
        "security.changed" => (
            match s(d, "what").as_str() {
                "totp_on" => "Two-step verification turned on",
                "totp_off" => "Two-step verification turned off",
                "totp_reset" => "An administrator reset your two-step verification",
                _ if !by.is_empty() => "An administrator reset your password",
                _ => "Your password was changed",
            }
            .into(),
            admin_line,
        ),
        "device.removed" => {
            let device = s(d, "device");
            (
                match s(d, "how").as_str() {
                    "signed_out" => format!("{device} was signed out"),
                    "unbound" => format!("An administrator unbound {device}"),
                    "disconnected" => format!("An administrator disconnected {device}"),
                    _ => format!("{device} was removed from your account"),
                },
                admin_line,
            )
        }
        "connection.incoming" => (
            format!("{} connected to {}", or(s(d, "who"), "A visitor"), s(d, "host")),
            s(d, "controller"),
        ),
        "connection.own" => (format!("{} connected to {}", s(d, "controller"), s(d, "host")), String::new()),
        "connection.refused" => (
            format!("{} could not connect to {}", or(s(d, "who"), "A visitor"), s(d, "host")),
            s(d, "controller"),
        ),
        "power.remote" => {
            let action = match s(d, "action").as_str() {
                "reboot" => "restarted",
                "shutdown" => "shut down",
                "wake" => "woke",
                _ => "used remote power on",
            };
            (format!("{} {action} {}", s(d, "who"), s(d, "device")), String::new())
        }
        "friend.request" => (format!("{} wants to be your friend", s(d, "who")), String::new()),
        "friend.accepted" => (format!("{} is now your friend", s(d, "who")), String::new()),
        "friend.grants" => (format!("{} changed what you may do on their devices", s(d, "who")), String::new()),
        "device.offline" => (
            format!("{} has been offline for {} minutes", s(d, "device"), s(d, "minutes")),
            String::new(),
        ),
        "access.changed" => {
            let (device, other) = (s(d, "device"), s(d, "other"));
            (
                match s(d, "what").as_str() {
                    "access_code.on" => format!("{device} turned on its access code"),
                    "access_code.off" => format!("{device} turned off its access code"),
                    "trusted.add" => format!("{device} trusts {other}"),
                    _ => format!("{device} no longer trusts {other}"),
                },
                String::new(),
            )
        }
        "admin.certificate" => ("The automatic certificate failed".into(), s(d, "error")),
        "admin.node_offline" => (format!("Relay node {} is offline", s(d, "node")), String::new()),
        "admin.registered" => {
            let invite = s(d, "invite");
            (
                format!("{} registered", s(d, "who")),
                if invite.is_empty() { String::new() } else { format!("Invitation: {invite}") },
            )
        }
        other => (other.to_owned(), String::new()),
    }
}

pub(crate) const ZH_CN: Notices = Notices {
    page: "通知",
    empty: "暂无通知",
    loading: "正在读取通知…",
    load_more: "加载更早的通知",
    settings: "通知设置",
    kind: |k| match k {
        "login.new_device" => "新设备登录",
        "login.failures" => "登录连续失败",
        "security.changed" => "密码与二次验证变更",
        "device.removed" => "设备被移除或下线",
        "connection.incoming" => "他人连接我的设备",
        "connection.refused" => "连接未成功",
        "power.remote" => "好友远程开关机",
        "friend.request" => "好友申请",
        "friend.accepted" => "好友申请通过",
        "friend.grants" => "好友修改权限",
        "connection.own" => "本账号设备之间的连接",
        "device.offline" => "设备离线 10 分钟",
        "access.changed" => "访问码与受信任设备变更",
        "admin.certificate" => "证书申请失败",
        "admin.node_offline" => "中转节点离线",
        "admin.registered" => "新账号注册",
        _ => "",
    },
    group: |g| match g {
        "security" => "账号安全",
        "connections" => "连接",
        "friends" => "好友",
        "devices" => "设备",
        "admin" => "服务器",
        _ => "",
    },
    text: zh_text,
};

pub(crate) const EN: Notices = Notices {
    page: "Notifications",
    empty: "No notifications",
    loading: "Reading notifications…",
    load_more: "Load earlier notifications",
    settings: "Notification settings",
    kind: |k| match k {
        "login.new_device" => "Sign-in from a new device",
        "login.failures" => "Repeated failed sign-ins",
        "security.changed" => "Password and two-step changes",
        "device.removed" => "Device removed or disconnected",
        "connection.incoming" => "Others connecting to my devices",
        "connection.refused" => "Connections not admitted",
        "power.remote" => "Remote power by friends",
        "friend.request" => "Friend requests",
        "friend.accepted" => "Accepted friend requests",
        "friend.grants" => "Permission changes by friends",
        "connection.own" => "Connections between this account's devices",
        "device.offline" => "Device offline for 10 minutes",
        "access.changed" => "Access code and trusted device changes",
        "admin.certificate" => "Certificate failures",
        "admin.node_offline" => "Relay node offline",
        "admin.registered" => "New accounts",
        _ => "",
    },
    group: |g| match g {
        "security" => "Account security",
        "connections" => "Connections",
        "friends" => "Friends",
        "devices" => "Devices",
        "admin" => "Server",
        _ => "",
    },
    text: en_text,
};
