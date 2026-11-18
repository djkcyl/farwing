//! Local controlled-device services; access policy and media sessions have separate owners.
mod access;
mod allocation;
pub(crate) mod annotation;
pub(crate) mod assist;
pub(crate) mod audio;
mod burst;
pub(crate) mod clipboard;
mod congestion;
pub(crate) mod desktop;
pub(crate) mod displays;
mod encoding_settings;
mod fec;
pub(crate) mod files;
pub(crate) mod format;
mod hevc;
pub(crate) mod input;
mod keyframe;
pub(crate) mod microphone;
pub(crate) mod network;
pub(crate) mod parameters;
pub(crate) mod peer;
pub(crate) mod power;
mod protection;
mod settings;
pub(crate) mod terminal;
mod track;
mod transport;
pub(crate) mod update_resume;
pub(crate) mod wol;

use std::sync::{Mutex, MutexGuard};

pub(crate) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) use access::{
    AccessRequest, ActiveEncoding, Handle, Lease, RemoteAction, SessionLease, Status,
};
pub(crate) use encoding_settings::{EncoderCodec, EncoderMode, EncodingSettings};

pub(crate) fn enroll_settings(account: &str, device: &str) -> anyhow::Result<()> {
    settings::Store::new(account, device)?.enroll()
}
pub(crate) fn restore_portable_settings(account: &str, device: &str) -> anyhow::Result<()> {
    settings::Store::new(account, device)?.restore_portable()
}
pub(crate) fn transfer_guest_settings(device: &str, enroll: bool) -> anyhow::Result<()> {
    let store = settings::Store::guest(device)?;
    if enroll {
        store.enroll()
    } else {
        store.restore_portable()
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VideoConfig {
    pub fps: u32,
    pub requested_fps: u32,
    pub fps_limit: u32,
    pub bitrate: u32,
    pub quality: i32,
    pub auto_quality: i32,
    pub revision: u64,
    pub reported_quality: i32,
    pub format: format::Format,
    pub color: format::Color,
    // Runtime CaptureSetting scale bounds, separate from initial decoder caps.
    pub requested_maximum: Option<(u32, u32)>,
    pub maximum: (u32, u32),
    pub maximum_fps: u32,
    pub maximum_quality: i32,
    pub sending: bool,
    pub capturing: bool,
    pub cursor_capture: bool,
}
impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            requested_fps: 30,
            fps_limit: 144,
            bitrate: 8_000_000,
            quality: 2,
            auto_quality: 2,
            revision: 0,
            reported_quality: 2,
            format: format::Format::AVC,
            color: format::Color::Sdr,
            requested_maximum: None,
            maximum: (3840, 2160),
            maximum_fps: 144,
            maximum_quality: 4,
            sending: true,
            capturing: true,
            cursor_capture: false,
        }
    }
}

pub(crate) use crate::media::geometry::output_size;

use crate::platform::transfer;
pub(crate) use crate::platform::{capture, encoder};
