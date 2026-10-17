//! Temporary XDND handoff target on the remote user desktop: it catches a file
//! drag the controller is carrying off this screen and learns its paths. Only
//! a real file drag can supply them, and the original drag always ends here
//! without anything being done to the files.
use super::{Position, xdnd};
use anyhow::{Context as _, Result, ensure};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use x11rb::connection::Connection;
use x11rb::protocol::Event as XEvent;
use x11rb::protocol::xproto::{
    AtomEnum, ConfigureWindowAux, ConnectionExt as _, CreateWindowAux, EventMask, KeyButMask,
    PropMode, StackMode, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

pub(crate) enum Event {
    Unavailable,
    /// The paths, and whether the original drag is kept for a return. It
    /// never is: this side does not offer `NATIVE_RETURN`.
    Captured(Vec<PathBuf>, bool),
    /// Never sent, as `Original` has no values.
    #[allow(dead_code, reason = "Linux does not offer the drag return.")]
    Resumed(super::original::Original),
    Released,
    Failed(String),
}
pub(crate) struct Portal {
    stop: Arc<AtomicBool>,
}
impl Portal {
    /// `preserve` asks to keep the original drag for a return, which only a
    /// peer that saw `NATIVE_RETURN` from this side does; Linux never sends it.
    pub fn start(
        point: Position,
        _preserve: bool,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let ending = stop.clone();
        std::thread::Builder::new()
            .name("XDND file handoff".into())
            .spawn(move || {
                if let Err(error) = run(point, &ending, &allowed, &notify) {
                    notify(Event::Failed(error.to_string()));
                }
            })?;
        Ok(Self { stop })
    }
    /// A return is only requested when this side offered it, which Linux does
    /// not; the original drag has already ended at the handoff.
    pub fn resume(&self, _point: Position) {}
}
impl Drop for Portal {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
    }
}

/// How long a drag has to reach the handoff before there is taken to be none.
const CAPTURE_WAIT: Duration = Duration::from_millis(600);

struct Handoff<'a> {
    connection: &'a RustConnection,
    atoms: xdnd::Atoms,
    root: Window,
    window: Window,
    /// The source window of the drag that entered, and whether its files are
    /// known.
    source: Option<Window>,
    requested: bool,
    captured: bool,
    left: bool,
    dropped: bool,
}
impl Drop for Handoff<'_> {
    fn drop(&mut self) {
        let _ = self.connection.destroy_window(self.window);
        let _ = self.connection.flush();
    }
}
impl Handoff<'_> {
    /// Once the selection is known, every release must land on this window,
    /// including after a screen move during the handoff.
    fn cover(&self) -> Result<()> {
        let geometry = self.connection.get_geometry(self.root)?.reply()?;
        self.connection.configure_window(
            self.window,
            &ConfigureWindowAux::new()
                .x(0)
                .y(0)
                .width(u32::from(geometry.width.max(1)))
                .height(u32::from(geometry.height.max(1)))
                .stack_mode(StackMode::ABOVE),
        )?;
        self.connection.flush()?;
        Ok(())
    }
    fn status(&self, accept: bool) -> Result<()> {
        if let Some(source) = self.source {
            xdnd::send(
                self.connection,
                source,
                source,
                self.atoms.XdndStatus,
                [
                    self.window,
                    u32::from(accept) | 2,
                    0,
                    0,
                    if accept { self.atoms.XdndActionCopy } else { 0 },
                ],
            )?;
        }
        Ok(())
    }
    fn types(&self, source: Window, data: [u32; 5]) -> Result<Vec<u32>> {
        if data[1] & 1 == 0 {
            return Ok(data[2..].iter().copied().filter(|t| *t != 0).collect());
        }
        Ok(self
            .connection
            .get_property(
                false,
                source,
                self.atoms.XdndTypeList,
                AtomEnum::ATOM,
                0,
                64,
            )?
            .reply()?
            .value32()
            .map(Iterator::collect)
            .unwrap_or_default())
    }
    /// Handle what arrived; the selection's paths when they did.
    fn events(&mut self, allowed: bool) -> Result<Option<Vec<PathBuf>>> {
        let mut paths = None;
        while let Some(event) = self.connection.poll_for_event()? {
            match event {
                XEvent::ClientMessage(message) => {
                    let data = message.data.as_data32();
                    let atoms = &self.atoms;
                    if message.type_ == atoms.XdndEnter {
                        self.source = Some(data[0]);
                        self.left = false;
                        if !self.requested && self.types(data[0], data)?.contains(&atoms.URI_LIST) {
                            self.connection.convert_selection(
                                self.window,
                                atoms.XdndSelection,
                                atoms.URI_LIST,
                                atoms.OPENUUYC_DATA,
                                x11rb::CURRENT_TIME,
                            )?;
                            self.requested = true;
                        }
                    } else if message.type_ == atoms.XdndPosition && self.source == Some(data[0]) {
                        self.left = false;
                        self.status(allowed && self.requested)?;
                    } else if message.type_ == atoms.XdndLeave && self.source == Some(data[0]) {
                        if self.captured {
                            self.left = true;
                        }
                    } else if message.type_ == atoms.XdndDrop && self.source == Some(data[0]) {
                        self.dropped = allowed;
                        self.left = true;
                        // Nothing is done with the files: the original drag
                        // ends with no action.
                        xdnd::send(
                            self.connection,
                            data[0],
                            data[0],
                            atoms.XdndFinished,
                            [self.window, 0, 0, 0, 0],
                        )?;
                    }
                }
                XEvent::SelectionNotify(event) if event.requestor == self.window => {
                    if event.property == x11rb::NONE {
                        continue;
                    }
                    let reply = self
                        .connection
                        .get_property(true, self.window, event.property, AtomEnum::ANY, 0, 1 << 20)?
                        .reply()?;
                    let found = xdnd::parse_uri_list(&reply.value);
                    if !found.is_empty() {
                        paths = Some(found);
                    }
                }
                _ => {}
            }
        }
        self.connection.flush()?;
        Ok(paths)
    }
}

