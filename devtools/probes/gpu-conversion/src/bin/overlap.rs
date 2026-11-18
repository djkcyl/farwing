//! Decisive throughput test: does the GPU conversion overlap the encoder?
//!
//! Codex measured conversion and encode back-to-back on one thread, where the
//! converter's per-frame `glFinish` (~12 ms) is added to the encode cost. In
//! production the PipeWire callback thread runs the conversion while the worker
//! thread runs the encoder, so a conversion whose GPU work is only ~2 ms but
//! whose finish blocks ~12 ms can hide inside the ~30 ms encode.
//!
//! This models both the current CPU path and the GPU path under the same
//! split: a producer thread prepares the frame (DMA-BUF copy for CPU, GPU
//! conversion for GPU) and a consumer thread encodes it.
#![allow(dead_code)]
use std::{io::Write, os::fd::AsRawFd, sync::Arc, sync::mpsc, time::Instant};

mod media {
    pub mod encoding {
        #[derive(Clone, Copy, Debug, PartialEq)]
        pub enum Codec {
            H264,
        }
        #[derive(Clone, Copy, Debug)]
        pub struct Format {
            pub codec: Codec,
            pub chroma: u8,
            pub depth: u8,
        }
        impl Format {
            pub const AVC: Self = Self {
                codec: Codec::H264,
                chroma: 1,
                depth: 8,
            };
            pub fn valid(self) -> bool {
                true
            }
        }
        #[derive(Clone, Copy)]
        pub struct Rate {
            pub target: u32,
            pub peak: u32,
            pub fps: u32,
            pub quality: i32,
        }
        pub struct Encoded {
            pub data: Vec<u8>,
            pub keyframe: bool,
            pub timestamp_100ns: i64,
            pub is_new: bool,
            pub timing: Option<()>,
            pub format: Format,
            pub color: (),
        }
        pub enum Color {
            Sdr,
        }
        impl Color {
            pub fn space(self, _: Option<()>) {}
        }
    }
}
#[path = "../../../../../src/platform/linux/vaapi_encoder.rs"]
mod encoder;
#[path = "../../../../../src/platform/linux/gpu_convert.rs"]
mod gpu_convert;
/// The probe does no CUDA work beside VA-API, so there is no `cuInit` to
/// order before the VA driver loads.
mod cuda {
    pub fn initialize_before_va() {}
}

fn rate() -> media::encoding::Rate {
    media::encoding::Rate {
        target: 8_000_000,
        peak: 8_000_000,
        fps: 30,
        quality: 4,
    }
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    let frames: u32 = args.get(3).map_or(Ok(96), |s| s.parse())?;

    let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let source = display
        .create_surfaces(
            cros_libva::VA_RT_FORMAT_RGB32,
            Some(cros_libva::VA_FOURCC_BGRA),
            w,
            h,
            None,
            vec![()],
        )?
        .remove(0);
    let desc = source.export_prime_writable()?;
    let rgb = gpu_convert::RgbDma {
        fd: desc.objects[0].fd.as_raw_fd(),
        width: w,
        height: h,
        pitch: desc.layers[0].pitch[0],
        offset: desc.layers[0].offset[0],
        fourcc: u32::from_le_bytes(*b"AR24"),
        modifier: desc.objects[0].drm_format_modifier,
    };
    let mut pattern = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            pattern[i..i + 4].copy_from_slice(&[
                (x % 251) as u8,
                (y % 241) as u8,
                ((x ^ y) % 239) as u8,
                255,
            ]);
        }
    }
    let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
    let p = *image.image();
    for y in 0..h as usize {
        let dst = p.offsets[0] as usize + y * p.pitches[0] as usize;
        image.as_mut()[dst..dst + w as usize * 4]
            .copy_from_slice(&pattern[y * w as usize * 4..(y + 1) * w as usize * 4]);
    }
    drop(image);

    let gpu = gpu_convert::Converter::new(display.clone())?;
    let encode_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let mut enc = encoder::Encoder::new(
        &encode_display,
        (w, h),
        media::encoding::Format::AVC,
        rate(),
    )?;
    let mut out = std::fs::File::create(format!("/tmp/uu-overlap-{w}x{h}.h264"))?;

    // --- Serial GPU: what Codex measured (one thread does both). ---
    let (mut c, mut e) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
    for n in 0..frames {
        let t = Instant::now();
        let nv = gpu.convert(&rgb, (0, 0, w, h), (w, h))?;
        c += t.elapsed();
        let t = Instant::now();
        let encoded = enc.encode_nv12(&nv, 10_000_000 + n as i64 * 333_333, n % 30 == 0)?;
        e += t.elapsed();
        for packet in encoded {
            out.write_all(&packet.data)?;
        }
    }
    println!(
        "serial GPU  {w}x{h} convert={:.3} encode={:.3} combined={:.3} ms/frame",
        c.as_secs_f64() * 1000. / f64::from(frames),
        e.as_secs_f64() * 1000. / f64::from(frames),
        (c + e).as_secs_f64() * 1000. / f64::from(frames),
    );
    drop(enc);
    drop(gpu);

    // --- Pipelined CPU: producer copies the DMA-BUF out, consumer uploads+encodes. ---
    run_pipeline(w, h, frames, &pattern, &rgb, false);
    // --- Pipelined GPU: producer converts on the GPU, consumer imports+encodes. ---
    run_pipeline(w, h, frames, &pattern, &rgb, true);
    Ok(())
}

