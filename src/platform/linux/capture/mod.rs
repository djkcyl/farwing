//! Screen capture for the host role, with backends tried in order of
//! preference the way Sunshine picks its capture method.
//!
//! - NVIDIA NvFBC into CUDA (`nvfbc`): Xorg on NVIDIA, only while NVENC works,
//!   so frames go from the display to the encoder without leaving the GPU.
//! - X11 MIT-SHM (`x11`): Xorg sessions; no setup, no consent prompt.
//! - wlroots screencopy (`wlr`): Wayland sessions on wlroots, niri, Sway and
//!   their relatives; frames come through the compositor itself, and nobody is
//!   asked for consent.
//! - XDG ScreenCast portal + PipeWire (`portal`): Wayland sessions, and any
//!   desktop whose portal offers it; asks the local user once.
//!
//! Sunshine puts KMS first; on this driver stack it finds no scanout buffer
//! under Xorg (the NVIDIA X driver bypasses KMS), so it is not in the list
//! yet. `OPENUUYC_CAPTURE` (for example `portal` or `nvfbc,x11`) overrides the
//! order.
//!
//! Frames are BGRA, in system memory or, from NvFBC, in CUDA device memory.
mod nvfbc;
mod pipewire;
mod portal;
mod wlr_screencopy;
mod x11;

use anyhow::{Result, bail, ensure};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub(crate) use crate::media::capture::{Screen, SourceGone};

/// An X11 capture has no GPU device. The one value stands for the CPU path
/// that capture and the software encoder share, so the loop's "same device,
/// no transfer" test always holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Device;

/// Nothing on the CPU path can be removed from under the capture.
pub(crate) fn device_lost(_device: &Device) -> bool {
    false
}

/// Linux captures on the CPU, so there is no adapter for a second device to be
/// opened on. The host only asks for one when a hardware encoder sits on a
/// different adapter than the capture, which cannot happen here.
pub(crate) fn create_device(_adapter: u64) -> Result<(Device, ())> {
    Ok((Device, ()))
}

/// GPU adapters a hardware encoder could be pinned to. None are listed: NVENC
/// runs on the GPU driving the display and VA-API on the driver's device, so
/// there is nothing to choose between and the preference stays automatic.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct EncodingAdapter {
    #[serde(default)]
    pub id: Option<crate::media::selection::GpuId>,
    pub luid: u64,
    pub vendor: u32,
    pub name: String,
}

pub(crate) fn encoding_adapters() -> Result<Vec<EncodingAdapter>> {
    Ok(Vec::new())
}

/// Whether the desktop session is locked. X11 has no portable answer: a
/// screen locker is just another client drawing over the root window, and the
/// capture sees exactly what it draws.
pub(crate) fn session_locked() -> Option<bool> {
    None
}

/// A captured picture's BGRA pixels, wherever the capture left them.
#[derive(Clone)]
pub(crate) enum Image {
    /// Tightly packed, `width * height * 4` bytes.
    Cpu {
        pixels: Arc<Vec<u8>>,
        width: u32,
        height: u32,
    },
    /// A pitched CUDA allocation on the GPU driving the display.
    Cuda(Arc<super::cuda::Buffer>),
    Nv12(Arc<super::gpu_convert::OwnedNv12>),
}
impl Image {
    pub fn size(&self) -> (u32, u32) {
        match self {
            Self::Cpu { width, height, .. } => (*width, *height),
            Self::Cuda(buffer) => (buffer.width, buffer.height),
            Self::Nv12(frame) => frame.size(),
        }
    }
    /// The pixels in system memory, read back from the GPU if need be.
    pub fn pixels(&self) -> Result<std::borrow::Cow<'_, [u8]>> {
        Ok(match self {
            Self::Cpu { pixels, .. } => std::borrow::Cow::Borrowed(pixels.as_slice()),
            Self::Cuda(buffer) => std::borrow::Cow::Owned(buffer.download()?),
            Self::Nv12(frame) => std::borrow::Cow::Owned(frame.pixels()?),
        })
    }
}