fn run(
    point: Position,
    stop: &AtomicBool,
    allowed: &Arc<dyn Fn() -> bool + Send + Sync>,
    notify: &Arc<dyn Fn(Event) + Send + Sync>,
) -> Result<()> {
    ensure!(allowed(), "拖出未获许可");
    let (connection, root, atoms) = xdnd::connect()?;
    connection
        .xtest_get_version(2, 2)?
        .reply()
        .context("X 服务器不支持 XTEST")?;
    let window = connection.generate_id()?;
    connection.create_window(
        x11rb::COPY_DEPTH_FROM_PARENT,
        window,
        root,
        (point.x - 8).clamp(i16::MIN.into(), i16::MAX.into()) as i16,
        (point.y - 8).clamp(i16::MIN.into(), i16::MAX.into()) as i16,
        17,
        17,
        0,
        WindowClass::INPUT_ONLY,
        x11rb::COPY_FROM_PARENT,
        &CreateWindowAux::new()
            .override_redirect(1)
            .event_mask(EventMask::PROPERTY_CHANGE),
    )?;
    let mut handoff = Handoff {
        connection: &connection,
        atoms,
        root,
        window,
        source: None,
        requested: false,
        captured: false,
        left: false,
        dropped: false,
    };
    connection.change_property32(
        PropMode::REPLACE,
        window,
        atoms.XdndAware,
        AtomEnum::ATOM,
        &[xdnd::VERSION],
    )?;
    connection.map_window(window)?;
    connection.configure_window(
        window,
        &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE),
    )?;
    connection.xtest_fake_input(
        x11rb::protocol::xproto::MOTION_NOTIFY_EVENT,
        0,
        x11rb::CURRENT_TIME,
        root,
        point.x.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
        point.y.clamp(i16::MIN.into(), i16::MAX.into()) as i16,
        0,
    )?;
    connection.flush()?;
    let started = Instant::now();
    let mut released_at: Option<Instant> = None;
    let mut raised_after_leave = false;
    loop {
        let permitted = !stop.load(Ordering::Acquire) && allowed();
        ensure!(permitted, "文件接管已取消");
        if let Some(paths) = handoff.events(permitted)?
            && !handoff.captured
        {
            handoff.captured = true;
            handoff.cover()?;
            notify(Event::Captured(paths, false));
        }
        let held = connection
            .query_pointer(root)?
            .reply()?
            .mask
            .contains(KeyButMask::BUTTON1);
        if handoff.captured {
            if !held {
                if handoff.dropped {
                    notify(Event::Released);
                    return Ok(());
                }
                ensure!(!handoff.left, "原文件拖动已取消");
                // The release can reach us before the source's drop message.
                // Keep the handoff until that message or its bounded wait.
                let released = released_at.get_or_insert_with(Instant::now);
                ensure!(
                    released.elapsed() < Duration::from_millis(500),
                    "原文件拖动未确认结束"
                );
            } else {
                released_at = None;
                if !handoff.left {
                    raised_after_leave = false;
                } else if !raised_after_leave {
                    handoff.cover()?;
                    raised_after_leave = true;
                }
            }
        } else if !held || started.elapsed() > CAPTURE_WAIT {
            notify(Event::Unavailable);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run while a file drag is in progress on the test server at
    /// OPENUUYC_DND_PORTAL (`x,y`); release the button afterwards.
    #[test]
    #[ignore = "needs an X display with a file drag in progress"]
    fn captures_drag() {
        let at = std::env::var("OPENUUYC_DND_PORTAL").unwrap();
        let (x, y) = at.split_once(',').unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let _portal = Portal::start(
            Position {
                x: x.parse().unwrap(),
                y: y.parse().unwrap(),
            },
            false,
            Arc::new(|| true),
            Arc::new(move |event| {
                let _ = tx.send(match event {
                    Event::Unavailable => "unavailable".to_owned(),
                    Event::Captured(paths, _) => format!("captured {paths:?}"),
                    Event::Resumed(original) => match original {},
                    Event::Released => "released".to_owned(),
                    Event::Failed(error) => format!("failed {error}"),
                });
            }),
        )
        .unwrap();
        let first = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        eprintln!("{first}");
        assert!(first.starts_with("captured"), "{first}");
        let second = rx.recv_timeout(Duration::from_secs(20)).unwrap();
        eprintln!("{second}");
        assert_eq!(second, "released");
    }
}
