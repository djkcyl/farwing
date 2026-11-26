//! Verify the GPU scaler plane by plane against a CPU reference.
//!
//! The converter writes the requested output geometry directly. Two things
//! must hold, both with and without an odd crop origin:
//!
//! - at equal sizes the pass stays the exact `texelFetch` it was: every Y and
//!   UV byte is less than one level from the BT.601 limited-range value
//!   computed here from the source texels, that is, it differs from the exact
//!   value only by the final rounding. Drivers do not all round that last step
//!   to nearest (NVIDIA's GLES sends a few percent of the bytes within 0.08 of
//!   a tie the other way), so the probe counts those bytes without failing;
//! - when scaling, every byte stays within `SCALED_TOLERANCE` of a bilinear
//!   reference sampled at the same pixel centres the CPU `scale` used, and a
//!   copy of that reference moved by half a source pixel must fail the same
//!   test, so a sampling grid that is off by half a pixel cannot pass.
//!
//! The source is three sinusoids, one per channel along x, y and the diagonal,
//! so a channel swap, a flipped axis or a shifted grid all change many bytes.
#![allow(dead_code)]
use std::{os::fd::AsRawFd, rc::Rc, sync::Arc};

#[path = "../../../../../src/platform/linux/gpu_convert.rs"]
mod gpu_convert;
/// The probe does no CUDA work beside VA-API, so there is no `cuInit` to
/// order before the VA driver loads.
mod cuda {
    pub fn initialize_before_va() {}
}

/// Largest difference a scaled byte may have from the reference. The GPU
/// interpolates with a few bits of sub-texel precision; on this pattern that
/// moves a byte by under one level (0.74 at most on NVIDIA's GLES), while a
/// grid off by a quarter of a source pixel moves the worst byte by fifteen
/// and half a pixel by thirty.
const SCALED_TOLERANCE: f64 = 2.0;

/// One BGRA pixel per source position.
fn pattern(w: u32, h: u32) -> Vec<u8> {
    let wave = |v: f64, period: f64| {
        (127.5 + 127.0 * (v * std::f64::consts::TAU / period).sin()).round() as u8
    };
    let mut out = vec![0u8; w as usize * h as usize * 4];
    for y in 0..h {
        for x in 0..w {
            let i = ((y * w + x) * 4) as usize;
            let (fx, fy) = (f64::from(x), f64::from(y));
            out[i..i + 4].copy_from_slice(&[
                wave(fx + 2.0 * fy, 29.0),
                wave(fy, 11.0),
                wave(fx, 13.0),
                255,
            ]);
        }
    }
    out
}

/// Normalised RGB of one source pixel, clamped to the image as the GPU's
/// `CLAMP_TO_EDGE` does.
fn texel(source: &[u8], (w, h): (u32, u32), x: i64, y: i64) -> [f64; 3] {
    let x = x.clamp(0, i64::from(w) - 1) as usize;
    let y = y.clamp(0, i64::from(h) - 1) as usize;
    let i = (y * w as usize + x) * 4;
    [
        f64::from(source[i + 2]) / 255.0,
        f64::from(source[i + 1]) / 255.0,
        f64::from(source[i]) / 255.0,
    ]
}

/// Bilinear sample at a source position measured in texels, texel centres at
/// integer positions.
fn bilinear(source: &[u8], size: (u32, u32), x: f64, y: f64) -> [f64; 3] {
    let (x0, y0) = (x.floor(), y.floor());
    let (fx, fy) = (x - x0, y - y0);
    let (x0, y0) = (x0 as i64, y0 as i64);
    let mut c = [0.0; 3];
    for (dx, dy, weight) in [
        (0, 0, (1.0 - fx) * (1.0 - fy)),
        (1, 0, fx * (1.0 - fy)),
        (0, 1, (1.0 - fx) * fy),
        (1, 1, fx * fy),
    ] {
        let t = texel(source, size, x0 + dx, y0 + dy);
        for k in 0..3 {
            c[k] += t[k] * weight;
        }
    }
    c
}

fn luma(c: [f64; 3]) -> f64 {
    16.0 + 65.481 * c[0] + 128.553 * c[1] + 24.966 * c[2]
}

