//! The parts of the XDND protocol both ends of a drag share: atoms, finding
//! the window under a point that takes drops, client messages and the
//! `text/uri-list` the files travel as.
use anyhow::{Context as _, Result, bail};
use std::path::{Path, PathBuf};
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    Atom, AtomEnum, ClientMessageData, ClientMessageEvent, ConnectionExt as _, EventMask, Window,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

/// The newest protocol version this side speaks.
pub(super) const VERSION: u32 = 5;

x11rb::atom_manager! {
    pub(super) Atoms: AtomsCookie {
        XdndAware,
        XdndProxy,
        XdndSelection,
        XdndEnter,
        XdndPosition,
        XdndStatus,
        XdndLeave,
        XdndDrop,
        XdndFinished,
        XdndTypeList,
        XdndActionCopy,
        XdndActionLink,
        XdndActionAsk,
        TARGETS,
        UTF8_STRING,
        URI_LIST: b"text/uri-list",
        PLAIN_UTF8: b"text/plain;charset=utf-8",
        OPENUUYC_TIME,
        OPENUUYC_DATA,
    }
}

/// A display connection for one drag. XDND is an X11 protocol: a Wayland
/// session's own windows never see it, so it is refused there up front.
pub(super) fn connect() -> Result<(RustConnection, Window, Atoms)> {
    if std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland") {
        bail!("Wayland 桌面暂不支持拖放文件，请使用 Xorg 会话");
    }
    let (connection, screen) = x11rb::connect(None).context("连接 X11 显示失败")?;
    let root = connection.setup().roots[screen].root;
    let atoms = Atoms::new(&connection)?.reply()?;
    Ok((connection, root, atoms))
}

/// A window that takes drops, with where its messages go and the version
/// both sides speak.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) struct Target {
    pub window: Window,
    pub proxy: Window,
    pub version: u32,
}

fn aware(connection: &RustConnection, atoms: &Atoms, window: Window) -> Result<Option<u32>> {
    let reply = connection
        .get_property(false, window, atoms.XdndAware, AtomEnum::ATOM, 0, 1)?
        .reply()?;
    Ok(reply
        .value32()
        .and_then(|mut values| values.next())
        .filter(|version| *version >= 3))
}

/// The innermost window under the root point that advertises XDND. Descends
/// through the window manager's frames the way a pointer event would.
pub(super) fn target_at(
    connection: &RustConnection,
    atoms: &Atoms,
    root: Window,
    x: i32,
    y: i32,
    skip: Window,
) -> Result<Option<Target>> {
    let (x, y) = (
        i16::try_from(x).unwrap_or(i16::MAX),
        i16::try_from(y).unwrap_or(i16::MAX),
    );
    let mut window = root;
    for _ in 0..32 {
        if window != root && window != skip {
            if let Some(version) = aware(connection, atoms, window)? {
                let proxy = connection
                    .get_property(false, window, atoms.XdndProxy, AtomEnum::WINDOW, 0, 1)?
                    .reply()?
                    .value32()
                    .and_then(|mut values| values.next())
                    .filter(|proxy| *proxy != 0);
                // A proxy must name itself, or the property is stale.
                let proxy = proxy.filter(|proxy| {
                    connection
                        .get_property(false, *proxy, atoms.XdndProxy, AtomEnum::WINDOW, 0, 1)
                        .ok()
                        .and_then(|cookie| cookie.reply().ok())
                        .and_then(|reply| reply.value32().and_then(|mut values| values.next()))
                        == Some(*proxy)
                });
                return Ok(Some(Target {
                    window,
                    proxy: proxy.unwrap_or(window),
                    version: version.min(VERSION),
                }));
            }
        }
        let child = connection
            .translate_coordinates(root, window, x, y)?
            .reply()?
            .child;
        if child == x11rb::NONE {
            return Ok(None);
        }
        window = child;
    }
    Ok(None)
}

/// Send one XDND client message about `window` to `to`.
pub(super) fn send(
    connection: &RustConnection,
    to: Window,
    window: Window,
    kind: Atom,
    data: [u32; 5],
) -> Result<()> {
    connection.send_event(
        false,
        to,
        EventMask::NO_EVENT,
        ClientMessageEvent {
            response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window,
            type_: kind,
            data: ClientMessageData::from(data),
        },
    )?;
    Ok(())
}

/// A server timestamp, read from the property change it stamps. The window
/// must select `PropertyChange`.
pub(super) fn timestamp(connection: &RustConnection, atoms: &Atoms, window: Window) -> Result<u32> {
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::PropMode;
    connection.change_property8(
        PropMode::APPEND,
        window,
        atoms.OPENUUYC_TIME,
        AtomEnum::STRING,
        &[],
    )?;
    connection.flush()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    while std::time::Instant::now() < deadline {
        match connection.poll_for_event()? {
            Some(Event::PropertyNotify(event)) if event.window == window => return Ok(event.time),
            Some(_) => {}
            None => std::thread::sleep(std::time::Duration::from_millis(2)),
        }
    }
    Ok(x11rb::CURRENT_TIME)
}

/// Percent-encode a path for a `file:` URI.
fn encode(path: &Path) -> String {
    use std::os::unix::ffi::OsStrExt as _;
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' | b'/' => {
                out.push(*byte as char);
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

/// The paths as RFC 2483 `text/uri-list`.
pub(super) fn uri_list(paths: &[PathBuf]) -> String {
    paths
        .iter()
        .map(|path| encode(path) + "\r\n")
        .collect::<String>()
}

/// Local paths from a `text/uri-list`; anything that is not a local `file:`
/// URI is skipped.
pub(crate) fn parse_uri_list(data: &[u8]) -> Vec<PathBuf> {
    use std::os::unix::ffi::OsStringExt as _;
    String::from_utf8_lossy(data)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|uri| {
            let rest = uri.strip_prefix("file://")?;
            // An authority, if present, has to name this machine.
            let path = if rest.starts_with('/') {
                rest
            } else {
                let (host, path) = rest.split_at(rest.find('/')?);
                if host != "localhost" {
                    return None;
                }
                path
            };
            let bytes = path.as_bytes();
            let mut decoded = Vec::with_capacity(bytes.len());
            let mut index = 0;
            while index < bytes.len() {
                if bytes[index] == b'%'
                    && index + 2 < bytes.len()
                    && let Some(value) = std::str::from_utf8(&bytes[index + 1..index + 3])
                        .ok()
                        .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                {
                    decoded.push(value);
                    index += 3;
                } else {
                    decoded.push(bytes[index]);
                    index += 1;
                }
            }
            let path = PathBuf::from(std::ffi::OsString::from_vec(decoded));
            path.is_absolute().then_some(path)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_lists_round_trip() {
        let paths = vec![
            PathBuf::from("/tmp/a b/文件.txt"),
            PathBuf::from("/run/user/1000/x%y"),
        ];
        assert_eq!(parse_uri_list(uri_list(&paths).as_bytes()), paths);
        assert_eq!(
            parse_uri_list(b"file://localhost/tmp/x\nhttp://a/b\nfile://other/tmp/y\n"),
            vec![PathBuf::from("/tmp/x")]
        );
    }
}