/// A backend's picture before it is scaled: tightly packed BGRA rows, or NV12
/// converted on the GPU.
pub(super) struct Picture {
    /// The size of the captured region: the BGRA `pixels` with no `nv12`, the
    /// cropped source with one. The GPU converter may already have scaled
    /// that frame, so its geometry is `nv12.size()`, not this.
    pub width: u32,
    pub height: u32,
    /// BGRA rows of `width * 4` bytes; empty when `nv12` carries the frame.
    pub pixels: Vec<u8>,
    /// The frame converted on the GPU (portal stream only), at the output
    /// geometry the worker had requested through the stream's `set_target`
    /// when the picture arrived (the crop size before any request). A quality
    /// change can make it stale.
    pub nv12: Option<Arc<super::gpu_convert::OwnedNv12>>,
}

/// One captured picture, already scaled to the size the encoder was asked for.
#[derive(Clone)]
pub(crate) struct Frame {
    pub width: u32,
    pub height: u32,
    pub image: Image,
    pub captured: Instant,
    pub is_new: bool,
    pub hdr_metadata: Option<crate::media::video_color::HdrMetadata>,
}

/// The active RandR outputs, in the protocol's terms. The identity is the
/// one the display topology uses, so screens and targets pair up exactly.
pub(crate) fn screens() -> Result<Vec<Screen>> {
    // Monitors come from RandR, which XWayland also provides on Wayland.
    ensure!(
        std::env::var_os("DISPLAY").is_some(),
        "没有 X11 显示（DISPLAY 未设置），无法枚举显示器"
    );
    let monitors = super::display::topology::Topology::query(true)?.monitors();
    ensure!(!monitors.is_empty(), "没有检测到显示器");
    monitors
        .into_iter()
        .map(|monitor| {
            Ok(Screen {
                id: super::display::source_id(&monitor.identity)?,
                device_name: monitor.name.clone(),
                display_name: monitor.name,
                width: monitor.width,
                height: monitor.height,
                left: monitor.left,
                top: monitor.top,
                primary: monitor.primary,
                fps: monitor.hz.filter(|hz| *hz > 1).unwrap_or(60),
                // X11 has no per-output scale to report.
                dpi_scale: None,
                hdr: false,
                adapter: 0,
                render_adapter: None,
                identity: Some(monitor.identity),
            })
        })
        .collect()
}

/// Whether this is a Wayland session, where X11 sees only XWayland clients.
pub(crate) fn wayland_session() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some()
        || std::env::var("XDG_SESSION_TYPE").is_ok_and(|kind| kind == "wayland")
}

/// The same physical output as `selected`, as it is now.
pub(crate) fn refresh(selected: &Screen) -> Result<Screen> {
    let mut found = screens()?
        .into_iter()
        .find(|screen| screen.identity == selected.identity)
        .ok_or(SourceGone)?;
    found.id = selected.id;
    Ok(found)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    NvFbc,
    X11,
    Wlr,
    Portal,
}
impl Kind {
    fn label(self) -> &'static str {
        match self {
            Self::NvFbc => "NvFBC",
            Self::X11 => "X11",
            Self::Wlr => "wlr-screencopy",
            Self::Portal => "XDG 门户",
        }
    }
}

