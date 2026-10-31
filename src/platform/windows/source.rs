//! Where an encoder pipeline's frames come from (`docs/protocol/camera-design.md`
//! §9.1): the pipeline, the encoder probe and the encode groups see only this,
//! not the desktop capture behind it.
use super::capture::{Desktop, Frame, Source};
use super::cursor_shape::Snapshot;
use anyhow::Result;
use windows::Win32::Graphics::Direct3D11::ID3D11Device;

/// A source of video frames as D3D11 textures on one device.
pub(crate) trait FrameSource {
    /// The device its frames are on.
    fn device(&self) -> &ID3D11Device;
    /// Changes whenever the source was reopened (its device or format may
    /// differ): the pipeline rebuilds what depends on them.
    fn generation(&self) -> u64;
    /// It shows a picture now (a locked or lost desktop does not).
    fn available(&self) -> bool;
    /// It can deliver HDR (FP16) frames now.
    fn hdr_available(&self) -> bool;
    /// For diagnostics: how it captures.
    fn backend_name(&self) -> &'static str;
    /// What it is now; may change while open (a display mode change).
    fn descriptor(&self) -> Source;
    /// The pointer, for sources that have one.
    fn cursor(&self) -> Option<Snapshot> {
        None
    }
    /// The next frame scaled for `tier` within `maximum`: `None` when no new
    /// picture arrived within `timeout` ms.
    fn next(
        &mut self,
        timeout: u32,
        tier: crate::media::geometry::Tier,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>>;
}

/// Open `source` for an encoder pipeline.
pub(crate) fn open(source: &Source) -> Result<Box<dyn FrameSource>> {
    match source {
        Source::Screen(screen) => Ok(Box::new(Desktop::open_selected(screen)?)),
        Source::Camera(camera) => Ok(Box::new(super::camera::Capture::open(camera)?)),
    }
}

impl FrameSource for Desktop {
    fn device(&self) -> &ID3D11Device {
        &self.device
    }
    fn generation(&self) -> u64 {
        self.generation
    }
    fn available(&self) -> bool {
        self.available
    }
    fn hdr_available(&self) -> bool {
        Desktop::hdr_available(self)
    }
    fn backend_name(&self) -> &'static str {
        Desktop::backend_name(self)
    }
    fn descriptor(&self) -> Source {
        Source::Screen(self.screen.clone())
    }
    fn cursor(&self) -> Option<Snapshot> {
        self.cursor.clone()
    }
    fn next(
        &mut self,
        timeout: u32,
        tier: crate::media::geometry::Tier,
        cursor: bool,
        hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        Desktop::next(self, timeout, tier, cursor, hdr, maximum)
    }
}
