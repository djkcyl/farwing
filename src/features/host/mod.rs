//! Local controlled-device services; access policy and media sessions have separate owners.
mod access;
pub(crate) mod shared_view;
mod allocation;
pub(crate) mod bandwidth;
pub(crate) mod cameras;
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
pub(crate) mod forwarding;
pub(crate) mod format;
pub(crate) mod hevc;
pub(crate) mod input;
mod keyframe;
pub(crate) mod microphone;
pub(crate) mod parameters;
pub(crate) mod participants;
pub(crate) mod peer;
pub(crate) mod power;
mod protection;
pub(crate) mod recovery;
mod settings;
pub(crate) use settings::HostSettings;
mod track;
mod link;
mod transport;
pub(crate) mod wol;

use std::sync::{Mutex, MutexGuard};

pub(crate) fn lock<T>(value: &Mutex<T>) -> MutexGuard<'_, T> {
    value
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) use access::{
    ParticipantLimit,
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
    /// The custom quality's bitrate (bps).
    pub bitrate: u32,
    pub quality: parameters::Quality,
    /// Where automatic quality starts.
    pub auto_tier: parameters::Tier,
    pub revision: u64,
    pub format: format::Format,
    pub color: format::Color,
    /// A size cap below what the decoder takes (the shared stream).
    pub requested_maximum: Option<(u32, u32)>,
    pub maximum: (u32, u32),
    pub maximum_fps: u32,
    /// The largest tier both sides take for this screen.
    pub maximum_tier: parameters::Tier,
    pub sending: bool,
    pub capturing: bool,
    pub cursor_capture: bool,
    pub weak_network: parameters::WeakNetwork,
    pub cinema: bool,
    /// Frame-rate cap while the controller's decoder falls behind; 0 = none.
    pub receiver_fps: u32,
    /// The codec the controller asked for from the session's start (its
    /// later settings keep it current); None lets the host choose.
    pub requested_codec: Option<format::Codec>,
}
impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            requested_fps: 30,
            fps_limit: 144,
            bitrate: 8_000_000,
            quality: parameters::Quality::Tier(parameters::Tier::P1080),
            auto_tier: parameters::Tier::P1080,
            revision: 0,
            format: format::Format::AVC,
            color: format::Color::Sdr,
            requested_maximum: None,
            maximum: (3840, 2160),
            maximum_fps: 144,
            maximum_tier: parameters::Tier::P2160,
            sending: true,
            capturing: true,
            cursor_capture: false,
            weak_network: parameters::WeakNetwork::Bitrate,
            cinema: false,
            receiver_fps: 0,
            requested_codec: None,
        }
    }
}

pub(crate) use crate::media::geometry::output_size;

use crate::platform::transfer;
pub(crate) use crate::platform::{capture, encoder};
