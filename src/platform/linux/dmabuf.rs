//! Zero-copy handover of VA-API surfaces to the renderer.
//!
//! A decoded VA surface is exported as a DRM PRIME dmabuf and imported into
//! wgpu as two single-plane textures, one per NV12 plane. That removes the
//! per-frame GPU-to-CPU readback and the CPU-to-GPU upload, which together
//! moved roughly `2 * width * height * 1.5` bytes per picture.
//!
//! The decoder owns the `VASurface` and cannot share it across threads, so the
//! frame carries a [`SurfaceLease`] instead: it names the pool slot and returns
//! it when the renderer is done with the picture. The slot is not reusable
//! until then, which is why the pool reserves headroom for exported frames.
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// One NV12 plane of an exported surface.
#[derive(Debug)]
pub(crate) struct Plane {
    /// An independent handle to the dmabuf, so Vulkan can take ownership.
    pub(crate) fd: OwnedFd,
    pub(crate) offset: u64,
    pub(crate) stride: u64,
}

impl Plane {
    /// Duplicates the handle: `texture_from_dmabuf_fd` consumes its fd.
    pub(crate) fn dup(&self) -> std::io::Result<OwnedFd> {
        // Safe: the raw fd is owned by `self.fd` and stays open across the call.
        let raw = unsafe { libc::dup(self.fd.as_raw_fd()) };
        if raw < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Safe: `dup` returned a fresh descriptor we now own.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }
}

/// An NV12 picture exported as one dmabuf with two planes.
#[derive(Debug)]
pub(crate) struct DmabufFrame {
    pub(crate) modifier: u64,
    pub(crate) planes: [Plane; 2],
    /// Returns the decoder's surface to its pool once the picture is dropped.
    lease: SurfaceLease,
}

impl DmabufFrame {
    pub(crate) fn new(modifier: u64, planes: [Plane; 2], lease: SurfaceLease) -> Self {
        Self {
            modifier,
            planes,
            lease,
        }
    }

    /// Keeps the decoder's surface reserved for as long as this returns.
    pub(crate) fn lease(&self) -> SurfaceLease {
        self.lease.clone()
    }
}

/// Keeps a decoded surface out of the decoder pool until the last clone drops.
#[derive(Clone, Debug)]
pub(crate) struct SurfaceLease {
    returned: Arc<Mutex<Vec<usize>>>,
    index: usize,
}

impl SurfaceLease {
    pub(crate) fn new(returned: Arc<Mutex<Vec<usize>>>, index: usize) -> Self {
        Self { returned, index }
    }
}

impl Drop for SurfaceLease {
    fn drop(&mut self) {
        // A poisoned lock only means some other thread panicked; the surface is
        // still ours to hand back, so ignore the poison.
        let mut returned = match self.returned.lock() {
            Ok(returned) => returned,
            Err(poisoned) => poisoned.into_inner(),
        };
        returned.push(self.index);
    }
}

/// The channel the decoder drains to learn which surfaces the renderer released.
pub(crate) type ReturnedSurfaces = Arc<Mutex<Vec<usize>>>;

/// Set once the renderer proves it cannot import the exported buffers.
///
/// The decoder checks it before exporting, so a backend that rejects the
/// dmabuf (GL, or a driver without the needed extensions) pays one dropped
/// picture and then returns to the CPU readback for good.
static UNSUPPORTED: AtomicBool = AtomicBool::new(false);

/// Records that importing an exported buffer failed.
pub(crate) fn mark_unsupported() {
    if !UNSUPPORTED.swap(true, Ordering::Relaxed) {
        tracing::warn!("dmabuf 导入不可用，回退到 CPU 回读路径");
    }
}

/// Whether the zero-copy path is enabled: opt in with `OPENUUYC_ZEROCOPY=1`.
///
/// Off by default until it is verified beyond the Intel (i965) machine it was
/// written on: NVIDIA's VA driver exports differently tiled buffers, and a
/// wrong import shows a corrupted picture rather than an error.
pub(crate) fn enabled() -> bool {
    if UNSUPPORTED.load(Ordering::Relaxed) {
        return false;
    }
    matches!(
        std::env::var("OPENUUYC_ZEROCOPY").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}
