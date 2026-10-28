//! The shared viewer stream (`docs/protocol/v2-design.md` §4.4). While
//! several participants are connected, everyone except the holder of the
//! control token watches one stream per screen: 1080p (高清) or, for those
//! who chose 流畅, 720p; 60 fps, cinema mode, in the best format all of
//! them can decode. One encoder serves each, the bandwidth stays bounded
//! and everyone sees the same picture; the holder keeps its own settings
//! and low latency.
use crate::features::host::VideoConfig;
use crate::features::host::format::{Codec, Color, Format, Negotiated};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The quality tier of the shared stream (1920×1080; the tier also sets
/// the bitrate bounds)...
pub(crate) const QUALITY: i32 = 2;
/// ...or of 流畅 (1280×720) for those who chose it.
const SMOOTH: i32 = 1;
pub(crate) const FPS: u32 = 60;
/// One size bound for everyone: the tier, not each decoder's limit, sizes
/// the picture, and equal bounds let the viewers share an encoder.
const MAXIMUM: (u32, u32) = (3840, 2160);

/// Formats in order of preference (4:2:0 only: every viewer decodes it).
const PREFERENCE: [Format; 5] = [
    Format { codec: Codec::Av1, chroma: 1, depth: 10 },
    Format { codec: Codec::Av1, chroma: 1, depth: 8 },
    Format { codec: Codec::H265, chroma: 1, depth: 10 },
    Format { codec: Codec::H265, chroma: 1, depth: 8 },
    Format { codec: Codec::H264, chroma: 1, depth: 8 },
];

struct Viewer {
    participant: Option<u64>,
    formats: Vec<Format>,
}

static VIEWERS: LazyLock<Mutex<HashMap<usize, Viewer>>> = LazyLock::new(Default::default);
static NEXT: AtomicUsize = AtomicUsize::new(1);

fn viewers() -> std::sync::MutexGuard<'static, HashMap<usize, Viewer>> {
    VIEWERS.lock().unwrap_or_else(|p| p.into_inner())
}

/// A participant's video stream taking part in the choice of format;
/// leaving on drop.
pub(crate) struct Registration(usize);

impl Drop for Registration {
    fn drop(&mut self) {
        viewers().remove(&self.0);
    }
}

/// The 4:2:0 formats this participant's session can carry.
pub(crate) fn formats(negotiated: &Negotiated) -> Vec<Format> {
    PREFERENCE
        .into_iter()
        .filter(|format| {
            negotiated.permits_format(*format)
                && negotiated
                    .choices
                    .iter()
                    .any(|choice| choice.capability.format == *format)
        })
        .collect()
}

pub(crate) fn register(participant: Option<u64>, formats: Vec<Format>) -> Registration {
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    viewers().insert(id, Viewer { participant, formats });
    Registration(id)
}

/// The best format every participant other than `holder` can decode.
fn common_format(holder: Option<u64>) -> Option<Format> {
    let viewers = viewers();
    PREFERENCE.into_iter().find(|format| {
        viewers
            .values()
            .filter(|viewer| viewer.participant.is_none() || viewer.participant != holder)
            .all(|viewer| viewer.formats.contains(format))
    })
}

/// Replace `wanted` with the shared stream's settings when `participant`
/// watches it. Returns whether it does. The settings stored for the
/// participant stay as they are: taking control brings them back. In a
/// forwarding room (`forwarded`) the forwarder serves 流畅 (its low layer)
/// instead.
pub(crate) fn apply(participant: Option<u64>, wanted: &mut VideoConfig, forwarded: bool) -> bool {
    if !super::participants::shared_viewer(participant) {
        return false;
    }
    let tier = if wanted.quality == SMOOTH && !forwarded {
        SMOOTH
    } else {
        QUALITY
    };
    wanted.quality = tier;
    wanted.auto_quality = tier;
    wanted.fps = FPS;
    wanted.requested_fps = FPS;
    wanted.cinema = true;
    wanted.cursor_capture = true;
    wanted.color = Color::Sdr;
    wanted.requested_maximum = Some(MAXIMUM);
    if let Some(format) = common_format(super::participants::holder()) {
        wanted.format = format;
    } else if wanted.format.chroma != 1 {
        wanted.format.chroma = 1;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn av1_hevc_h264() -> Vec<Format> {
        PREFERENCE.to_vec()
    }

    #[test]
    fn the_shared_format_is_the_best_one_all_viewers_decode() {
        // Registrations are process-wide: use participant keys of our own.
        let a = register(Some(7101), av1_hevc_h264());
        let b = register(Some(7102), vec![PREFERENCE[3], PREFERENCE[4]]);
        let holder_only_h264 = register(Some(7103), vec![PREFERENCE[4]]);
        // 7101 and 7102 have HEVC 8-bit in common; the holder is left out.
        assert_eq!(common_format(Some(7103)), Some(PREFERENCE[3]));
        // Nobody holds control: everyone watches, H.264 is all they share.
        assert_eq!(common_format(None), Some(PREFERENCE[4]));
        drop(holder_only_h264);
        drop(b);
        drop(a);
    }
}
