//! Does the encoder's VA import respect the GL conversion's writes without an
//! explicit glFinish, when the two live on different VA displays?
//!
//! The converter writes NV12 through EGL/GLES into a VA-owned DMA-BUF and the
//! encoder later imports that same buffer into its own VA display. If the
//! dma-buf implicit fence carries the write ordering, the per-frame glFinish
//! (measured ~12 ms of latency for ~2 ms of GPU work) can be dropped.
//!
//! The producer overwrites its source right after submitting, so a missing
//! fence shows up as wrong pixels rather than a silent race.
#![allow(dead_code)]
use std::{io::Write, os::fd::AsRawFd, sync::Arc, time::Instant};

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

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    let frames: u32 = args.get(3).map_or(Ok(64), |s| s.parse())?;

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
    let colors: [[u8; 3]; 8] = [
        [0, 0, 0],
        [255, 255, 255],
        [0, 0, 255],
        [0, 255, 0],
        [255, 0, 0],
        [255, 255, 0],
        [255, 0, 255],
        [0, 255, 255],
    ];
    let expect: [[u8; 3]; 8] = [
        [16, 128, 128],
        [235, 128, 128],
        [81, 90, 240],
        [145, 54, 34],
        [41, 240, 110],
        [170, 166, 16],
        [106, 202, 222],
        [210, 16, 146],
    ];
    let paint = |bar: usize| -> anyhow::Result<()> {
        let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
        let p = *image.image();
        let c = colors[bar % 8];
        for y in 0..h as usize {
            let dst = p.offsets[0] as usize + y * p.pitches[0] as usize;
            for x in 0..w as usize {
                image.as_mut()[dst + x * 4..dst + x * 4 + 4]
                    .copy_from_slice(&[c[0], c[1], c[2], 255]);
            }
        }
        Ok(())
    };

    let gpu = gpu_convert::Converter::new(display.clone())?;
    let enc_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let rate = media::encoding::Rate {
        target: 8_000_000,
        peak: 8_000_000,
        fps: 30,
        quality: 4,
    };
    let mut enc = encoder::Encoder::new(&enc_display, (w, h), media::encoding::Format::AVC, rate)?;
    let mut out = std::fs::File::create(format!("/tmp/uu-sync-{w}x{h}.h264"))?;

    let mut import_time = std::time::Duration::ZERO;
    let mut encode_time = std::time::Duration::ZERO;
    let mut mismatches = 0u32;
    for n in 0..frames {
        let bar = (n as usize) % 8;
        paint(bar)?;
        // Producer returns immediately after submitting the shader.
        let nv: Arc<gpu_convert::OwnedNv12> = gpu.convert(&rgb, (0, 0, w, h), (w, h))?;
        // Immediately reuse the source with a different colour.
        paint((bar + 1) % 8)?;
        // Import on the encoder's own display and read the pixels the encoder
        // will actually encode.
        let t = Instant::now();
        let imported = nv.import(&enc_display)?;
        import_time += t.elapsed();
        let image = cros_libva::Image::derive_from(&imported, (w, h))?;
        let p = image.image();
        let b = image.as_ref();
        for k in 0usize..8 {
            let x = (k as u32 * w / 8 + w / 16) & !1;
            let y = h / 2;
            let vals = [
                b[p.offsets[0] as usize + (y * p.pitches[0] + x) as usize],
                b[p.offsets[1] as usize + (y / 2 * p.pitches[1] + x) as usize],
                b[p.offsets[1] as usize + (y / 2 * p.pitches[1] + x + 1) as usize],
            ];
            for c in 0..3 {
                if (vals[c] as i32 - expect[bar][c] as i32).abs() > 1 {
                    mismatches += 1;
                }
            }
        }
        drop(image);
        let t = Instant::now();
        let encoded = enc.encode_nv12(&nv, 10_000_000 + n as i64 * 333_333, n % 30 == 0)?;
        encode_time += t.elapsed();
        for packet in encoded {
            out.write_all(&packet.data)?;
        }
    }
    println!(
        "no-finish {w}x{h} frames={frames} color_mismatches={mismatches} import_ms={:.3} encode_nv12_ms={:.3}",
        import_time.as_secs_f64() * 1000. / f64::from(frames),
        encode_time.as_secs_f64() * 1000. / f64::from(frames),
    );
    Ok(())
}
