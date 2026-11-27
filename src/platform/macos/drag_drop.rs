//! Desktop drag source/sink, viewer-only: macOS has no drag-drop integration
//! in this port. A drag placed by the peer refuses to start and the desktop
//! catcher/handoff report unavailable, so the connection falls back to what
//! the protocol alone offers.
pub(crate) mod send_target;

use anyhow::{Result, bail};
use std::{
    path::PathBuf,
    sync::{Arc, Mutex, atomic::{AtomicBool, AtomicU32, Ordering}},
};

/// What a drag offers: local paths, and whatever keeps them readable for as
/// long as the drag needs them.
pub(crate) struct Files {
    pub paths: Vec<PathBuf>,
    pub _hold: Arc<dyn std::any::Any + Send + Sync>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Position {
    pub x: i32,
    pub y: i32,
}
/// Whether this side can hand a file drag back to its original gesture when
/// it re-enters the viewer. Not offered on macOS.
pub(crate) const RETURN_CAPABLE: bool = false;

/// Whether the left mouse button is down on the local display.
pub(crate) fn left_held() -> bool {
    false
}

/// Only used once a returned drag is restored, which `RETURN_CAPABLE` rules
/// out here.
pub(crate) fn wake_viewer(_owner: u64) {}

#[derive(Debug)]
pub(crate) enum Event {
    Feedback(u32),
    Released,
    Finished {
        accepted: bool,
        effect: u32,
        error: Option<String>,
    },
}

/// The only effect offered: the receiving side copies the files.
pub(crate) const COPY: u32 = 1;

pub(crate) struct Session;
impl Session {
    /// A drag placed by the peer; the macOS desktop cannot be driven yet.
    pub fn start(
        _point: Position,
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        _object: impl FnOnce() -> Result<Files> + Send + 'static,
        _notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        anyhow::ensure!(allowed(), "拖放未获许可");
        bail!("macOS 暂不支持向本机拖出文件")
    }
    /// A drag the local user is holding; there is no way to follow one yet.
    pub fn physical(
        allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        _object: impl FnOnce() -> Result<Files> + Send + 'static,
        _notify: Arc<dyn Fn(Event) + Send + Sync>,
    ) -> Result<Self> {
        anyhow::ensure!(allowed(), "拖放未获许可");
        bail!("macOS 暂不支持向本机拖出文件")
    }
    pub fn position(&self, _point: Position) {}
    pub fn commit(&self, _point: Position) -> Result<()> {
        bail!("macOS 暂不支持向本机拖出文件")
    }
    pub fn cancel(&self) {}
}
impl Drop for Session {
    fn drop(&mut self) {
        self.cancel();
    }
}

/// Drags this process is running; none ever start here.
static OWN_DRAGS: AtomicU32 = AtomicU32::new(0);
pub(super) fn own_drag() -> bool {
    OWN_DRAGS.load(Ordering::Acquire) != 0
}

/// The X11 peer-drag handoff (`portal` on Linux); unavailable here.
pub(crate) mod portal {
    use super::*;

    pub(crate) enum Event {
        Unavailable,
        Captured(Vec<PathBuf>, bool),
        #[allow(dead_code, reason = "macOS does not offer the drag return.")]
        Resumed(super::original::Original),
        Released,
        Failed(String),
    }
    pub(crate) struct Portal;
    impl Portal {
        pub fn start(
            _point: Position,
            _preserve: bool,
            _allowed: Arc<dyn Fn() -> bool + Send + Sync>,
            notify: Arc<dyn Fn(Event) + Send + Sync>,
        ) -> Result<Self> {
            notify(Event::Unavailable);
            bail!("macOS 暂不支持接取本机拖放")
        }
        pub fn resume(&self, _point: Position) {}
    }
    impl Drop for Portal {
        fn drop(&mut self) {}
    }
}

/// The retained original gesture of a drag the peer placed; never exists.
pub(crate) mod original {
    #[derive(Clone, Copy)]
    pub(crate) enum Original {}
    impl Original {
        pub fn live(&self) -> bool {
            match *self {}
        }
        pub fn cancel(&self) -> anyhow::Result<()> {
            match *self {}
        }
    }
}

/// The drag preview/identity hooks; no desktop integration, so the drag goes
/// out with no image and no identity, which the peer already tolerates.
pub(crate) mod appearance {
    use super::*;
    pub(crate) use crate::protocol::drag_drop::DragImage;

    pub(crate) struct Identity {
        pub token: u64,
        pub drag: u64,
    }
    pub fn identify(_data: &super::Files, _identity: Identity) -> Result<()> {
        Ok(())
    }
    pub fn decorate(_data: &super::Files, _image: &DragImage) -> Result<()> {
        Ok(())
    }
    pub fn selection(paths: &[PathBuf]) -> Result<DragImage> {
        bail!("macOS 暂不支持生成拖放预览")
    }
}

/// The Wayland drag catcher; macOS has no compositor data-device to catch.
pub(crate) struct Catcher;
impl Catcher {
    pub fn start(
        _device: &str,
        _size: (u32, u32),
        _point: Position,
        _allowed: Arc<dyn Fn() -> bool + Send + Sync>,
        notify: Arc<dyn Fn(portal::Event) + Send + Sync>,
    ) -> Result<Self> {
        notify(portal::Event::Unavailable);
        bail!("macOS 暂不支持接取本机拖放")
    }
    pub fn cancel(&self) {}
}

// The shared file list the clipboard side parses; the parser itself is
// X11-specific and stays on the Linux side.
pub(crate) fn parse_uri_list(_text: &str) -> Vec<PathBuf> {
    Vec::new()
}

// Keeps the Mutex import honest; Session state lands here when one exists.
#[allow(unused)]
fn _unused(_m: Mutex<()>, _s: AtomicBool) {}
