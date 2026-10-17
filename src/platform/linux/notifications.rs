//! Desktop notifications through the freedesktop Notifications service, the
//! counterpart of Windows toasts, and the placement of the in-app notice
//! window.
//!
//! A Windows toast button relaunches the program with its protocol URI. Here
//! the notification server reports `ActionInvoked` to this process on the
//! session bus instead, and the action key is that same URI, so both arrive at
//! the same activation sink.
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, mpsc};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::Value;

type Action = crate::application::app::notifications::Action;
const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";

fn activation_sink() -> &'static Mutex<Option<mpsc::Sender<Action>>> {
    static SINK: OnceLock<Mutex<Option<mpsc::Sender<Action>>>> = OnceLock::new();
    SINK.get_or_init(Mutex::default)
}
pub(crate) fn set_activation_sink(sender: Option<mpsc::Sender<Action>>) {
    *activation_sink().lock().unwrap_or_else(|p| p.into_inner()) = sender;
}
pub(crate) fn receive_activation(uri: &str) -> bool {
    let Ok(action) = Action::parse(uri) else {
        return false;
    };
    activation_sink()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .as_ref()
        .is_some_and(|s| s.send(action).is_ok())
}

#[derive(Clone, PartialEq, Eq)]
pub(crate) struct ToastAction {
    pub label: String,
    pub uri: String,
}
#[derive(Clone, PartialEq, Eq)]
pub(crate) struct Toast {
    /// File-transfer progress in per mille, updated in place.
    pub progress: Option<u16>,
    pub key: String,
    pub title: String,
    pub body: String,
    pub expires_at: i64,
    pub buttons: Vec<ToastAction>,
}

/// Notification ids shown by this process, to the URI their body opens.
fn opened() -> &'static Mutex<HashMap<u32, String>> {
    static OPENED: OnceLock<Mutex<HashMap<u32, String>>> = OnceLock::new();
    OPENED.get_or_init(Mutex::default)
}

/// Notifications the user closed. A progress notice stays closed; its next
/// update must not bring it back.
fn dismissed() -> &'static Mutex<std::collections::HashSet<u32>> {
    static DISMISSED: OnceLock<Mutex<std::collections::HashSet<u32>>> = OnceLock::new();
    DISMISSED.get_or_init(Mutex::default)
}

/// One listener for the process: the signal stream blocks for good, so it is
/// started once and outlives every `Native`.
fn listen(connection: &Connection) -> Result<()> {
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    let connection = connection.clone();
    STARTED
        .get_or_init(|| {
            let proxy =
                Proxy::new(&connection, SERVICE, PATH, SERVICE).map_err(|e| format!("{e:#}"))?;
            let signals = proxy
                .receive_signal("ActionInvoked")
                .map_err(|e| format!("{e:#}"))?;
            let closed = proxy
                .receive_signal("NotificationClosed")
                .map_err(|e| format!("{e:#}"))?;
            std::thread::Builder::new()
                .name("desktop-notifications-closed".into())
                .spawn(move || {
                    for message in closed {
                        // Reason 2: dismissed by the user.
                        if let Ok((id, 2)) = message.body().deserialize::<(u32, u32)>() {
                            dismissed()
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .insert(id);
                        }
                    }
                })
                .map_err(|e| format!("{e:#}"))?;
            std::thread::Builder::new()
                .name("desktop-notifications".into())
                .spawn(move || {
                    for message in signals {
                        let Ok((id, key)) = message.body().deserialize::<(u32, String)>() else {
                            continue;
                        };
                        let uri = if key == "default" {
                            let opened = opened().lock().unwrap_or_else(|p| p.into_inner());
                            match opened.get(&id) {
                                Some(uri) => uri.clone(),
                                None => continue,
                            }
                        } else {
                            key
                        };
                        receive_activation(&uri);
                    }
                })
                .map(|_| ())
                .map_err(|e| format!("{e:#}"))
        })
        .clone()
        .map_err(|e| anyhow::anyhow!("监听桌面通知操作失败：{e}"))
}

