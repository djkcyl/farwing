//! Exhaustive current-backend checks. No automatic candidate or adapter fallback.
// The report types are shared, but only Windows has a check to fill them in
// (it drives DXVA11 adapters), so elsewhere most of this module goes unused.
#![cfg_attr(
    not(windows),
    allow(
        dead_code,
        unused_imports,
        reason = "The DXVA11 check is Windows-only."
    )
)]
use super::software_slot::SoftwareSlot;
use crate::media::decode_api::{DecoderMode, DecoderNotification, VideoDecoderConfig};
use crate::media::{LocalDisplayInfo, VideoCodec};
#[cfg(windows)]
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
        format!(
            "{} · {} · {}位",
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
            Self::Pending => "待检查",
            Self::Passed => "通过",
            Self::Failed => "失败",
            Self::Unsupported => "未实现",
            Self::Busy => "资源占用",
            Self::Unchecked => "未检查",
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

/// The check drives DXVA11 adapters one by one; VA-API has no counterpart yet,
/// so there is nothing to report here rather than a partial matrix.
#[cfg(not(windows))]
pub(crate) fn run(
    _display: LocalDisplayInfo,
    _tx: &Sender<Event>,
    _cancel: Arc<AtomicBool>,
) -> Result<()> {
    anyhow::bail!("解码检查目前只覆盖 Windows 的 DXVA11")
}

#[cfg(windows)]
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
            Err(anyhow::anyhow!("没有可用的硬件适配器")),
        )],
        Err(error) => vec![("DXVA11 枚举".into(), Err(error))],
    };
    let mut all: Vec<_> = devices
        .into_iter()
        .map(|(name, writer)| (format!("DXVA11 · {name}"), Some(name), Some(writer)))
        .collect();
    all.push(("OpenUUYC · 软件".into(), None, None));
    let mut sizes: Vec<_> = crate::protocol::capability::QUALITY_DIMENSIONS
        .iter()
        .map(|&(w, h)| (w as u32, h as u32))
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
                    openuuyc_codec::Format {
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
                    cell.detail = "本程序此后端未实现该格式".into();
                } else if let Some(Err(error)) = device {
                    cell.status = Status::Failed;
                    cell.detail = format!("设备不可用：{error:#}");
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
                            .context("驱动配置创建失败")?;
                            cell.detail = "驱动配置：可创建\n".into();
                        }
                        decode_sample(format, size, sample, writer, cancel.clone())
                    })();
                    match result {
                        Ok(()) => {
                            cell.status = Status::Passed;
                            cell.detail
                                .push_str("实际解码出帧，尺寸、像素格式及合成图案采样校验通过");
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
                    cell.detail = "没有此尺寸的内置样本，未执行实际解码".into();
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
#[cfg(windows)]
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
#[cfg(windows)]
fn decode_sample(
    format: Format,
    size: (u32, u32),
    sample: &[u8],
    writer: Option<&D3D11SurfaceWriter>,
    cancel: Arc<AtomicBool>,
) -> Result<()> {
    let sample = first_picture(format, sample);
    let signature = crate::media::video_format::parse_stream_format(format.codec, &sample)
        .context("样本序列头无效")?;
    ensure!(
        signature.chroma_format_idc == format.chroma && signature.bit_depth_luma == format.depth,
        "样本格式不匹配"
    );
    ensure!(
        (signature.visible_width, signature.visible_height) == size,
        "样本尺寸与目标不一致"
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
    .context("创建指定后端")?;
    decoder.set_notification(DecoderNotification::new(cancel.clone()));
    decoder.push_packet(&sample, 1).context("提交样本码流")?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        ensure!(!cancel.load(Ordering::Acquire), "检查已取消");
        if let Some(frame) = decoder.poll_owned_frame().context("读取解码输出")? {
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
                    ensure!(desc.Format == expected, "GPU输出像素格式与样本不一致");
                    let pixels = frame.diagnostic_pixels(&points, &cancel)?;
                    check_codec_pattern(format, size, &points, &pixels)?;
                    let actual = (frame.width(), frame.height());
                    let _surface = writer
                        .context("缺少D3D11输出拥有者")?
                        .wrap_decoded_surface(frame)?;
                    actual
                }
                WindowsDecodedFrame::Cpu(frame) => {
                    use crate::platform::decoder::WindowsCpuFormat;
                    let expected = if format.codec == VideoCodec::Av1 {
                        match (format.chroma, format.depth) {
                            (1, 8) => WindowsCpuFormat::Nv12,
                            (1, 10) => WindowsCpuFormat::P010,
                            (3, 8) => WindowsCpuFormat::Ayuv,
                            (3, 10) => WindowsCpuFormat::Y410,
                            _ => unreachable!(),
                        }
                    } else if format.chroma == 3 {
                        WindowsCpuFormat::I444
                    } else {
                        WindowsCpuFormat::Nv12
                    };
                    ensure!(frame.format == expected, "软件输出像素格式与样本不一致");
                    ensure!(
                        (frame.width, frame.height) == (w, h),
                        "软件输出尺寸与样本不一致"
                    );
                    let cw = frame.coded_width as usize;
                    let ch = frame.coded_height as usize;
                    let plane = cw * ch;
                    let length = match frame.format {
                        WindowsCpuFormat::Nv12 => plane * 3 / 2,
                        WindowsCpuFormat::P010 | WindowsCpuFormat::I444 => plane * 3,
                        _ => plane * 4,
                    };
                    ensure!(frame.data.len() == length, "软件输出平面长度不符");
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
                                WindowsCpuFormat::I444 => [
                                    frame.data[at] as u16,
                                    frame.data[plane + at] as u16,
                                    frame.data[2 * plane + at] as u16,
                                ],
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
            ensure!(actual == (w, h), "解码输出尺寸与样本不一致");
            return Ok(());
        }
        ensure!(Instant::now() < deadline, "创建成功，但2秒内没有解码输出");
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[cfg(windows)]
fn check_pattern(pixels: &[[u16; 3]]) -> Result<()> {
    for (index, actual) in pixels.iter().enumerate() {
        let expected = [if index == 2 { 192 } else { 64 }, 96, 160];
        ensure!(
            actual
                .iter()
                .zip(expected)
                .all(|(&a, e)| a.abs_diff(e) <= 8),
            "合成图案第{}点像素不符：实际{:?}，预期{:?}",
            index + 1,
            actual,
            expected
        );
    }
    Ok(())
}

#[cfg(windows)]
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
                "AV1 4:4:4合成色块像素不符"
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
                "AV1合成图案像素不符"
            );
        }
    } else {
        check_pattern(&pixels)?;
    }

    Ok(())
}