/// The capture methods to try, best first.
fn candidates() -> Result<Vec<Kind>> {
    if let Ok(order) = std::env::var("OPENUUYC_CAPTURE") {
        let kinds = order
            .split(',')
            .map(|name| match name.trim() {
                "nvfbc" => Ok(Kind::NvFbc),
                "x11" => Ok(Kind::X11),
                "wlr" => Ok(Kind::Wlr),
                "portal" => Ok(Kind::Portal),
                other => bail!("OPENUUYC_CAPTURE 中有未知的采集方式：{other}"),
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(!kinds.is_empty(), "OPENUUYC_CAPTURE 为空");
        return Ok(kinds);
    }
    Ok(if wayland_session() {
        // The compositor's own protocol first: it is the only one that copies
        // without asking, and the portal stays as the fallback for compositors
        // that do not serve it.
        vec![Kind::Wlr, Kind::Portal]
    } else {
        vec![Kind::NvFbc, Kind::X11, Kind::Portal]
    })
}

/// The candidates that can really open here, in order. A `Wlr` entry is
/// dropped when the compositor does not serve the protocol, so whatever is
/// behind it — on Mutter and KWin, the portal — is seen for what it is.
fn usable(kinds: &[Kind], wlr_available: bool) -> Vec<Kind> {
    kinds
        .iter()
        .copied()
        .filter(|kind| *kind != Kind::Wlr || wlr_available)
        .collect()
}

/// Whether opening capture now would ask the local user to share the
/// screen: the portal comes first and has no remembered grant yet. A `Wlr`
/// that cannot open leaves the portal first, so the question has to be asked
/// of the list the session would really use.
pub(crate) fn needs_consent() -> bool {
    candidates().is_ok_and(|kinds| {
        // Only a `Wlr` head can hide the portal behind it, and the probe costs
        // a connection, so it is skipped for every other order.
        let wlr = kinds.first() == Some(&Kind::Wlr) && wlr_screencopy::available();
        consent_needed(&usable(&kinds, wlr), portal::remembered())
    })
}

fn consent_needed(kinds: &[Kind], remembered: bool) -> bool {
    kinds.first() == Some(&Kind::Portal) && !remembered
}

enum Backend {
    NvFbc(nvfbc::Grab),
    X11(x11::Grab),
    Wlr(wlr_screencopy::Grab),
    Portal(portal::Grab),
}
impl Backend {
    /// `size` is the frame size for backends that scale while capturing.
    fn open(kind: Kind, screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        Ok(match kind {
            Kind::NvFbc => Self::NvFbc(nvfbc::Grab::open(screen, cursor, size)?),
            Kind::X11 => Self::X11(x11::Grab::open(screen)?),
            Kind::Wlr => Self::Wlr(wlr_screencopy::Grab::open(screen, cursor)?),
            Kind::Portal => Self::Portal(portal::Grab::open(screen, cursor)?),
        })
    }
    fn kind(&self) -> Kind {
        match self {
            Self::NvFbc(_) => Kind::NvFbc,
            Self::X11(_) => Kind::X11,
            Self::Wlr(_) => Kind::Wlr,
            Self::Portal(_) => Kind::Portal,
        }
    }
    /// Open the first capture method that works for `screen`.
    fn select(screen: &Screen, cursor: bool, size: (u32, u32)) -> Result<Self> {
        let mut failures = Vec::new();
        for kind in candidates()? {
            match Self::open(kind, screen, cursor, size) {
                Ok(backend) => {
                    tracing::info!(
                        backend = backend.name(),
                        screen = screen.id,
                        "host capture backend selected"
                    );
                    return Ok(backend);
                }
                Err(error) => {
                    tracing::info!(backend = kind.label(), error = %format!("{error:#}"), "host capture backend unavailable");
                    failures.push(format!("{}：{error:#}", kind.label()));
                }
            }
        }
        bail!("没有可用的屏幕采集方式（{}）", failures.join("；"))
    }
    fn name(&self) -> &'static str {
        match self {
            Self::NvFbc(grab) => grab.name(),
            Self::X11(grab) => grab.name(),
            Self::Wlr(grab) => grab.name(),
            Self::Portal(grab) => grab.name(),
        }
    }
}

/// A capture of one monitor, kept open between frames.
pub(crate) struct Desktop {
    pub device: Device,
    pub screen: Screen,
    pub generation: u64,
    pub available: bool,
    /// The pointer as of the latest `next`, for the cursor channel.
    pub cursor: Option<super::cursor_shape::Snapshot>,
    sampler: super::cursor_shape::Sampler,
    backend: Backend,
    /// The last system-memory picture, which X11 compares against.
    last: Option<Arc<Vec<u8>>>,
    last_size: (u32, u32),
    refreshed: Instant,
    /// Keeps the display on and the machine awake while captured. Declared
    /// last so it outlives the capture resources, as on Windows.
    _power: Option<super::power_request::Request>,
}

impl Desktop {
    pub fn open_selected(selected: &Screen) -> Result<Self> {
        // Unlike Windows, a desktop without the idle-inhibit interface does
        // not stop the capture; the session works, the screen may blank.
        let power = super::power_request::Request::capture()
            .inspect_err(|error| {
                tracing::warn!(error = %format!("{error:#}"),
                    "capture automatic display-off/sleep prevention unavailable")
            })
            .ok();
        let screen = refresh(selected)?;
        let size = (screen.width & !1, screen.height & !1);
        let backend = Backend::select(&screen, false, size)?;
        Ok(Self {
            device: Device,
            screen,
            generation: 0,
            available: true,
            cursor: None,
            sampler: Default::default(),
            backend,
            last: None,
            last_size: (0, 0),
            refreshed: Instant::now(),
            _power: power,
        })
    }

    pub fn backend_name(&self) -> &'static str {
        self.backend.name()
    }

    /// Neither backend captures HDR.
    pub fn hdr_available(&self) -> bool {
        false
    }

    /// The next picture, or `None` when nothing changed before `timeout` ms.
    pub fn next(
        &mut self,
        timeout: u32,
        quality: i32,
        cursor: bool,
        _hdr: bool,
        maximum: (u32, u32),
    ) -> Result<Option<Frame>> {
        if self.refreshed.elapsed() >= Duration::from_secs(1) {
            let current = refresh(&self.screen)?;
            self.refreshed = Instant::now();
            self.screen.display_name.clone_from(&current.display_name);
            if current != self.screen {
                let size = output(current.width, current.height, quality, maximum);
                let backend = Backend::open(self.backend.kind(), &current, cursor, size)
                    .or_else(|_| Backend::select(&current, cursor, size))?;
                self.backend = backend;
                self.screen = current;
                self.generation = self.generation.wrapping_add(1);
                self.last = None;
            }
        }
        // The portal and wlr-screencopy draw the cursor or leave it out for the
        // whole session; a change of request moves to a session with the other
        // mode.
        if let Backend::Portal(grab) = &self.backend
            && grab.cursor() != cursor
        {
            self.backend = Backend::Portal(portal::Grab::open(&self.screen, cursor)?);
            self.last = None;
        }
        if let Backend::Wlr(grab) = &self.backend
            && grab.cursor() != cursor
        {
            self.backend = Backend::Wlr(wlr_screencopy::Grab::open(&self.screen, cursor)?);
            self.last = None;
        }
        // NvFBC scales and draws the cursor as it captures; both are fixed
        // per session, so a different request opens a new one.
        let wanted = output(self.screen.width, self.screen.height, quality, maximum);
        if let Backend::NvFbc(grab) = &self.backend
            && !grab.matches(cursor, wanted)
        {
            self.backend = Backend::NvFbc(nvfbc::Grab::open(&self.screen, cursor, wanted)?);
        }
        // The portal converter derives its output from each picture's own size.
        // Hand it the current request so a controller quality that differs from
        // the source is scaled on the GPU instead of read back to the CPU.
        if let Backend::Portal(grab) = &self.backend {
            grab.set_target(Some((quality, maximum)));
        }
        self.cursor = match self.sampler.sample() {
            Ok(pointer) => Some(pointer),
            Err(error) => {
                tracing::debug!(%error, "host cursor sampling failed");
                None
            }
        };
        let deadline = Instant::now() + Duration::from_millis(u64::from(timeout));
        let result = match &mut self.backend {
            // The driver waits for a new frame and delivers it on the GPU.
            Backend::NvFbc(grab) => {
                return match grab.next(timeout) {
                    Ok(frame) => {
                        self.available = true;
                        Ok(frame.map(|buffer| Frame {
                            width: buffer.width,
                            height: buffer.height,
                            image: Image::Cuda(buffer),
                            captured: Instant::now(),
                            is_new: true,
                            hdr_metadata: None,
                        }))
                    }
                    Err(error) => {
                        self.available = false;
                        Err(error)
                    }
                };
            }
            Backend::X11(grab) => loop {
                if let Err(error) = grab.grab(&self.screen, cursor) {
                    break Err(error);
                }
                let size = output(self.screen.width, self.screen.height, quality, maximum);
                let image = scale(grab.pixels(), (self.screen.width, self.screen.height), size);
                // X11 cannot say whether anything changed; compare.
                if self.last_size != size
                    || self
                        .last
                        .as_deref()
                        .is_none_or(|last| last.as_slice() != image.as_slice())
                {
                    break Ok(Some((size, image)));
                }
                let now = Instant::now();
                if now >= deadline {
                    break Ok(None);
                }
                std::thread::sleep((deadline - now).min(Duration::from_millis(8)));
            },
            // The compositor answers a copy request only when it next draws,
            // so a still desktop reports no picture.
            Backend::Wlr(grab) => grab
                .next(deadline.saturating_duration_since(Instant::now()))
                .map(|picture| {
                    picture.map(|picture| {
                        let size = output(picture.width, picture.height, quality, maximum);
                        let image = if size == (picture.width, picture.height) {
                            picture.pixels
                        } else {
                            scale(&picture.pixels, (picture.width, picture.height), size)
                        };
                        (size, image)
                    })
                }),
            // PipeWire only delivers a picture when the monitor changed.
            Backend::Portal(grab) => {
                match grab.next(deadline.saturating_duration_since(Instant::now())) {
                    Ok(Some(picture)) => {
                        let size = output(picture.width, picture.height, quality, maximum);
                        if let Some(frame) = picture.nv12 {
                            // The converter already produced `size`; a mismatch
                            // would mean a stale target, so keep the GPU frame
                            // only when it matches and fall back otherwise.
                            if size == frame.size() {
                                self.available = true;
                                self.last_size = size;
                                return Ok(Some(Frame {
                                    width: size.0,
                                    height: size.1,
                                    image: Image::Nv12(frame),
                                    captured: Instant::now(),
                                    is_new: true,
                                    hdr_metadata: None,
                                }));
                            }
                            // The converter produced a different geometry than
                            // the current request (a stale target during a
                            // quality change). Scale from the frame's own size,
                            // not the source crop, and let the next frame use
                            // the refreshed target.
                            let pixels = frame.pixels()?;
                            let from = frame.size();
                            Ok(Some((
                                size,
                                if from == size {
                                    pixels
                                } else {
                                    scale(&pixels, from, size)
                                },
                            )))
                        } else {
                            let image = if size == (picture.width, picture.height) {
                                picture.pixels
                            } else {
                                scale(&picture.pixels, (picture.width, picture.height), size)
                            };
                            Ok(Some((size, image)))
                        }
                    }
                    Ok(None) => Ok(None),
                    Err(e) => Err(e),
                }
            }
        };
        match result {
            Ok(Some((size, image))) => {
                self.available = true;
                let image = Arc::new(image);
                self.last = Some(image.clone());
                self.last_size = size;
                Ok(Some(Frame {
                    width: size.0,
                    height: size.1,
                    image: Image::Cpu {
                        pixels: image,
                        width: size.0,
                        height: size.1,
                    },
                    captured: Instant::now(),
                    is_new: true,
                    hdr_metadata: None,
                }))
            }
            Ok(None) => {
                self.available = true;
                Ok(None)
            }
            Err(error) => {
                self.available = false;
                Err(error)
            }
        }
    }
}

