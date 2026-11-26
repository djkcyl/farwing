//! Keeps the display on and the machine out of idle sleep while a screen is
//! captured or a viewing session is live, like the Windows power request.
//! The desktop's idle policy is reached through
//! `org.freedesktop.ScreenSaver.Inhibit` on the session bus (GNOME, KDE,
//! Xfce, hypridle and others honour it); the inhibition is also released if
//! the bus connection closes.
//!
//! Capture also wakes a display that is already off, without synthetic
//! input: GNOME's power-save mode is lifted through Mutter, other desktops
//! (KDE, Xfce, xscreensaver) take `SimulateUserActivity`, and on X11 the
//! screen saver is reset and a monitor DPMS powered down is turned back on.
use anyhow::{Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedValue, Value};

pub(crate) struct Request {
    connection: Connection,
    cookie: u32,
}

impl Request {
    pub(crate) fn capture() -> Result<Self> {
        let request = Self::new("OpenUUYC 正在采集远程桌面");
        wake_display(request.as_ref().ok().map(|request| &request.connection));
        request
    }

    pub(crate) fn viewing() -> Result<Self> {
        Self::new("OpenUUYC 正在进行远程观看会话")
    }

    fn new(reason: &str) -> Result<Self> {
        let connection = Connection::session().context("连接会话总线失败")?;
        let cookie: u32 = screensaver(&connection)?
            .call("Inhibit", &("OpenUUYC", reason))
            .context("桌面未接受阻止息屏与自动休眠的请求")?;
        tracing::debug!(cookie, "session display/idle inhibition acquired");
        Ok(Self { connection, cookie })
    }
}

fn screensaver(connection: &Connection) -> Result<Proxy<'static>> {
    Proxy::new(
        connection,
        "org.freedesktop.ScreenSaver",
        "/org/freedesktop/ScreenSaver",
        "org.freedesktop.ScreenSaver",
    )
    .context("桌面未提供 org.freedesktop.ScreenSaver 接口")
}

/// Each way is tried; a desktop that does not have one simply skips it.
fn wake_display(session: Option<&Connection>) {
    let mut woken = Vec::new();
    if let Some(connection) = session {
        match wake_mutter(connection) {
            Ok(true) => woken.push("Mutter"),
            Ok(false) => {}
            Err(error) => {
                tracing::debug!(error = %format!("{error:#}"), "Mutter display wake failed")
            }
        }
        if screensaver(connection)
            .and_then(|proxy| Ok(proxy.call::<_, _, ()>("SimulateUserActivity", &())?))
            .is_ok()
        {
            woken.push("SimulateUserActivity");
        }
    }
    if !super::capture::wayland_session() {
        match wake_x11() {
            Ok(true) => woken.push("X11 DPMS"),
            Ok(false) => {}
            Err(error) => tracing::debug!(error = %format!("{error:#}"), "X11 display wake failed"),
        }
    }
    tracing::debug!(?woken, "capture display wake requested");
}

/// GNOME blanks through Mutter's power-save mode, which only Mutter lifts.
fn wake_mutter(connection: &Connection) -> Result<bool> {
    const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
    let properties = Proxy::new(
        connection,
        INTERFACE,
        "/org/gnome/Mutter/DisplayConfig",
        "org.freedesktop.DBus.Properties",
    )?;
    let Ok(mode) = properties.call::<_, _, OwnedValue>("Get", &(INTERFACE, "PowerSaveMode")) else {
        return Ok(false);
    };
    if i32::try_from(mode).is_ok_and(|mode| mode == 0) {
        return Ok(false);
    }
    properties
        .call::<_, _, ()>("Set", &(INTERFACE, "PowerSaveMode", Value::from(0i32)))
        .context("设置 Mutter 显示电源模式失败")?;
    Ok(true)
}

fn wake_x11() -> Result<bool> {
    use x11rb::connection::{Connection as _, RequestConnection as _};
    use x11rb::protocol::dpms::{self, ConnectionExt as _, DPMSMode};
    use x11rb::protocol::xproto::{ConnectionExt as _, ScreenSaver};
    let (connection, _) = x11rb::connect(None).context("连接 X11 显示失败")?;
    // Unblanks the core screen saver and restarts its timer; it does not
    // power a monitor back up.
    connection.force_screen_saver(ScreenSaver::RESET)?.check()?;
    let mut woken = false;
    if connection
        .extension_information(dpms::X11_EXTENSION_NAME)?
        .is_some()
    {
        let info = connection.dpms_info()?.reply()?;
        if info.state && info.power_level != DPMSMode::ON {
            connection.dpms_force_level(DPMSMode::ON)?.check()?;
            woken = true;
        }
    }
    connection.flush()?;
    Ok(woken)
}

