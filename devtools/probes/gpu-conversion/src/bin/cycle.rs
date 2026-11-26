//! Reproduce the production crash: capture restarts create and drop the
//! thread-local `Converter` repeatedly, on the PipeWire callback thread.
//!
//! `Converter::drop` calls eglMakeCurrent(NO_CONTEXT), eglDestroyContext,
//! eglTerminate and gbm_device_destroy. If those globals are shared with another
//! live thread, later GL calls abort inside Mesa (the production SIGABRT).
//! This cycles converters both sequentially and on two concurrent threads.
#![allow(dead_code)]
use std::os::fd::AsRawFd;

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

fn source(w: u32, h: u32) -> anyhow::Result<(cros_libva::Surface<()>, gpu_convert::RgbDma)> {
    let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
    let surface = display
        .create_surfaces(
            cros_libva::VA_RT_FORMAT_RGB32,
            Some(cros_libva::VA_FOURCC_BGRA),
            w,
            h,
            None,
            vec![()],
        )?
        .remove(0);
    let desc = surface.export_prime_writable()?;
    let rgb = gpu_convert::RgbDma {
        fd: desc.objects[0].fd.as_raw_fd(),
        width: w,
        height: h,
        pitch: desc.layers[0].pitch[0],
        offset: desc.layers[0].offset[0],
        fourcc: u32::from_le_bytes(*b"AR24"),
        modifier: desc.objects[0].drm_format_modifier,
    };
    // Leak the descriptor deliberately: the RgbDma fd must stay valid, exactly
    // as the PipeWire buffer does for the callback's duration.
    std::mem::forget(desc);
    Ok((surface, rgb))
}

fn main() -> anyhow::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let w: u32 = args.get(1).map_or(Ok(2880), |s| s.parse())?;
    let h: u32 = args.get(2).map_or(Ok(1800), |s| s.parse())?;
    let rounds: u32 = args.get(3).map_or(Ok(20), |s| s.parse())?;
    let concurrent: bool = args.get(4).map_or(false, |s| s == "concurrent");

    let (_source, rgb) = source(w, h)?;

    let rgb = std::sync::Arc::new(rgb);
    fn cycle(
        id: usize,
        rounds: u32,
        w: u32,
        h: u32,
        rgb: &gpu_convert::RgbDma,
    ) -> anyhow::Result<()> {
        for round in 0..rounds {
            let display = cros_libva::Display::open_drm_display("/dev/dri/renderD128")?;
            let gpu = gpu_convert::Converter::new(display)?;
            for _ in 0..8 {
                let _ = gpu.convert(rgb, (0, 0, w, h), (w, h))?;
            }
            drop(gpu);
            if round % 5 == 0 {
                println!("thread {id} round {round} ok");
            }
        }
        Ok(())
    }

    if concurrent {
        let rgb_a = rgb.clone();
        let rgb_b = rgb.clone();
        let a = std::thread::spawn(move || cycle(0, rounds, w, h, &rgb_a));
        let b = std::thread::spawn(move || cycle(1, rounds, w, h, &rgb_b));
        a.join().unwrap()?;
        b.join().unwrap()?;
    } else {
        cycle(0, rounds, w, h, &rgb)?;
    }
    println!("cycle {w}x{h} rounds={rounds} concurrent={concurrent} completed");
    Ok(())
}