/// The encoded size for a source of `width`x`height` at `quality`.
fn output(width: u32, height: u32, quality: i32, maximum: (u32, u32)) -> (u32, u32) {
    let size = crate::media::geometry::output_size(width, height, quality);
    crate::media::geometry::fit_size(size.0, size.1, maximum)
}

/// Bilinear BGRA scaling, or a crop to even dimensions when the size already
/// fits. The encoder takes exactly the size it was opened with. The output is
/// opaque whatever the source's fourth byte held (X leaves it undefined).
///
/// A capture larger than the stream (a 4K-class screen at the highest
/// quality, which caps at 3840x2160) is scaled on every frame, so the work is
/// split by rows across the cores; each output row depends only on the source.
fn scale(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
    let (source_width, source_height) = (from.0 as usize, from.1 as usize);
    let (width, height) = (to.0 as usize, to.1 as usize);
    let mut out = vec![0u8; width * height * 4];
    if width <= source_width
        && height <= source_height
        && (source_width - width) <= 1
        && (source_height - height) <= 1
    {
        for y in 0..height {
            let start = y * source_width * 4;
            let line = &mut out[y * width * 4..(y + 1) * width * 4];
            line.copy_from_slice(&source[start..start + width * 4]);
            for pixel in line.chunks_exact_mut(4) {
                pixel[3] = 0xff;
            }
        }
        return out;
    }
    let columns = taps(source_width, width);
    let rows = taps(source_height, height);
    let row_bytes = width * 4;
    // Below about a megapixel the threads cost more than they save.
    let threads = if width * height < 1 << 20 {
        1
    } else {
        std::thread::available_parallelism().map_or(1, |n| n.get().min(8))
    };
    let band = height.div_ceil(threads);
    std::thread::scope(|scope| {
        for (index, chunk) in out.chunks_mut(band * row_bytes).enumerate() {
            let (columns, rows) = (&columns, &rows);
            let mut work = move || {
                for (offset, line) in chunk.chunks_exact_mut(row_bytes).enumerate() {
                    scale_row(
                        source,
                        source_width,
                        columns,
                        rows[index * band + offset],
                        line,
                    );
                }
            };
            if index + 1 == threads.min(height.div_ceil(band)) {
                work();
            } else {
                scope.spawn(work);
            }
        }
    });
    out
}

