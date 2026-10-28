//! Exhaustive current-backend checks. No automatic candidate or adapter fallback.
use super::software_slot::SoftwareSlot;
use crate::media::decode_api::{DecoderMode, DecoderNotification, VideoDecoderConfig};
use crate::media::{LocalDisplayInfo, VideoCodec};
use crate::platform::{
    decoder::{WindowsDecodedFrame, WindowsVideoDecoder},
    surface::D3D11SurfaceWriter,
};
use anyhow::{Context, Result, ensure};
use bytes::Bytes;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
    mpsc::Sender,
};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
pub(crate) struct Format {
    codec: VideoCodec,
    chroma: u8,
    depth: u8,
}
impl Format {
    fn label(self) -> String {
        (crate::i18n::t().session.probe_format)(
            if self.codec == VideoCodec::Av1 {
                "AV1"
            } else if self.codec == VideoCodec::H264 {
                "H.264"
            } else {
                "H.265"
            },
            if self.chroma == 1 { "420" } else { "444" },
            self.depth
        )
    }
    fn sample(self, size: (u32, u32)) -> Option<&'static [u8]> {
        macro_rules! sample {
            ($name:literal, $extension:literal) => {
                match size {
                    (1280, 720) => Some(
                        include_bytes!(concat!("fixtures/matrix/", $name, "_1280x720", $extension))
                            .as_slice(),
                    ),
                    (1920, 1080) => Some(
                        include_bytes!(concat!(
                            "fixtures/matrix/",
                            $name,
                            "_1920x1080",
                            $extension
                        ))
                        .as_slice(),
                    ),
                    (2560, 1440) => Some(
                        include_bytes!(concat!(
                            "fixtures/matrix/",
                            $name,
                            "_2560x1440",
                            $extension
                        ))
                        .as_slice(),
                    ),
                    (3840, 2160) => Some(
                        include_bytes!(concat!(
                            "fixtures/matrix/",
                            $name,
                            "_3840x2160",
                            $extension
                        ))
                        .as_slice(),
                    ),
                    _ => None,
                }
            };
        }
        match (self.codec, self.chroma, self.depth) {
            (VideoCodec::Av1, 1, 8) => sample!("av1_8", ".obu"),
            (VideoCodec::Av1, 1, 10) => sample!("av1_10", ".obu"),
            (VideoCodec::Av1, 3, 8) => sample!("av1_444_8", ".obu"),
            (VideoCodec::Av1, 3, 10) => sample!("av1_444_10", ".obu"),

            (VideoCodec::H264, 1, 8) => sample!("h264", ".annexb"),
            (VideoCodec::H264, 3, 8) => sample!("h264_444", ".annexb"),
            (VideoCodec::H265, 1, 8) => sample!("hevc", ".annexb"),
            (VideoCodec::H265, 1, 10) => sample!("main10", ".annexb"),
            (VideoCodec::H265, 3, 8) => sample!("hevc_444", ".annexb"),
            (VideoCodec::H265, 3, 10) => sample!("hevc_444_10", ".annexb"),
            _ => None,
        }
    }
}
fn formats() -> Vec<Format> {
    [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]
        .into_iter()
        .flat_map(|codec| {
            [1, 3].into_iter().flat_map(move |chroma| {
                [8, 10].map(|depth| Format {
                    codec,
                    chroma,
                    depth,
                })
            })
        })
        .collect()
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Pending,
    Passed,
    Failed,
    Unsupported,
    Busy,
    Unchecked,
}
impl Status {
    pub fn label(self) -> &'static str {
        match self {
            Self::Pending => crate::i18n::t().session.probe_pending,
            Self::Passed => crate::i18n::t().session.probe_passed,
            Self::Failed => crate::i18n::t().session.probe_failed,
            Self::Unsupported => crate::i18n::t().session.probe_unsupported,
            Self::Busy => crate::i18n::t().session.probe_busy,
            Self::Unchecked => crate::i18n::t().session.probe_unchecked,
        }
    }
}
#[derive(Clone)]
pub(crate) struct Cell {
    pub status: Status,
    pub detail: String,
}
pub(crate) struct Row {
    pub format: String,
    pub cells: Vec<Cell>,
}
pub(crate) struct Backend {
    pub name: String,
    pub device: Option<String>,
    pub sizes: Vec<(u32, u32)>,
    pub rows: Vec<Row>,
}
#[derive(Default)]
pub(crate) struct Report {
    pub backends: Vec<Backend>,
    pub message: String,
}
pub(crate) enum Event {
    Backend(Backend),
    Cell(usize, usize, usize, Cell),
    Finished(String),
}

