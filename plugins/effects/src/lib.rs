farwing_plugin_api::embed_manifest!(include_bytes!("../manifest.json"));
use farwing_plugin_api::*;
use std::panic::{AssertUnwindSafe, catch_unwind};
#[derive(Clone, Copy)]
enum Effect {
    Color,
    Sharpen,
    Pixelate,
}
unsafe fn describe(
    effect: Effect,
    config: *const u8,
    len: usize,
    out: *mut u8,
    capacity: usize,
    written: *mut usize,
) -> i32 {
    catch_unwind(AssertUnwindSafe(|| {
        if config.is_null() || out.is_null() || written.is_null() || len > 65536 {
            return -1;
        }
        let Ok(config) = serde_json::from_slice::<serde_json::Value>(unsafe {
            std::slice::from_raw_parts(config, len)
        }) else {
            return -1;
        };
        if !config.is_object() {
            return -1;
        }
        let value = |key: &str, default: f32, min: f32, max: f32| {
            let v = match config.get(key) {
                None => Some(default),
                Some(v) => v.as_f64().map(|v| v as f32),
            }?;
            (v.is_finite() && (min..=max).contains(&v)).then_some(v)
        };
        let Some(params) = (|| -> Option<[f32; 4]> {
            Some(match effect {
                Effect::Color => [
                    value("brightness", 0.0, -1.0, 1.0)?,
                    value("contrast", 1.0, 0.0, 3.0)?,
                    value("saturation", 1.0, 0.0, 3.0)?,
                    0.0,
                ],
                Effect::Sharpen => [value("amount", 0.3, 0.0, 2.0)?, 0.0, 0.0, 0.0],
                Effect::Pixelate => {
                    let v = value("block_size", 8.0, 1.0, 64.0)?;
                    if v.fract() != 0.0 {
                        return None;
                    }
                    [v, 0.0, 0.0, 0.0]
                }
            })
        })() else {
            return -1;
        };
        let shader: &[u8] = match effect {
            Effect::Color => include_bytes!(concat!(env!("OUT_DIR"), "/color.cso")),
            Effect::Sharpen => include_bytes!(concat!(env!("OUT_DIR"), "/sharpen.cso")),
            Effect::Pixelate => include_bytes!(concat!(env!("OUT_DIR"), "/pixelate.cso")),
        };
        let program = VideoProgram {
            backend: "d3d11-rgba16f".into(),
            delay_frames: 0,
            passes: vec![VideoPass {
                shader: shader.to_vec(),
                scale_numerator: 1,
                scale_denominator: 1,
                params,
                phases: vec![1.0],
            }],
        };
        let Ok(bytes) = serde_json::to_vec(&program) else {
            return -1;
        };
        if bytes.len() > capacity {
            return -1;
        }
        unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), out, bytes.len());
            *written = bytes.len();
        }
        0
    }))
    .unwrap_or(-2)
}
macro_rules! api {
    ($api:ident,$callback:ident,$kind:ident) => {
        unsafe extern "C" fn $callback(
            c: *const u8,
            n: usize,
            o: *mut u8,
            cap: usize,
            w: *mut usize,
        ) -> i32 {
            unsafe { describe(Effect::$kind, c, n, o, cap, w) }
        }
        static $api: VideoApi = VideoApi {
            abi_version: ABI_VERSION,
            struct_size: size_of::<VideoApi>() as u32,
            describe: $callback,
        };
    };
}
api!(COLOR, color, Color);
api!(SHARPEN, sharpen, Sharpen);
api!(PIXELATE, pixelate, Pixelate);
/// # Safety
/// `name` must reference `len` readable bytes for this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn farwing_video_node_query_v1(
    version: u32,
    name: *const u8,
    len: usize,
) -> *const VideoApi {
    if version != ABI_VERSION || name.is_null() || len > 128 {
        return std::ptr::null();
    }
    match unsafe { std::slice::from_raw_parts(name, len) } {
        b"farwing.effects.color" => &COLOR,
        b"farwing.effects.sharpen" => &SHARPEN,
        b"farwing.effects.pixelate" => &PIXELATE,
        _ => std::ptr::null(),
    }
}
