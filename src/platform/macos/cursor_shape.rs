//! Host cursor shape sampling. The controlled role is not supported on macOS,
//! so there is no pointer to read and sampling reports an invisible cursor.
use anyhow::{Result, bail};

/// The local pointer, as the host role would move it.
pub(crate) struct Pointer {
    pub handle: usize,
    pub showing: bool,
    pub x: i32,
    pub y: i32,
}

/// One cursor image, ARGB or PNG depending on the kind.
pub(crate) struct Shape {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub rgba: Vec<u8>,
    pub kind: i32,
}

/// The cursor image the protocol carries, PNG-encoded.
pub(crate) struct Image {
    pub width: u32,
    pub height: u32,
    pub hotspot: [u32; 2],
    pub kind: i32,
    pub png: String,
}

/// One point-in-time cursor state.
pub(crate) struct Snapshot {
    pub showing: bool,
    pub x: i32,
    pub y: i32,
    pub image: Option<Image>,
}

/// Sampler for the capture loop; nothing to sample without a host role.
pub(crate) struct Sampler;
impl Sampler {
    pub fn new() -> Self {
        Self
    }
    pub fn sample(&mut self) -> Result<Snapshot> {
        Ok(Snapshot {
            showing: false,
            x: 0,
            y: 0,
            image: None,
        })
    }
}
impl Default for Sampler {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) fn pointer() -> Result<Pointer> {
    bail!("macOS host role is not supported")
}

pub(crate) fn shape(_handle: usize) -> Result<Shape> {
    bail!("macOS host role is not supported")
}