fn run_pipeline(w: u32, h: u32, frames: u32, pattern: &[u8], rgb: &gpu_convert::RgbDma, gpu: bool) {
    let pattern = pattern.to_vec();
    let rgb = gpu_convert::RgbDma {
        fd: rgb.fd,
        width: rgb.width,
        height: rgb.height,
        pitch: rgb.pitch,
        offset: rgb.offset,
        fourcc: rgb.fourcc,
        modifier: rgb.modifier,
    };
    enum Item {
        Pixels(Vec<u8>),
        Nv12(Arc<gpu_convert::OwnedNv12>),
    }
    let (tx, rx) = mpsc::sync_channel::<Item>(2);
    let producer = std::thread::spawn(move || -> anyhow::Result<std::time::Duration> {
        let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
        let mut total = std::time::Duration::ZERO;
        if gpu {
            let gpu = gpu_convert::Converter::new(display)?;
            for _ in 0..frames {
                let t = Instant::now();
                let nv = gpu.convert(&rgb, (0, 0, w, h), (w, h))?;
                total += t.elapsed();
                if tx.send(Item::Nv12(nv)).is_err() {
                    break;
                }
            }
        } else {
            for _ in 0..frames {
                let t = Instant::now();
                // Models the DMA-BUF import plus the copy into an owned BGRA frame.
                let pixels = pattern.clone();
                total += t.elapsed();
                if tx.send(Item::Pixels(pixels)).is_err() {
                    break;
                }
            }
        }
        Ok(total)
    });
    let consumer = std::thread::spawn(move || -> anyhow::Result<(std::time::Duration, u64)> {
        let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
        let mut enc =
            encoder::Encoder::new(&display, (w, h), media::encoding::Format::AVC, rate()).unwrap();
        let mut total = std::time::Duration::ZERO;
        let mut bytes = 0u64;
        for (n, item) in rx.iter().enumerate() {
            let ts = 10_000_000 + n as i64 * 333_333;
            let t = Instant::now();
            let encoded = match &item {
                Item::Pixels(pixels) => enc.encode(pixels, w as usize * 4, ts, n % 30 == 0),
                Item::Nv12(nv) => enc.encode_nv12(nv, ts, n % 30 == 0),
            }?;
            total += t.elapsed();
            for packet in encoded {
                bytes += packet.data.len() as u64;
            }
        }
        Ok((total, bytes))
    });
    let wall = Instant::now();
    let prep = producer.join().unwrap().unwrap();
    let (encode, bytes) = consumer.join().unwrap().unwrap();
    let elapsed = wall.elapsed();
    println!(
        "pipelined {} {w}x{h} prep={:.3} encode={:.3} wall={:.3} fps={:.2} bytes={bytes}",
        if gpu { "GPU" } else { "CPU" },
        prep.as_secs_f64() * 1000. / f64::from(frames),
        encode.as_secs_f64() * 1000. / f64::from(frames),
        elapsed.as_secs_f64() * 1000. / f64::from(frames),
        f64::from(frames) / elapsed.as_secs_f64(),
    );
}