pub(crate) fn run(
    display: LocalDisplayInfo,
    tx: &Sender<Event>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    if cancel.load(Ordering::Acquire) {
        return Ok(());
    }
    let devices = match D3D11SurfaceWriter::diagnostic_adapters() {
        Ok(devices) if !devices.is_empty() => devices,
        Ok(_) => vec![(
            "DXVA11".into(),
            Err(anyhow::anyhow!("{}", crate::i18n::t().session.probe_no_adapter)),
        )],
        Err(error) => vec![(crate::i18n::t().session.probe_dxva_enumeration.into(), Err(error))],
    };
    let mut all: Vec<_> = devices
        .into_iter()
        .map(|(name, writer)| (format!("DXVA11 · {name}"), Some(name), Some(writer)))
        .collect();
    all.push((crate::i18n::t().session.probe_software_backend.into(), None, None));
    let mut sizes: Vec<_> = std::iter::once((1280, 720))
        .chain(super::PROBE_SIZES.into_iter().rev())
        .collect();
    let local = (display.width, display.height);
    if local.0 > 0 && local.1 > 0 && !sizes.contains(&local) {
        sizes.push(local);
    }
    let matrix = formats();
    for (name, device, _) in &all {
        if tx
            .send(Event::Backend(Backend {
                name: name.clone(),
                device: device.clone(),
                sizes: sizes.clone(),
                rows: matrix
                    .iter()
                    .map(|f| Row {
                        format: f.label(),
                        cells: sizes
                            .iter()
                            .map(|_| Cell {
                                status: Status::Pending,
                                detail: String::new(),
                            })
                            .collect(),
                    })
                    .collect(),
            }))
            .is_err()
        {
            return Ok(());
        }
    }
    for (backend, (name, _, device)) in all.iter().enumerate() {
        for (row, format) in matrix.iter().copied().enumerate() {
            for (column, &size) in sizes.iter().enumerate() {
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                let supported = if device.is_some() {
                    matches!(format.codec, VideoCodec::H265 | VideoCodec::Av1)
                        || (format.chroma == 1 && format.depth == 8)
                } else {
                    farwing_codec::Format {
                        codec: format.codec,
                        chroma: format.chroma,
                        depth: format.depth,
                    }
                    .can_decode()
                };
                let mut cell = Cell {
                    status: Status::Unchecked,
                    detail: String::new(),
                };
                if !supported {
                    cell.status = Status::Unsupported;
                    cell.detail = crate::i18n::t().session.probe_format_not_implemented.into();
                } else if let Some(Err(error)) = device {
                    cell.status = Status::Failed;
                    cell.detail = (crate::i18n::t().session.probe_device_unavailable)(&format!("{error:#}"));
                } else if let Some(sample) = format.sample(size) {
                    let writer = device.as_ref().and_then(|d| d.as_ref().ok());
                    let result = (|| {
                        if let Some(writer) = writer {
                            WindowsVideoDecoder::check_format(
                                writer.device_handle(),
                                format.codec,
                                size.0,
                                size.1,
                                format.depth,
                                format.chroma,
                            )
                            .context(crate::i18n::t().session.probe_driver_config_failed)?;
                            cell.detail = format!("{}\n", crate::i18n::t().session.probe_driver_config_ok);
                        }
                        decode_sample(format, size, sample, writer, cancel.clone())
                    })();
                    match result {
                        Ok(()) => {
                            cell.status = Status::Passed;
                            cell.detail
                                .push_str(crate::i18n::t().session.probe_output_verified);
                        }
                        Err(error) => {
                            cell.status = if error
                                .downcast_ref::<super::software_slot::SoftwarePlaybackBusy>()
                                .is_some()
                            {
                                Status::Busy
                            } else {
                                Status::Failed
                            };
                            cell.detail.push_str(&format!("{error:#}"));
                        }
                    }
                } else {
                    cell.detail = crate::i18n::t().session.probe_no_sample.into();
                }
                if cancel.load(Ordering::Acquire) {
                    return Ok(());
                }
                cell.detail = format!(
                    "{} · {}×{}\n{}",
                    format.label(),
                    size.0,
                    size.1,
                    cell.detail
                );
                tracing::info!(
                    backend = name,
                    format = format.label(),
                    width = size.0,
                    height = size.1,
                    result = cell.status.label(),
                    detail = cell.detail,
                    "decoder diagnostic result"
                );
                if tx.send(Event::Cell(backend, row, column, cell)).is_err() {
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Fixtures contain multiple access units; submit the first complete picture,
/// including all its parameter sets and slices, not the whole stream as one frame.
fn first_picture(format: Format, data: &[u8]) -> Vec<u8> {
    if format.codec == VideoCodec::Av1 {
        return data.to_vec();
    }
    let mut out = Vec::new();
    let mut picture = false;
    for nal in crate::media::video_format::annex_b_units(data) {
        let (vcl, first, aud) = if format.codec == VideoCodec::H264 {
            let kind = nal[0] & 31;
            (
                matches!(kind, 1 | 5),
                nal.get(1).is_some_and(|v| v & 128 != 0),
                kind == 9,
            )
        } else {
            let kind = (nal[0] >> 1) & 63;
            (
                kind <= 31,
                nal.get(2).is_some_and(|v| v & 128 != 0),
                kind == 35,
            )
        };
        if picture && ((vcl && first) || aud) {
            break;
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        picture |= vcl;
    }
    out
}
fn decode_sample(
    format: Format,
    size: (u32, u32),
    sample: &[u8],
    writer: Option<&D3D11SurfaceWriter>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let sample = first_picture(format, sample);
    let signature = crate::media::video_format::parse_stream_format(format.codec, &sample)
        .context(crate::i18n::t().session.probe_sample_header_invalid)?;
    ensure!(
        signature.chroma_format_idc == format.chroma && signature.bit_depth_luma == format.depth,
        "{}",
        crate::i18n::t().session.probe_sample_format_mismatch
    );
    ensure!(
        (signature.visible_width, signature.visible_height) == size,
        "{}",
        crate::i18n::t().session.probe_sample_size_mismatch
    );
    let _slot = if writer.is_none() {
        Some(SoftwareSlot::acquire()?)
    } else {
        None
    };
    let (w, h) = (signature.visible_width, signature.visible_height);
    let points = [
        (w / 8, h / 8),
        (w * 7 / 8, h / 8),
        (w / 2, h / 2),
        (w / 8, h * 7 / 8),
        (w * 7 / 8, h * 7 / 8),
    ];
    let mut decoder = WindowsVideoDecoder::open(&VideoDecoderConfig {
        codec: format.codec,
        width: w,
        height: h,
        mode: if writer.is_some() {
            DecoderMode::Hardware
        } else {
            DecoderMode::Software
        },
        gpu_device: writer.map(|w| w.device_handle()),
        extra_data: Bytes::new(),
    })
    .context(crate::i18n::t().session.probe_backend_create_failed)?;
    decoder.set_notification(DecoderNotification::new(cancel.clone()));
    decoder.push_packet(&sample, 1).context(crate::i18n::t().session.probe_submit_failed)?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        ensure!(!cancel.load(Ordering::Acquire), "{}", crate::i18n::t().session.probe_cancelled);
        if let Some(frame) = decoder.poll_owned_frame().context(crate::i18n::t().session.probe_output_read_failed)? {
            let actual = match frame {
                WindowsDecodedFrame::Gpu(frame) => {
                    use windows::Win32::Graphics::{
                        Direct3D11::D3D11_TEXTURE2D_DESC, Dxgi::Common::*,
                    };
                    let expected = match (format.chroma, format.depth) {
                        (1, 8) => DXGI_FORMAT_NV12,
                        (1, 10) => DXGI_FORMAT_P010,
                        (3, 8) => DXGI_FORMAT_AYUV,
                        (3, 10) => DXGI_FORMAT_Y410,
                        _ => unreachable!(),
                    };
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe {
                        frame.texture().GetDesc(&mut desc);
                    }
                    ensure!(desc.Format == expected, "{}", crate::i18n::t().session.probe_gpu_format_mismatch);
                    let pixels = frame.diagnostic_pixels(&points, &cancel)?;
                    check_codec_pattern(format, size, &points, &pixels)?;
                    let actual = (frame.width(), frame.height());
                    let _surface = writer
                        .context(crate::i18n::t().session.probe_no_d3d11_owner)?
                        .wrap_decoded_surface(frame)?;
                    actual
                }
                WindowsDecodedFrame::Cpu(frame) => {
                    use crate::platform::decoder::WindowsCpuFormat;
                    let expected = match (format.chroma, format.depth) {
                        (1, 8) => WindowsCpuFormat::Nv12,
                        (1, 10) => WindowsCpuFormat::P010,
                        (3, 8) => WindowsCpuFormat::Ayuv,
                        (3, 10) => WindowsCpuFormat::Y410,
                        _ => unreachable!(),
                    };
                    ensure!(frame.format == expected, "{}", crate::i18n::t().session.probe_software_format_mismatch);
                    ensure!(
                        (frame.width, frame.height) == (w, h),
                        "{}",
                        crate::i18n::t().session.probe_software_size_mismatch
                    );
                    let cw = frame.coded_width as usize;
                    let ch = frame.coded_height as usize;
                    let plane = cw * ch;
                    let length = match frame.format {
                        WindowsCpuFormat::Nv12 => plane * 3 / 2,
                        WindowsCpuFormat::P010 => plane * 3,
                        _ => plane * 4,
                    };
                    ensure!(frame.data.len() == length, "{}", crate::i18n::t().session.probe_software_plane_mismatch);
                    let pixels: Vec<_> = points
                        .iter()
                        .map(|&(x, y)| {
                            let (x, y) = (x as usize, y as usize);
                            let at = y * cw + x;
                            match frame.format {
                                WindowsCpuFormat::Nv12 => {
                                    let uv = plane + (y / 2 * cw + x / 2 * 2);
                                    [
                                        frame.data[at] as u16,
                                        frame.data[uv] as u16,
                                        frame.data[uv + 1] as u16,
                                    ]
                                }
                                WindowsCpuFormat::P010 => {
                                    let uv = plane + y / 2 * cw + x / 2 * 2;
                                    let get = |i: usize| {
                                        u16::from_le_bytes(
                                            frame.data[i * 2..i * 2 + 2].try_into().unwrap(),
                                        ) >> 8
                                    };
                                    [get(at), get(uv), get(uv + 1)]
                                }
                                WindowsCpuFormat::Ayuv => [
                                    frame.data[at * 4 + 2] as u16,
                                    frame.data[at * 4 + 1] as u16,
                                    frame.data[at * 4] as u16,
                                ],
                                WindowsCpuFormat::Y410 => {
                                    let v = u32::from_le_bytes(
                                        frame.data[at * 4..at * 4 + 4].try_into().unwrap(),
                                    );
                                    [
                                        ((v >> 10 & 1023) >> 2) as u16,
                                        ((v & 1023) >> 2) as u16,
                                        ((v >> 20 & 1023) >> 2) as u16,
                                    ]
                                }
                            }
                        })
                        .collect();
                    check_codec_pattern(format, size, &points, &pixels)?;
                    if format.codec == VideoCodec::Av1 {
                        let uploader = D3D11SurfaceWriter::new()?;
                        let _surface = uploader.upload_cpu(&frame)?;
                    }
                    (frame.width, frame.height)
                }
            };
            ensure!(actual == (w, h), "{}", crate::i18n::t().session.probe_output_size_mismatch);
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "{}", crate::i18n::t().session.probe_no_output);
        std::thread::sleep(Duration::from_millis(1));
    }
}

fn check_pattern(pixels: &[[u16; 3]]) -> Result<()> {
    for (index, actual) in pixels.iter().enumerate() {
        let expected = [if index == 2 { 192 } else { 64 }, 96, 160];
        ensure!(
            actual
                .iter()
                .zip(expected)
                .all(|(&a, e)| a.abs_diff(e) <= 8),
            "{}",
            (crate::i18n::t().session.probe_pattern_mismatch)(index + 1, &format!("{actual:?}"), &format!("{expected:?}"))
        );
    }
    Ok(())
}

fn check_codec_pattern(
    format: Format,
    (w, h): (u32, u32),
    points: &[(u32, u32)],
    pixels: &[[u16; 3]],
) -> Result<()> {
    if format.codec == VideoCodec::Av1 && format.chroma == 3 {
        for (i, p) in pixels.iter().enumerate() {
            let (x, y) = points[i];
            let expected = [
                if i == 2 { 181 } else { 71 },
                if x < w / 2 { 80 } else { 176 },
                if y < h / 2 { 96 } else { 160 },
            ];
            ensure!(
                p.iter().zip(expected).all(|(a, b)| a.abs_diff(b) <= 8),
                "{}",
                crate::i18n::t().session.probe_av1_444_mismatch
            );
        }
    } else if format.codec == VideoCodec::Av1 {
        for (i, p) in pixels.iter().enumerate() {
            let y = match (format.depth, i == 2) {
                (8, false) => 71,
                (8, true) => 181,
                (_, false) => 91,
                (_, true) => 117,
            };
            ensure!(
                p[0].abs_diff(y) <= 8 && p[1].abs_diff(128) <= 8 && p[2].abs_diff(128) <= 8,
                "{}",
                crate::i18n::t().session.probe_av1_pattern_mismatch
            );
        }
    } else {
        check_pattern(&pixels)?;
    }

    Ok(())
}
