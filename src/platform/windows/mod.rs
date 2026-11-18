//! Windows OS and GPU drivers. No account/session ownership in this layer.
mod adapter_type;
pub(crate) mod amf;
pub(crate) mod annotation;
pub(crate) mod assist_link;
pub(crate) mod audio_thread;
pub(crate) mod capture;
pub(crate) mod capture_service;
pub(crate) mod components;
pub(crate) mod cursor;
pub(crate) mod cursor_shape;
pub(crate) mod decoder;
pub(crate) mod device_profile;
pub(crate) mod display;
pub(crate) mod display_hdr;
pub(crate) mod drag_drop;
pub(crate) mod encoder;
pub(crate) mod file_locations;
pub(crate) mod gdi;
pub(crate) mod gpu_conversion;
pub(crate) mod gpu_diagnostics;
mod gpu_readback;
mod gpu_scheduling;
pub(crate) mod host_service;
pub(crate) mod input;
pub(crate) mod loopback;
pub(crate) mod notifications;
pub(crate) mod nvenc;
pub(crate) mod power_request;
pub(crate) mod preprocess;
pub(crate) mod qsv;
pub(crate) mod qsv_allocator;
pub(crate) mod registry;
mod software_encoder;
pub(crate) mod surface;
pub(crate) mod system_power;
pub(crate) mod terminal;
pub(crate) mod transfer;
pub(crate) mod virtual_audio;
pub(crate) mod wol;

use crate::media::encoding as format;
use crate::media::encoding::rate as encoder_rate;
use crate::media::geometry::{fit_size, output_size};
fn lock<T>(value: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) mod graphics;
pub(crate) mod swapchain;
pub(crate) mod video_shader;
