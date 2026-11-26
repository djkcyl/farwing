use cpal::traits::DeviceTrait;
use cpal::{BufferSize, SupportedBufferSize, SupportedStreamConfig};

/// PulseAudio's default playback target is about two seconds. Request a 20 ms
/// callback period instead: CPAL uses two periods for its end-to-end target
/// (40 ms), while NetEq already handles network jitter before playback. ALSA
/// keeps its default.
pub(super) fn playback_buffer(
    device: &cpal::Device,
    supported: &SupportedStreamConfig,
) -> BufferSize {
    if device
        .id()
        .is_ok_and(|id| id.host() == cpal::HostId::PulseAudio)
    {
        period(supported.sample_rate(), *supported.buffer_size())
    } else {
        BufferSize::Default
    }
}

fn period(rate: u32, supported: SupportedBufferSize) -> BufferSize {
    match supported {
        SupportedBufferSize::Range { min, max } => {
            BufferSize::Fixed((rate / 50).max(1).clamp(min, max))
        }
        SupportedBufferSize::Unknown => BufferSize::Default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pulse_playback_requests_twenty_ms_at_device_rate() {
        let supported = SupportedBufferSize::Range {
            min: 1,
            max: 48_000,
        };
        for (rate, frames) in [(48_000, 960), (44_100, 882), (96_000, 1_920)] {
            assert_eq!(period(rate, supported), BufferSize::Fixed(frames));
        }
    }

    #[test]
    fn pulse_playback_respects_device_buffer_limits() {
        assert_eq!(
            period(
                48_000,
                SupportedBufferSize::Range {
                    min: 1_024,
                    max: 4_096
                }
            ),
            BufferSize::Fixed(1_024),
        );
        assert_eq!(
            period(48_000, SupportedBufferSize::Range { min: 64, max: 512 }),
            BufferSize::Fixed(512),
        );
    }

    #[test]
    fn pulse_playback_preserves_default_when_limits_are_unknown() {
        assert_eq!(
            period(48_000, SupportedBufferSize::Unknown),
            BufferSize::Default
        );
    }
}