/// For each output position along one axis: the two source positions it
/// blends and the weight of the second, in 16.16 fixed point, sampling pixel
/// centres.
fn taps(source: usize, output: usize) -> Vec<(usize, usize, u32)> {
    let step = ((source as u64) << 16) / output as u64;
    let last = source - 1;
    (0..output)
        .map(|position| {
            let at = ((position as u64 * step) + (step >> 1)).saturating_sub(1 << 15);
            let first = ((at >> 16) as usize).min(last);
            (first, (first + 1).min(last), (at & 0xffff) as u32)
        })
        .collect()
}

fn scale_row(
    source: &[u8],
    source_width: usize,
    columns: &[(usize, usize, u32)],
    (y0, y1, fy): (usize, usize, u32),
    line: &mut [u8],
) {
    let top_row = &source[y0 * source_width * 4..(y0 + 1) * source_width * 4];
    let bottom_row = &source[y1 * source_width * 4..(y1 + 1) * source_width * 4];
    let (wy0, wy1) = (u64::from(0x10000 - fy), u64::from(fy));
    for (pixel, &(x0, x1, fx)) in line.chunks_exact_mut(4).zip(columns) {
        let (a, b) = (&top_row[x0 * 4..x0 * 4 + 4], &top_row[x1 * 4..x1 * 4 + 4]);
        let (c, d) = (
            &bottom_row[x0 * 4..x0 * 4 + 4],
            &bottom_row[x1 * 4..x1 * 4 + 4],
        );
        let (wx0, wx1) = (0x10000 - fx, fx);
        for channel in 0..3 {
            let top = u32::from(a[channel]) * wx0 + u32::from(b[channel]) * wx1;
            let bottom = u32::from(c[channel]) * wx0 + u32::from(d[channel]) * wx1;
            let value = ((u64::from(top >> 8) * wy0 + u64::from(bottom >> 8) * wy1) >> 24) as u32;
            pixel[channel] = value.min(255) as u8;
        }
        pixel[3] = 0xff;
    }
}

