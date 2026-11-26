//! Gamescope startup and host credential-service integration.
//!
//! This runs before graphics or account records are opened. A portable data
//! directory must not hide the compositor socket, and the shared system wallet
//! must remain a host service rather than a Steam-tracked application child.

use anyhow::{Context, Result, bail, ensure};
use futures_util::{FutureExt, StreamExt};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use winit::dpi::PhysicalPosition;
use winit::window::Window;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

const SECRET_SERVICE: &str = "org.freedesktop.secrets";
const SECRET_PATH: &str = "/org/freedesktop/secrets";
const KWALLET_SERVICE: &str = "org.kde.secretservicecompat";

fn kwallet_fallback_allowed(error_name: Option<&str>, services: &[String]) -> bool {
    error_name == Some("org.freedesktop.DBus.Error.ServiceUnknown")
        && !services.iter().any(|name| name == SECRET_SERVICE)
        && services.iter().any(|name| name == KWALLET_SERVICE)
}

fn is_kwallet_owner(owner: &str, compatibility_owner: Option<&str>) -> bool {
    owner.starts_with(':') && compatibility_owner == Some(owner)
}

/// Absolute names keep working after a launcher redirects XDG_RUNTIME_DIR.
fn socket_path(runtime: &Path, name: &std::ffi::OsStr) -> PathBuf {
    runtime.join(name)
}

fn graphical_environment(read: impl Fn(&str) -> Option<String>) -> HashMap<&'static str, String> {
    [
        "DISPLAY",
        "WAYLAND_DISPLAY",
        "XAUTHORITY",
        "XDG_SESSION_TYPE",
        "GAMESCOPE_WAYLAND_DISPLAY",
    ]
    .into_iter()
    .map(|key| (key, read(key).unwrap_or_default()))
    .collect()
}

/// Called by bootstrap before argument parsing or application worker creation.
/// Outside Gamescope the environment is left exactly as the session set it:
/// children (terminal shells, opened files) inherit it.
pub(crate) fn initialize_environment() {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })));
    // `active()` needs the Gamescope socket as a path, so resolve it first.
    for key in ["GAMESCOPE_WAYLAND_DISPLAY", "WAYLAND_DISPLAY"] {
        if key == "WAYLAND_DISPLAY" && !active() {
            break;
        }
        if let Some(name) = std::env::var_os(key).filter(|name| !name.is_empty()) {
            let mut path = socket_path(&runtime, &name);
            if !Path::new(&name).is_absolute() && !path.exists() {
                // Also accept a launcher that already moved its own runtime
                // directory without first making the display names absolute.
                let host = PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }));
                let candidate = socket_path(&host, &name);
                if candidate.exists() {
                    path = candidate;
                }
            }
            // SAFETY: native main invokes bootstrap before spawning threads.
            unsafe { std::env::set_var(key, path) };
        }
    }
    if active() && std::env::var_os("WINIT_X11_SCALE_FACTOR").is_none() {
        // Gamescope's synthetic RandR physical dimensions are not the handheld's
        // DPI. They otherwise scale windows and input by different factors.
        // SAFETY: as above, no application worker has been created yet.
        unsafe { std::env::set_var("WINIT_X11_SCALE_FACTOR", "1") };
    }
}

pub(crate) fn active() -> bool {
    use std::os::unix::fs::FileTypeExt as _;
    std::env::var_os("GAMESCOPE_WAYLAND_DISPLAY")
        .filter(|name| !name.is_empty())
        .and_then(|path| std::fs::metadata(path).ok())
        .is_some_and(|metadata| metadata.file_type().is_socket())
}

/// The control center includes its custom caption in the client size. Fit it
/// once on creation; viewer windows and small floating panels keep their sizes.
pub(crate) fn fit_control_center(window: &Window) {
    if !active() {
        return;
    }
    if let Some(monitor) = window
        .current_monitor()
        .or_else(|| window.available_monitors().next())
    {
        let size = monitor.size();
        if size.width != 0 && size.height != 0 {
            let _ = window.request_inner_size(size);
            let origin = monitor.position();
            window.set_outer_position(PhysicalPosition::new(origin.x, origin.y));
        }
    }
}

/// Gamescope may have no desktop session to activate and unlock a wallet.
/// Help/version and repeat-instance activation never reach this GUI hook.
/// Best effort: the keyring itself reports a wallet it cannot use when the
/// login is read, so a failure here never keeps the program from starting.
pub(crate) fn prepare_wallet() {
    if active()
        && let Err(error) = pollster::block_on(prepare_wallet_async())
    {
        tracing::warn!(error = %format!("{error:#}"), "准备系统钱包失败");
    }
}