fn chroma(c: [f64; 3]) -> [f64; 2] {
    [
        128.0 - 37.797 * c[0] - 74.203 * c[1] + 112.0 * c[2],
        128.0 + 112.0 * c[0] - 93.786 * c[1] - 18.214 * c[2],
    ]
}

/// Expected Y and interleaved UV planes, unrounded. `shift` moves the
/// sampling grid by that many source pixels on both axes; zero is correct.
fn expected(
    source: &[u8],
    size: (u32, u32),
    (cx, cy, cw, ch): (u32, u32, u32, u32),
    (tw, th): (u32, u32),
    shift: f64,
) -> (Vec<f64>, Vec<f64>) {
    let native = (cw, ch) == (tw, th) && shift == 0.0;
    let (sx, sy) = (f64::from(cw) / f64::from(tw), f64::from(ch) / f64::from(th));
    // The colour behind output pixel (x, y): the texel itself at equal sizes,
    // otherwise the bilinear sample at that pixel's centre in the crop.
    let at = |x: u32, y: u32| {
        if native {
            texel(source, size, i64::from(cx + x), i64::from(cy + y))
        } else {
            bilinear(
                source,
                size,
                f64::from(cx) + (f64::from(x) + 0.5) * sx - 0.5 + shift,
                f64::from(cy) + (f64::from(y) + 0.5) * sy - 0.5 + shift,
            )
        }
    };
    let mut y_plane = Vec::with_capacity(tw as usize * th as usize);
    for y in 0..th {
        for x in 0..tw {
            y_plane.push(luma(at(x, y)));
        }
    }
    // Chroma averages the colours behind the 2x2 block of output pixels.
    let mut uv_plane = Vec::with_capacity(tw as usize * th as usize / 2);
    for y in (0..th).step_by(2) {
        for x in (0..tw).step_by(2) {
            let mut c = [0.0; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let v = at(x + dx, y + dy);
                for k in 0..3 {
                    c[k] += v[k] * 0.25;
                }
            }
            uv_plane.extend(chroma(c));
        }
    }
    (y_plane, uv_plane)
}

/// How far one plane is from its reference.
#[derive(Default)]
struct Error {
    /// Bytes other than the reference rounded to nearest.
    not_nearest: usize,
    /// Largest absolute difference from the unrounded reference.
    max: f64,
    /// PSNR in dB against the rounded reference, infinite when identical.
    psnr: f64,
}

fn compare(got: &[u8], want: &[f64]) -> Error {
    let mut error = Error::default();
    let mut squared = 0.0;
    for (&g, &w) in got.iter().zip(want) {
        let w = w.clamp(0.0, 255.0);
        let g = f64::from(g);
        let rounded = w.round();
        error.not_nearest += usize::from(g != rounded);
        error.max = error.max.max((g - w).abs());
        squared += (g - rounded).powi(2);
    }
    let mse = squared / want.len() as f64;
    error.psnr = 10.0 * (255.0f64.powi(2) / mse).log10();
    error
}

