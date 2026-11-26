//! Hardware-only reproducible validation, intentionally outside the GUI.
#![allow(dead_code)]
use std::{io::Write, os::fd::AsRawFd};
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
#[path = "../../../../src/platform/linux/vaapi_encoder.rs"]
mod encoder;
#[path = "../../../../src/platform/linux/gpu_convert.rs"]
mod gpu_convert;
/// The probe does no CUDA work beside VA-API, so there is no `cuInit` to
/// order before the VA driver loads.
mod cuda {
    pub fn initialize_before_va() {}
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(640), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(480), |s| s.parse())?;
    let frames: u32 = args.get(3).map_or(Ok(64), |s| s.parse())?;
    anyhow::ensure!(
        w % 16 == 0 && h % 2 == 0 && w >= 32 && frames > 0,
        "invalid dimensions/count"
    );
    let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let gpu = gpu_convert::Converter::new(display.clone())?;
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
    let expected: [[u8; 3]; 8] = [
        [16, 128, 128],
        [235, 128, 128],
        [81, 90, 240],
        [145, 54, 34],
        [41, 240, 110],
        [170, 166, 16],
        [106, 202, 222],
        [210, 16, 146],
    ];
    // Separate displays model capture-thread allocation and encoder-thread import.
    // Nonuniform rows and an offset crop catch flips, pitch mistakes and UV
    // averaging errors that flat color bars cannot detect. Import on a worker
    // thread after overwriting the producer's buffer.
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
    let (cx, cy, cw, ch) = (7u32, 5u32, w - 16, h - 12);
    let snapshot = gpu.convert(&rgb, (cx, cy, cw, ch), (cw, ch))?;
    let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
    image.as_mut().fill(0);
    drop(image);
    std::thread::spawn(move || -> anyhow::Result<()> {
        let d = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
        let imported = snapshot.import(&d)?;
        let image = cros_libva::Image::derive_from(&imported, (cw, ch))?;
        let p = image.image();
        let b = image.as_ref();
        let color = |x: u32, y: u32| {
            let i = (((y + cy) * w + x + cx) * 4) as usize;
            [
                pattern[i + 2] as f64 / 255.,
                pattern[i + 1] as f64 / 255.,
                pattern[i] as f64 / 255.,
            ]
        };
        for y in 0..ch {
            for x in 0..cw {
                let c = color(x, y);
                let want = (16. + 65.481 * c[0] + 128.553 * c[1] + 24.966 * c[2]).round() as i32;
                let got = b[(p.offsets[0] + y * p.pitches[0] + x) as usize];
                anyhow::ensure!(
                    (got as i32 - want).abs() <= 1,
                    "gradient Y mismatch {x},{y}"
                );
            }
        }
        for y in (0..ch).step_by(2) {
            for x in (0..cw).step_by(2) {
                let mut c = [0.; 3];
                for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                    let v = color(x + dx, y + dy);
                    for k in 0..3 {
                        c[k] += v[k] * 0.25;
                    }
                }
                let uv = [
                    (128. - 37.797 * c[0] - 74.203 * c[1] + 112. * c[2]).round() as i32,
                    (128. + 112. * c[0] - 93.786 * c[1] - 18.214 * c[2]).round() as i32,
                ];
                for k in 0..2 {
                    let got = b[(p.offsets[1] + y / 2 * p.pitches[1] + x) as usize + k];
                    anyhow::ensure!(
                        (got as i32 - uv[k]).abs() <= 1,
                        "gradient UV mismatch {x},{y}"
                    );
                }
            }
        }
        // Exercise the BGRA readback used by software/NVENC/resize fallback.
        let pixels = snapshot.pixels()?;
        anyhow::ensure!(
            pixels.len() == cw as usize * ch as usize * 4,
            "fallback dimensions"
        );
        Ok(())
    })
    .join()
    .map_err(|_| anyhow::anyhow!("worker panicked"))??;
    println!(
        "PASS: cross-thread immutable snapshot, gradient orientation, odd crop origin, chroma average, fallback readback"
    );
    let encode_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let mut cpu_pixels = vec![0u8; w as usize * h as usize * 4];
    for mode in ["cpu", "gpu"] {
        let rate = media::encoding::Rate {
            target: 8_000_000,
            peak: 8_000_000,
            fps: 30,
            quality: 4,
        };
        let mut enc =
            encoder::Encoder::new(&encode_display, (w, h), media::encoding::Format::AVC, rate)?;
        let mut out = std::fs::File::create(format!("/tmp/uu-{mode}-conversion-{w}x{h}.h264"))?;
        let (mut convert, mut encode) = (std::time::Duration::ZERO, std::time::Duration::ZERO);
        for n in 0..frames {
            for y in 0..h {
                for x in 0..w {
                    let i = ((y * w + x) * 4) as usize;
                    let c = colors[((x / (w / 8) + n) % 8) as usize];
                    cpu_pixels[i..i + 4].copy_from_slice(&[c[0], c[1], c[2], 255]);
                }
            }
            let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
            let info = *image.image();
            for y in 0..h as usize {
                let dst = info.offsets[0] as usize + y * info.pitches[0] as usize;
                image.as_mut()[dst..dst + w as usize * 4]
                    .copy_from_slice(&cpu_pixels[y * w as usize * 4..(y + 1) * w as usize * 4]);
            }
            drop(image);
            let started = std::time::Instant::now();
            let nv = if mode == "gpu" {
                Some(gpu.convert(&rgb, (0, 0, w, h), (w, h))?)
            } else {
                None
            };
            convert += started.elapsed();
            if let Some(nv) = &nv {
                // Producer is immediately reused. The NV12 snapshot must not change.
                let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
                image.as_mut().fill(0);
                drop(image);
                let imported = nv.import(&encode_display)?;
                let image = cros_libva::Image::derive_from(&imported, (w, h))?;
                let p = image.image();
                let b = image.as_ref();
                for k in 0..8 {
                    let x = (k * w / 8 + w / 16) & !1;
                    let y = h / 2;
                    let vals = [
                        b[p.offsets[0] as usize + (y * p.pitches[0] + x) as usize],
                        b[p.offsets[1] as usize + (y / 2 * p.pitches[1] + x) as usize],
                        b[p.offsets[1] as usize + (y / 2 * p.pitches[1] + x + 1) as usize],
                    ];
                    let want = expected[((k + n) % 8) as usize];
                    for c in 0..3 {
                        anyhow::ensure!(
                            (vals[c] as i32 - want[c] as i32).abs() <= 1,
                            "frame {n} bar {k}: {vals:?} != {want:?}"
                        );
                    }
                }
            }
            let ts = 10_000_000 + n as i64 * 333_333;
            let idr = n % 17 == 0;
            let started = std::time::Instant::now();
            let encoded = if let Some(nv) = nv {
                enc.encode_nv12(&nv, ts, idr)?
            } else {
                enc.encode(&cpu_pixels, w as usize * 4, ts, idr)?
            };
            encode += started.elapsed();
            anyhow::ensure!(
                encoded[0].timestamp_100ns == ts && encoded[0].keyframe == idr,
                "timestamp/IDR mismatch"
            );
            for packet in encoded {
                out.write_all(&packet.data)?;
            }
        }
        println!(
            "{mode} {w}x{h} frames={frames} convert_ms={:.3} encode_ms={:.3} combined_ms={:.3}",
            convert.as_secs_f64() * 1000. / frames as f64,
            encode.as_secs_f64() * 1000. / frames as f64,
            (convert + encode).as_secs_f64() * 1000. / frames as f64
        );
    }
    println!("PASS: dynamic colors, source reuse, separate displays, IDR and timestamps");
    Ok(())
}