async fn activate_wallet(bus: &zbus::Proxy<'_>) -> Result<String> {
    let available: bool = bus.call("NameHasOwner", &(SECRET_SERVICE,)).await?;
    if !available {
        // Do not import HOME or XDG data paths: the credential daemon must keep
        // using its host wallet even when this client is running portably.
        let graphical = graphical_environment(|key| std::env::var(key).ok());
        bus.call::<_, _, ()>("UpdateActivationEnvironment", &(graphical,))
            .await?;
        // Always honor the configured default, even when KWallet is installed.
        // Only a missing activation entry permits the KDE compatibility alias;
        // a broken or denied default must not silently select another wallet.
        if let Err(error) = bus
            .call::<_, _, u32>("StartServiceByName", &(SECRET_SERVICE, 0u32))
            .await
        {
            let error_name = match &error {
                zbus::Error::MethodError(name, _, _) => Some(name.as_str()),
                _ => None,
            };
            if error_name != Some("org.freedesktop.DBus.Error.ServiceUnknown") {
                return Err(error.into());
            }
            let services: Vec<String> = bus.call("ListActivatableNames", &()).await?;
            if !kwallet_fallback_allowed(error_name, &services) {
                return Err(error.into());
            }
            // A provider may have appeared while activation entries were read.
            let available: bool = bus.call("NameHasOwner", &(SECRET_SERVICE,)).await?;
            if !available {
                let _: u32 = bus
                    .call("StartServiceByName", &(KWALLET_SERVICE, 0u32))
                    .await?;
            }
        }
    }
    // The compatibility alias can exist with Secret Service disabled. Require
    // the real interface, then pin all operations to this unique bus owner.
    bus.call("GetNameOwner", &(SECRET_SERVICE,))
        .await
        .context("系统钱包未提供 Secret Service 接口")
}

async fn prepare_wallet_async() -> Result<()> {
    let connection = zbus::Connection::session()
        .await
        .context("连接会话总线失败")?;
    let bus = zbus::Proxy::new(
        &connection,
        "org.freedesktop.DBus",
        "/org/freedesktop/DBus",
        "org.freedesktop.DBus",
    )
    .await?;
    let owner = activate_wallet(&bus).await?;
    let secrets = zbus::Proxy::new(
        &connection,
        owner.as_str(),
        SECRET_PATH,
        "org.freedesktop.Secret.Service",
    )
    .await?;
    let collection: OwnedObjectPath = secrets.call("ReadAlias", &("default",)).await?;
    if collection.as_str() == "/" {
        // A new account still uses the provider's ordinary first-use flow.
        return Ok(());
    }
    let properties = zbus::Proxy::new(
        &connection,
        owner.as_str(),
        collection.as_str(),
        "org.freedesktop.DBus.Properties",
    )
    .await?;
    let locked: OwnedValue = properties
        .call("Get", &("org.freedesktop.Secret.Collection", "Locked"))
        .await?;
    if !bool::try_from(locked).context("读取钱包锁定状态失败")? {
        return Ok(());
    }
    let (_, prompt): (Vec<OwnedObjectPath>, OwnedObjectPath) =
        secrets.call("Unlock", &(vec![collection.clone()],)).await?;
    if prompt.as_str() == "/" {
        return Ok(());
    }
    let prompt_proxy = zbus::Proxy::new(
        &connection,
        owner.as_str(),
        prompt.as_str(),
        "org.freedesktop.Secret.Prompt",
    )
    .await?;
    // Subscribe before Prompt: an already-unlocked provider may finish at once.
    let mut completed = prompt_proxy.receive_signal("Completed").await?;
    let mut windows = kwallet_windows(&bus, &owner).await;
    prompt_proxy.call::<_, _, ()>("Prompt", &("",)).await?;
    let deadline = Instant::now() + Duration::from_secs(180);
    loop {
        if let Some(message) = completed.next().now_or_never() {
            let message = message.context("系统钱包在解锁前断开")?;
            if message.header().sender().map(|name| name.as_str()) != Some(owner.as_str()) {
                continue;
            }
            let (dismissed, _): (bool, OwnedValue) = message.body().deserialize()?;
            ensure!(!dismissed, "系统钱包解锁已取消");
            return Ok(());
        }
        if Instant::now() >= deadline {
            // Dismiss only this prompt, leaving the shared wallet service alone.
            let _ = prompt_proxy.call::<_, _, ()>("Dismiss", &()).await;
            bail!("系统钱包解锁超时");
        }
        if let Some(windows) = &mut windows {
            windows.attach();
        }
        // This private bootstrap loop runs before the GUI or Tokio workers.
        // zbus dispatches on its own async-io executor while we wait for a user.
        std::thread::sleep(Duration::from_millis(100));
    }
}

async fn kwallet_windows(bus: &zbus::Proxy<'_>, owner: &str) -> Option<KWalletWindows> {
    // KWallet registers both names on the same connection. Other providers can
    // delegate prompts to a separate process, so their PID is not a UI identity.
    let compatibility_owner: String = bus.call("GetNameOwner", &(KWALLET_SERVICE,)).await.ok()?;
    if !is_kwallet_owner(owner, Some(&compatibility_owner)) {
        return None;
    }
    let pid: u32 = bus
        .call("GetConnectionUnixProcessID", &(owner,))
        .await
        .ok()?;
    KWalletWindows::new(pid).ok()
}

