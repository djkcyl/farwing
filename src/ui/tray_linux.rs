//! Notification area integration through the freedesktop StatusNotifierItem
//! protocol, with the same entries as the Windows tray: activating the icon
//! or "打开 OpenUUYC" shows the control center, "退出" ends the run.
//!
//! GNOME shows these icons through the AppIndicator extension (enabled on
//! Ubuntu); KDE, Xfce and most other panels show them natively. Without a
//! watcher the tray cannot be created, and the caller keeps the window's
//! ordinary close behaviour instead of hiding it somewhere unreachable.
use anyhow::{Context, Result};
use ksni::blocking::TrayMethods;
use std::sync::atomic::{AtomicBool, Ordering};

pub(super) struct Tray {
    handle: ksni::blocking::Handle<Item>,
    /// 托盘服务已经接受的被控状态。每一趟 UI 都会报一次，只有变了才去改图标。
    controlled: AtomicBool,
}

struct Item {
    controlled: bool,
}

fn send(request: super::window_manager::Request) {
    let _ = super::window_manager::send(request);
}

impl ksni::Tray for Item {
    fn id(&self) -> String {
        "openuuyc".into()
    }
    fn title(&self) -> String {
        "OpenUUYC".into()
    }
    fn icon_pixmap(&self) -> Vec<ksni::Icon> {
        vec![icon(self.controlled)]
    }
    fn activate(&mut self, _x: i32, _y: i32) {
        send(super::window_manager::Request::ShowMain);
    }
    fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
        use ksni::menu::StandardItem;
        vec![
            StandardItem {
                label: "打开 OpenUUYC".into(),
                activate: Box::new(|_| send(super::window_manager::Request::ShowMain)),
                ..Default::default()
            }
            .into(),
            ksni::MenuItem::Separator,
            StandardItem {
                label: "退出".into(),
                activate: Box::new(|_| send(super::window_manager::Request::Exit)),
                ..Default::default()
            }
            .into(),
        ]
    }
}

/// The application icon as SNI wants it: ARGB32, big-endian. `controlled`
/// adds the red corner badge, the same artwork as the Windows tray.
fn icon(controlled: bool) -> ksni::Icon {
    let image = crate::ui::branding::tray_pixmap(64, controlled);
    let (width, height) = image.dimensions();
    let mut data = image.into_raw();
    for pixel in data.chunks_exact_mut(4) {
        pixel.rotate_right(1);
    }
    ksni::Icon {
        width: width as i32,
        height: height as i32,
        data,
    }
}

impl Tray {
    pub fn new() -> Result<Self> {
        let handle = Item { controlled: false }
            .spawn()
            .context("桌面没有可用的托盘（StatusNotifierItem）")?;
        Ok(Self {
            handle,
            controlled: AtomicBool::new(false),
        })
    }

    /// 被控角标：这台机器正被别人控制时，托盘图标右上角多一个红三角。
    pub(super) fn set_controlled(&self, controlled: bool) {
        if self.controlled.load(Ordering::Relaxed) == controlled {
            return;
        }
        // 托盘服务已经退出时更新不会生效，状态留着不记，下一趟 UI 再试。
        if self
            .handle
            .update(|item| item.controlled = controlled)
            .is_some()
        {
            self.controlled.store(controlled, Ordering::Relaxed);
        } else {
            tracing::debug!(controlled, "tray icon update was not accepted");
        }
    }
}

impl Drop for Tray {
    fn drop(&mut self) {
        self.handle.shutdown().wait();
    }
}

#[cfg(test)]
mod tests {
    /// 角标只染右上角，ARGB 里 alpha 在第一个字节；左下角与不带角标的那份一样。
    #[test]
    fn the_badge_marks_only_the_top_right_corner() {
        let idle = super::icon(false);
        let controlled = super::icon(true);
        assert_eq!((idle.width, idle.height), (64, 64));
        let pixel = |icon: &ksni::Icon, x: usize, y: usize| {
            icon.data[(y * icon.width as usize + x) * 4..][..4].to_vec()
        };
        let corner = pixel(&controlled, 54, 11);
        assert_eq!(corner[0], 255);
        assert_eq!(&corner[1..], &[232, 62, 74]);
        assert_ne!(pixel(&idle, 54, 11), corner);
        assert_eq!(pixel(&idle, 8, 56), pixel(&controlled, 8, 56));
    }
}