/// The server may interpret a subset of HTML in the body.
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub(crate) struct Native {
    proxy: Proxy<'static>,
    actions: bool,
    shown: HashMap<String, (Toast, u32)>,
}
impl Native {
    pub fn new() -> Result<Self> {
        let connection = Connection::session().context("连接桌面会话总线失败")?;
        let proxy =
            Proxy::new(&connection, SERVICE, PATH, SERVICE).context("桌面通知服务不可用")?;
        let capabilities: Vec<String> = proxy
            .call("GetCapabilities", &())
            .context("桌面通知服务没有响应")?;
        // Without actions the notice still informs; the decision is then
        // made in the control center.
        let actions = capabilities.iter().any(|c| c == "actions");
        if actions {
            listen(&connection)?;
        }
        Ok(Self {
            proxy,
            actions,
            shown: HashMap::new(),
        })
    }
    pub fn synchronize(&mut self, items: &[Toast]) -> Result<()> {
        let remove = self
            .shown
            .keys()
            .filter(|key| !items.iter().any(|i| &i.key == *key))
            .cloned()
            .collect::<Vec<_>>();
        for key in remove {
            self.remove(&key)?;
        }
        for item in items {
            let previous = self.shown.get(&item.key);
            if previous.is_some_and(|(old, _)| old == item) {
                continue;
            }
            // A dismissed progress notice stays dismissed; the finished
            // transfer has its own ticket and may notify once.
            if item.progress.is_some()
                && let Some((old, id)) = previous
                && old.progress.is_some()
                && dismissed()
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .contains(id)
            {
                let id = *id;
                self.shown.insert(item.key.clone(), (item.clone(), id));
                continue;
            }
            anyhow::ensure!(
                item.key.len() == 32 && item.key.bytes().all(|b| b.is_ascii_hexdigit()),
                "无效的通知标识"
            );
            let open = Action::uri(
                &item.key,
                crate::application::app::notifications::Verb::Open,
            );
            let mut actions = Vec::new();
            if self.actions {
                actions.push("default".to_owned());
                actions.push("查看".to_owned());
                for button in &item.buttons {
                    actions.push(button.uri.clone());
                    actions.push(button.label.clone());
                }
            }
            let mut hints = HashMap::<&str, Value<'_>>::new();
            hints.insert("suppress-sound", Value::from(true));
            if let Some(progress) = item.progress {
                // The standard progress hint, a percentage.
                hints.insert("value", Value::from(i32::from(progress.min(1000) / 10)));
            }
            let timeout = if item.expires_at > 0 {
                let left = item.expires_at - chrono::Utc::now().timestamp();
                i32::try_from(left.max(1).saturating_mul(1000)).unwrap_or(i32::MAX)
            } else {
                -1
            };
            let id: u32 = self
                .proxy
                .call(
                    "Notify",
                    &(
                        "OpenUUYC",
                        previous.map_or(0, |(_, id)| *id),
                        "",
                        item.title.as_str(),
                        escape(&item.body),
                        actions,
                        hints,
                        timeout,
                    ),
                )
                .context("显示桌面通知失败")?;
            opened()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .insert(id, open);
            self.shown.insert(item.key.clone(), (item.clone(), id));
        }
        Ok(())
    }
    fn remove(&mut self, key: &str) -> Result<()> {
        if let Some((_, id)) = self.shown.remove(key) {
            opened()
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .remove(&id);
            self.proxy
                .call::<_, _, ()>("CloseNotification", &(id,))
                .context("关闭桌面通知失败")?;
        }
        Ok(())
    }
    pub fn clear(&mut self) -> Result<()> {
        let keys = self.shown.keys().cloned().collect::<Vec<_>>();
        for key in keys {
            self.remove(&key)?;
        }
        Ok(())
    }
}

/// Keeps the in-app notice window in the lower right corner of its monitor,
/// above other windows. Wayland lets the compositor place windows, so the
/// position request only takes effect on X11.
pub(crate) struct Placement {
    failed: bool,
}

impl Placement {
    pub(crate) fn new(window: &winit::window::Window) -> Result<Self> {
        window.set_window_level(winit::window::WindowLevel::AlwaysOnTop);
        let placement = Self { failed: false };
        let monitor = window
            .primary_monitor()
            .or_else(|| window.current_monitor())
            .context("找不到用于放置通知的显示器")?;
        placement.move_to(window, &monitor);
        Ok(placement)
    }

    /// Follows resolution changes of the monitor the window is on.
    pub(crate) fn refresh(&mut self, window: &winit::window::Window) {
        match window
            .current_monitor()
            .or_else(|| window.primary_monitor())
        {
            Some(monitor) => {
                self.failed = false;
                self.move_to(window, &monitor);
            }
            None => {
                if !self.failed {
                    tracing::warn!("notification placement unavailable");
                }
                self.failed = true;
            }
        }
    }

    fn move_to(&self, window: &winit::window::Window, monitor: &winit::monitor::MonitorHandle) {
        let size = window.outer_size();
        let margin =
            (crate::ui::theme::NOTIFICATION_MARGIN * window.scale_factor() as f32).round() as i32;
        let origin = monitor.position();
        let area = monitor.size();
        let right = origin.x + area.width as i32;
        let bottom = origin.y + area.height as i32;
        let position = winit::dpi::PhysicalPosition::new(
            (right - size.width as i32 - margin).max(origin.x),
            (bottom - size.height as i32 - margin).max(origin.y),
        );
        if window.outer_position().ok() != Some(position) {
            window.set_outer_position(position);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Shows and closes one notice on the desktop:
    /// `cargo test -- --ignored shows_a_desktop_notification`.
    #[test]
    #[ignore]
    fn shows_a_desktop_notification() {
        let mut native = Native::new().unwrap();
        println!("actions supported: {}", native.actions);
        let key = "0123456789abcdef0123456789abcdef".to_owned();
        native
            .synchronize(&[Toast {
                progress: None,
                key: key.clone(),
                title: "OpenUUYC 通知测试".into(),
                body: "这是一条测试通知，3 秒后自动关闭。".into(),
                expires_at: chrono::Utc::now().timestamp() + 10,
                buttons: vec![ToastAction {
                    label: "查看".into(),
                    uri: Action::uri(&key, crate::application::app::notifications::Verb::Open),
                }],
            }])
            .unwrap();
        println!("shown as {}", native.shown[&key].1);
        std::thread::sleep(std::time::Duration::from_secs(3));
        native.clear().unwrap();
        assert!(native.shown.is_empty());
    }
}