#[cfg(test)]
mod tests {
    use super::{Kind, consent_needed, scale, usable};

    #[test]
    fn only_an_unremembered_portal_asks_the_user() {
        assert!(consent_needed(&[Kind::Portal], false));
        assert!(!consent_needed(&[Kind::Portal], true));
        // X11 sessions reach the portal only after NvFBC and MIT-SHM fail.
        assert!(!consent_needed(
            &[Kind::NvFbc, Kind::X11, Kind::Portal],
            false
        ));
    }

    #[test]
    fn a_wlr_that_cannot_open_leaves_the_portal_in_front() {
        // Neither Mutter nor KWin serves `zwlr_screencopy_manager_v1`: the
        // startup probe must not fall through to the portal and pop its
        // dialog, which is what #1's guard exists to prevent.
        assert!(consent_needed(
            &usable(&[Kind::Wlr, Kind::Portal], false),
            false
        ));
        assert!(!consent_needed(
            &usable(&[Kind::Wlr, Kind::Portal], true),
            false
        ));
        // A grant that is already remembered stays quiet either way.
        assert!(!consent_needed(
            &usable(&[Kind::Wlr, Kind::Portal], false),
            true
        ));
        // An X11 order has no wlr entry to drop.
        assert!(!consent_needed(
            &usable(&[Kind::NvFbc, Kind::X11, Kind::Portal], false),
            false
        ));
    }

