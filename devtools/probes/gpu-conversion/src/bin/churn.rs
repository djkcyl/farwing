//! Production-faithful stress: churn source DMA-BUFs through the real GPU
//! converter and encoder, exactly as the PipeWire capture path does.
//!
//! Models the two capture threads the production log showed (each with its own
//! `Converter`), a fresh source buffer per frame, a fresh owned NV12 per frame,
//! an import on the encoder's own display, and full teardown. Any leak or race
//! in the converter's EGL/GL/VA lifetimes shows up as a crash or RSS/fd growth.
#![allow(dead_code)]
use std::{io::Write, os::fd::AsRawFd, sync::mpsc, time::Instant};

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

/// One capture-thread's worth of state: a source RGB surface it repaints and a
/// converter, plus a separate encoder display and encoder.
fn capture_thread(
    id: usize,
    w: u32,
    h: u32,
    frames: u32,
    tx: mpsc::SyncSender<u64>,
) -> anyhow::Result<()> {
    let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let gpu = gpu_convert::Converter::new(display.clone())?;
    let encode_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let rate = media::encoding::Rate {
        target: 8_000_000,
        peak: 8_000_000,
        fps: 30,
        quality: 4,
    };
    let mut enc =
        encoder::Encoder::new(&encode_display, (w, h), media::encoding::Format::AVC, rate)?;
    let mut out = std::fs::File::create(format!("/tmp/uu-churn-{id}-{w}x{h}.h264"))?;
    let mut bytes = 0u64;
    for n in 0..frames {
        // A fresh compositor-style source surface each frame.
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
        {
            let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
            let p = *image.image();
            for y in 0..h as usize {
                let row = p.offsets[0] as usize + y * p.pitches[0] as usize;
                for x in 0..w as usize {
                    let v = ((x + n as usize) & 0xff) as u8;
                    image.as_mut()[row + x * 4..row + x * 4 + 4].copy_from_slice(&[
                        v,
                        (y & 0xff) as u8,
                        v,
                        255,
                    ]);
                }
            }
        }
        let nv = gpu.convert(&rgb, (0, 0, w, h), (w, h))?;
        let encoded = enc.encode_nv12(&nv, 10_000_000 + n as i64 * 333_333, n % 30 == 0)?;
        for packet in encoded {
            bytes += packet.data.len() as u64;
            out.write_all(&packet.data)?;
        }
        // Drop the source surface and its export while the owned NV12 lives on.
        drop(source);
        drop(desc);
        if n % 500 == 0 {
            let _ = tx.try_send(bytes);
        }
    }
    let _ = tx.try_send(bytes);
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    let frames: u32 = args.get(3).map_or(Ok(3000), |s| s.parse())?;
    let threads: usize = args.get(4).map_or(Ok(1), |s| s.parse())?;
    let (tx, _rx) = mpsc::sync_channel(64);
    let started = Instant::now();
    let mut handles = vec![];
    for id in 0..threads {
        let tx = tx.clone();
        handles.push(std::thread::spawn(move || {
            let r = capture_thread(id, w, h, frames, tx);
            if let Err(e) = &r {
                eprintln!("thread {id} failed: {e:#}");
            }
            r
        }));
    }
    let mut ok = 0;
    for handle in handles {
        if handle.join().map(|r| r.is_ok()).unwrap_or(false) {
            ok += 1;
        }
    }
    println!(
        "churn {w}x{h} frames={frames} threads={threads} completed={ok} elapsed={:.1}s",
        started.elapsed().as_secs_f64()
    );
    Ok(())
}