impl Drop for Request {
    fn drop(&mut self) {
        let released = screensaver(&self.connection).and_then(|proxy| {
            proxy
                .call::<_, _, ()>("UnInhibit", &(self.cookie,))
                .context("UnInhibit")
        });
        match released {
            Ok(()) => tracing::debug!("session display/idle inhibition released"),
            // Closing the connection with this value releases it anyway.
            Err(error) => tracing::warn!(error = %format!("{error:#}"),
                "session display/idle inhibition release failed"),
        }
    }
}

#[cfg(test)]
mod tests {
    /// Needs a desktop session bus. On GNOME, also checks the session
    /// manager lists this idle inhibition only while the request is held.
    #[test]
    #[ignore]
    fn viewing_holds_the_idle_inhibition_until_dropped() {
        use zbus::{blocking::Proxy, zvariant::OwnedObjectPath};
        let connection = zbus::blocking::Connection::session().expect("session bus");
        let ours = || -> Option<usize> {
            let manager = Proxy::new(
                &connection,
                "org.gnome.SessionManager",
                "/org/gnome/SessionManager",
                "org.gnome.SessionManager",
            )
            .ok()?;
            let inhibitors: Vec<OwnedObjectPath> = manager.call("GetInhibitors", &()).ok()?;
            Some(
                inhibitors
                    .into_iter()
                    .filter(|path| {
                        Proxy::new(
                            &connection,
                            "org.gnome.SessionManager",
                            path.as_ref(),
                            "org.gnome.SessionManager.Inhibitor",
                        )
                        .and_then(|inhibitor| {
                            let reason: String = inhibitor.call("GetReason", &())?;
                            let flags: u32 = inhibitor.call("GetFlags", &())?;
                            Ok(reason == "OpenUUYC 正在进行远程观看会话" && flags & 8 != 0)
                        })
                        .unwrap_or(false)
                    })
                    .count(),
            )
        };
        let before = ours();
        let request = super::Request::viewing().expect("inhibit");
        let held = ours();
        drop(request);
        let after = ours();
        println!(
            "GNOME idle inhibitors from us: before {before:?}, held {held:?}, after {after:?}"
        );
        if let (Some(before), Some(held), Some(after)) = (before, held, after) {
            assert_eq!(held, before + 1);
            assert_eq!(after, before);
        }
    }

    /// Turns this machine's monitor off for a moment. Needs the desktop
    /// session bus and `DISPLAY`; on GNOME it blanks through Mutter, and on
    /// X11 also through DPMS with `xset`.
    #[test]
    #[ignore]
    fn capture_wakes_a_blanked_display() {
        use zbus::zvariant::{OwnedValue, Value};
        let connection = zbus::blocking::Connection::session().expect("session bus");
        let properties = zbus::blocking::Proxy::new(
            &connection,
            "org.gnome.Mutter.DisplayConfig",
            "/org/gnome/Mutter/DisplayConfig",
            "org.freedesktop.DBus.Properties",
        )
        .unwrap();
        const INTERFACE: &str = "org.gnome.Mutter.DisplayConfig";
        let mutter = || -> Option<i32> {
            let mode: OwnedValue = properties.call("Get", &(INTERFACE, "PowerSaveMode")).ok()?;
            i32::try_from(mode).ok()
        };
        let pause = || std::thread::sleep(std::time::Duration::from_millis(500));
        if mutter().is_some() {
            properties
                .call::<_, _, ()>("Set", &(INTERFACE, "PowerSaveMode", Value::from(3i32)))
                .unwrap();
            pause();
            assert_eq!(mutter(), Some(3));
            let request = super::Request::capture().expect("inhibit");
            pause();
            let woken = mutter();
            drop(request);
            println!("Mutter power-save mode after capture: {woken:?}");
            assert_eq!(woken, Some(0));
        }
        let xset = |args: &[&str]| {
            std::process::Command::new("xset")
                .args(args)
                .output()
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        };
        if !crate::platform::linux::capture::wayland_session() && xset(&["q"]).is_some() {
            xset(&["dpms", "force", "off"]);
            pause();
            assert!(xset(&["q"]).unwrap().contains("Monitor is Off"));
            let request = super::Request::capture().expect("inhibit");
            pause();
            let state = xset(&["q"]).unwrap();
            drop(request);
            println!(
                "X11 DPMS after capture: on = {}",
                state.contains("Monitor is On")
            );
            assert!(state.contains("Monitor is On"));
        }
    }
}