struct KWalletWindows {
    connection: x11rb::rust_connection::RustConnection,
    root: u32,
    pid: u32,
    pid_atom: u32,
    steam_atom: u32,
    appid: u32,
    seen: HashSet<u32>,
}

impl KWalletWindows {
    fn new(pid: u32) -> Result<Self> {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::ConnectionExt as _;
        let appid = std::env::var("SteamAppId")?.parse::<u32>()?;
        ensure!(appid != 0, "没有 Steam 应用 ID");
        let (connection, screen) = x11rb::connect(None)?;
        let root = connection.setup().roots[screen].root;
        let pid_atom = connection.intern_atom(false, b"_NET_WM_PID")?.reply()?.atom;
        let steam_atom = connection.intern_atom(false, b"STEAM_GAME")?.reply()?.atom;
        Ok(Self {
            connection,
            root,
            pid,
            pid_atom,
            steam_atom,
            appid,
            seen: HashSet::new(),
        })
    }

    fn attach(&mut self) {
        use x11rb::connection::Connection as _;
        use x11rb::protocol::xproto::{
            AtomEnum, ConfigureWindowAux, ConnectionExt as _, InputFocus, MapState, PropMode,
            StackMode,
        };
        use x11rb::wrapper::ConnectionExt as _;
        let Some(tree) = self
            .connection
            .query_tree(self.root)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
        else {
            return;
        };
        for window in tree.children {
            if self.seen.contains(&window) {
                continue;
            }
            let result = (|| -> Result<()> {
                let attributes = self.connection.get_window_attributes(window)?.reply()?;
                let geometry = self.connection.get_geometry(window)?.reply()?;
                if attributes.map_state != MapState::VIEWABLE
                    || geometry.width < 100
                    || geometry.height < 40
                {
                    return Ok(());
                }
                let property = self
                    .connection
                    .get_property(false, window, self.pid_atom, AtomEnum::CARDINAL, 0, 1)?
                    .reply()?;
                if property.value32().and_then(|mut values| values.next()) != Some(self.pid) {
                    return Ok(());
                }
                self.connection
                    .change_property32(
                        PropMode::REPLACE,
                        window,
                        self.steam_atom,
                        AtomEnum::CARDINAL,
                        &[self.appid],
                    )?
                    .check()?;
                self.connection
                    .configure_window(
                        window,
                        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
                    )?
                    .check()?;
                self.connection
                    .set_input_focus(InputFocus::PARENT, window, x11rb::CURRENT_TIME)?
                    .check()?;
                self.connection.flush()?;
                self.seen.insert(window);
                Ok(())
            })();
            if let Err(error) = result {
                tracing::debug!(%error, "KWallet dialog is not ready for Gamescope focus");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wallet_activation_keeps_host_credential_storage() {
        let environment = HashMap::from([
            ("DISPLAY", ":1"),
            ("GAMESCOPE_WAYLAND_DISPLAY", "/run/user/1000/gamescope-0"),
            ("HOME", "/portable/home"),
            ("XDG_DATA_HOME", "/portable/data"),
            ("XDG_RUNTIME_DIR", "/portable/runtime"),
        ]);
        let activation =
            graphical_environment(|key| environment.get(key).map(|value| value.to_string()));
        assert_eq!(activation["DISPLAY"], ":1");
        assert_eq!(
            activation["GAMESCOPE_WAYLAND_DISPLAY"],
            "/run/user/1000/gamescope-0"
        );
        assert_eq!(activation["WAYLAND_DISPLAY"], "");
        for key in ["HOME", "XDG_DATA_HOME", "XDG_RUNTIME_DIR"] {
            assert!(!activation.contains_key(key));
        }
    }

    #[test]
    fn wallet_fallback_never_replaces_a_configured_provider() {
        let kde = vec![KWALLET_SERVICE.to_string()];
        let both = vec![SECRET_SERVICE.to_string(), KWALLET_SERVICE.to_string()];
        let unknown = Some("org.freedesktop.DBus.Error.ServiceUnknown");
        assert!(kwallet_fallback_allowed(unknown, &kde));
        assert!(!kwallet_fallback_allowed(unknown, &both));
        assert!(!kwallet_fallback_allowed(unknown, &[]));
        for error in [
            None,
            Some("org.freedesktop.DBus.Error.AccessDenied"),
            Some("org.freedesktop.DBus.Error.TimedOut"),
            Some("org.freedesktop.DBus.Error.Spawn.ChildExited"),
        ] {
            assert!(!kwallet_fallback_allowed(error, &kde));
        }
    }

    #[test]
    fn wallet_window_workaround_requires_the_same_kwallet_connection() {
        assert!(is_kwallet_owner(":1.42", Some(":1.42")));
        assert!(!is_kwallet_owner(":1.42", Some(":1.43")));
        assert!(!is_kwallet_owner(":1.42", None));
        assert!(!is_kwallet_owner("", Some("")));
        assert!(!is_kwallet_owner(SECRET_SERVICE, Some(SECRET_SERVICE)));
    }
}