    #[test]
    fn crop_to_even_keeps_pixels() {
        let source: Vec<u8> = (0..3 * 3 * 4).map(|v| v as u8).collect();
        let out = scale(&source, (3, 3), (2, 2));
        let opaque = |pixels: &[u8]| -> Vec<u8> {
            pixels
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2], 0xff])
                .collect()
        };
        assert_eq!(&out[..8], &opaque(&source[..8])[..]);
        assert_eq!(&out[8..16], &opaque(&source[12..20])[..]);
    }

    /// The single-threaded scaler `scale` replaced; its output is the contract.
    fn reference(source: &[u8], from: (u32, u32), to: (u32, u32)) -> Vec<u8> {
        let (source_width, source_height) = (from.0 as usize, from.1 as usize);
        let (width, height) = (to.0 as usize, to.1 as usize);
        let mut out = vec![0u8; width * height * 4];
        if width <= source_width
            && height <= source_height
            && (source_width - width) <= 1
            && (source_height - height) <= 1
        {
            for y in 0..height {
                let start = y * source_width * 4;
                out[y * width * 4..(y + 1) * width * 4]
                    .copy_from_slice(&source[start..start + width * 4]);
            }
            return out;
        }
        // 16.16 fixed point, sampling pixel centres.
        let step_x = ((source_width as u64) << 16) / width as u64;
        let step_y = ((source_height as u64) << 16) / height as u64;
        let max_x = source_width - 1;
        let max_y = source_height - 1;
        for y in 0..height {
            let sy = ((y as u64 * step_y) + (step_y >> 1)).saturating_sub(1 << 15);
            let y0 = ((sy >> 16) as usize).min(max_y);
            let y1 = (y0 + 1).min(max_y);
            let fy = (sy & 0xffff) as u32;
            for x in 0..width {
                let sx = ((x as u64 * step_x) + (step_x >> 1)).saturating_sub(1 << 15);
                let x0 = ((sx >> 16) as usize).min(max_x);
                let x1 = (x0 + 1).min(max_x);
                let fx = (sx & 0xffff) as u32;
                let at = |px: usize, py: usize| (py * source_width + px) * 4;
                let (a, b, c, d) = (at(x0, y0), at(x1, y0), at(x0, y1), at(x1, y1));
                let o = (y * width + x) * 4;
                for channel in 0..4 {
                    let top = u32::from(source[a + channel]) * (0x10000 - fx)
                        + u32::from(source[b + channel]) * fx;
                    let bottom = u32::from(source[c + channel]) * (0x10000 - fx)
                        + u32::from(source[d + channel]) * fx;
                    let value = ((u64::from(top >> 8) * u64::from(0x10000 - fy)
                        + u64::from(bottom >> 8) * u64::from(fy))
                        >> 24) as u32;
                    out[o + channel] = value.min(255) as u8;
                }
            }
        }
        out
    }

    #[test]
    fn the_threaded_scaler_matches_the_reference_byte_for_byte() {
        // A cheap deterministic noise, so every channel and weight is exercised.
        let mut state = 0x2545_f491_u32;
        let mut noise = |length: usize| -> Vec<u8> {
            (0..length)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 17;
                    state ^= state << 5;
                    state as u8
                })
                .collect()
        };
        for (from, to) in [
            ((4096, 2560), (3456, 2160)),
            ((1920, 1080), (1280, 720)),
            ((1366, 768), (1280, 720)),
            ((5120, 1440), (3840, 1080)),
            ((640, 480), (1280, 960)),
            ((7, 5), (4, 2)),
            ((3, 3), (2, 2)),
        ] {
            let source = noise((from.0 * from.1 * 4) as usize);
            let (ours, theirs) = (scale(&source, from, to), reference(&source, from, to));
            // Colour exactly as before; alpha is now always opaque.
            assert!(
                ours.chunks_exact(4)
                    .zip(theirs.chunks_exact(4))
                    .all(|(a, b)| a[..3] == b[..3] && a[3] == 0xff),
                "{from:?} -> {to:?}"
            );
        }
    }

    #[test]
    fn downscale_of_flat_colour_is_flat() {
        let source = [10u8, 20, 30, 255].repeat(64 * 48);
        let out = scale(&source, (64, 48), (32, 24));
        assert!(out.chunks_exact(4).all(|p| p == [10, 20, 30, 255]));
    }
}