/// Read both planes back, tightly packed, through the encoder's own import.
fn planes(
    nv: &Arc<gpu_convert::OwnedNv12>,
    display: &Rc<cros_libva::Display>,
) -> anyhow::Result<(Vec<u8>, Vec<u8>)> {
    let (w, h) = nv.size();
    let imported = nv.import(display)?;
    let image = cros_libva::Image::derive_from(&imported, (w, h))?;
    let p = image.image();
    let b = image.as_ref();
    let (w, h) = (w as usize, h as usize);
    let mut y = Vec::with_capacity(w * h);
    for row in 0..h {
        let start = p.offsets[0] as usize + row * p.pitches[0] as usize;
        y.extend_from_slice(&b[start..start + w]);
    }
    let mut uv = Vec::with_capacity(w * h / 2);
    for row in 0..h / 2 {
        let start = p.offsets[1] as usize + row * p.pitches[1] as usize;
        uv.extend_from_slice(&b[start..start + w]);
    }
    Ok((y, uv))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    // 2880x1800 at the 1080p quality: the geometry `fit_size` really produces.
    let tw: u32 = args.get(3).map_or(Ok(1728), |s| s.parse())?;
    let th: u32 = args.get(4).map_or(Ok(1080), |s| s.parse())?;
    // An odd crop origin, the way a portal's crop metadata can place it.
    let crop = (7, 5, w.saturating_sub(16), h.saturating_sub(12));
    anyhow::ensure!(
        [w, h, tw, th].iter().all(|n| n % 2 == 0) && tw <= crop.2 && th <= crop.3 && tw >= 2,
        "sizes must be even and the target no larger than the source minus 16x12"
    );

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
    let pixels = pattern(w, h);
    {
        let mut image = cros_libva::Image::derive_from(&source, (w, h))?;
        let p = *image.image();
        for y in 0..h as usize {
            let row = p.offsets[0] as usize + y * p.pitches[0] as usize;
            image.as_mut()[row..row + w as usize * 4]
                .copy_from_slice(&pixels[y * w as usize * 4..(y + 1) * w as usize * 4]);
        }
    }
    let gpu = gpu_convert::Converter::new(display.clone())?;
    let enc_display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;

    let mut failed = false;
    for (label, crop) in [("full", (0, 0, w, h)), ("crop", crop)] {
        // Equal sizes: the exact texelFetch pass, every byte one of the two
        // integers next to the exact value.
        let native = (crop.2, crop.3);
        let (y, uv) = planes(&gpu.convert(&rgb, crop, native)?, &enc_display)?;
        let (want_y, want_uv) = expected(&pixels, (w, h), crop, native, 0.0);
        let (ey, euv) = (compare(&y, &want_y), compare(&uv, &want_uv));
        let ok = ey.max < 1.0 && euv.max < 1.0;
        failed |= !ok;
        println!(
            "native {label} {crop:?}->{native:?}: Y max={:.3} not-nearest={}, UV max={:.3} not-nearest={} {}",
            ey.max,
            ey.not_nearest,
            euv.max,
            euv.not_nearest,
            if ok { "ok" } else { "FAIL" }
        );

        // Scaled: close to the reference everywhere, and far from a grid that
        // is half a source pixel off.
        let scaled = gpu.convert(&rgb, crop, (tw, th))?;
        let (y, uv) = planes(&scaled, &enc_display)?;
        let (want_y, want_uv) = expected(&pixels, (w, h), crop, (tw, th), 0.0);
        let (ey, euv) = (compare(&y, &want_y), compare(&uv, &want_uv));
        let ok = ey.max <= SCALED_TOLERANCE && euv.max <= SCALED_TOLERANCE;
        failed |= !ok;
        println!(
            "scaled {label} {crop:?}->({tw}, {th}): Y psnr={:.2} max={:.2}, UV psnr={:.2} max={:.2} {}",
            ey.psnr,
            ey.max,
            euv.psnr,
            euv.max,
            if ok { "ok" } else { "FAIL" }
        );
        let (off_y, off_uv) = expected(&pixels, (w, h), crop, (tw, th), 0.5);
        let (sy, suv) = (compare(&y, &off_y), compare(&uv, &off_uv));
        let caught = sy.max > SCALED_TOLERANCE && suv.max > SCALED_TOLERANCE;
        failed |= !caught;
        println!(
            "         vs a grid half a pixel off: Y psnr={:.2} max={:.2}, UV psnr={:.2} max={:.2} {}",
            sy.psnr,
            sy.max,
            suv.psnr,
            suv.max,
            if caught {
                "rejected"
            } else {
                "FAIL: the test cannot see the shift"
            }
        );

        // Repeating the conversion must give the same bytes.
        let mut drift = 0;
        for _ in 0..4 {
            let (again_y, again_uv) = planes(&gpu.convert(&rgb, crop, (tw, th))?, &enc_display)?;
            drift += usize::from(again_y != y || again_uv != uv);
        }
        failed |= drift != 0;
        println!(
            "repeat {label}: {drift} of 4 differ {}",
            if drift == 0 { "ok" } else { "FAIL" }
        );
    }
    println!(
        "{}",
        if failed {
            "FAIL"
        } else {
            "PASS: native exact up to rounding, scaled within tolerance on the pixel-centre grid"
        }
    );
    anyhow::ensure!(!failed, "scale probe failed");
    Ok(())
}