#[cfg(test)]
mod session_tests {
    /// Needs a desktop session: `OPENUUYC_CAPTURE=portal cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn captures_frames_for_a_few_seconds() {
        let screens = super::screens().unwrap();
        let mut desktop = super::Desktop::open_selected(&screens[0]).unwrap();
        println!("backend: {}", desktop.backend_name());
        let started = std::time::Instant::now();
        let (mut frames, mut size) = (0, None);
        while started.elapsed() < std::time::Duration::from_secs(4) {
            if let Some(frame) = desktop.next(50, 4, false, false, (3840, 2160)).unwrap() {
                frames += 1;
                size = Some((frame.width, frame.height));
            }
        }
        println!("frames: {frames} size: {size:?}");
        assert!(frames > 0);
    }
}

#[cfg(test)]
mod stage_tests {
    /// Needs an X session: what each step of the X11 path costs per frame at
    /// the highest quality (a screen above 3840x2160 is scaled).
    #[test]
    #[ignore]
    fn x11_capture_stages() {
        let screens = super::screens().unwrap();
        let screen = &screens[0];
        let mut grab = super::x11::Grab::open(screen).unwrap();
        let size = super::output(screen.width, screen.height, 4, (3840, 2160));
        let n = 20u32;
        let (mut tg, mut ts, mut tc) = (
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
            std::time::Duration::ZERO,
        );
        let mut last: Option<Vec<u8>> = None;
        for _ in 0..n {
            let t = std::time::Instant::now();
            grab.grab(screen, true).unwrap();
            tg += t.elapsed();
            let t = std::time::Instant::now();
            let image = super::scale(grab.pixels(), (screen.width, screen.height), size);
            ts += t.elapsed();
            let t = std::time::Instant::now();
            let _same = last.as_deref() == Some(image.as_slice());
            tc += t.elapsed();
            last = Some(image);
        }
        println!(
            "{}x{} -> {}x{}: grab {:.1} ms, scale {:.1} ms, compare {:.1} ms",
            screen.width,
            screen.height,
            size.0,
            size.1,
            tg.as_secs_f64() * 1000.0 / f64::from(n),
            ts.as_secs_f64() * 1000.0 / f64::from(n),
            tc.as_secs_f64() * 1000.0 / f64::from(n)
        );
    }
}
