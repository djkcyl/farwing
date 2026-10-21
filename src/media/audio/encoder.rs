//! Shared Opus sending configuration for microphone and desktop capture.
use anyhow::{Result, ensure};
use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate};
const RATE: u32 = 48_000;
// Leave room for RTP extensions/SRTP within both peers' receive MTU.
pub(crate) const MAX_PACKET: usize = 1200;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub(crate) struct Quality {
    pub kbps: u32,
}
impl Default for Quality {
    fn default() -> Self {
        Self { kbps: 128 }
    }
}
impl Quality {
    pub const RATES: [u32; 4] = [64, 128, 192, 256];

    pub fn validate(self) -> Result<()> {
        ensure!(Self::RATES.contains(&self.kbps), "{}", crate::i18n::t().session.audio_quality_required);
        Ok(())
    }

    pub fn restore(self) -> Result<Self> {
        ensure!((6..=320).contains(&self.kbps), "{}", crate::i18n::t().session.audio_quality_saved_invalid);
        Ok(Self {
            kbps: Self::RATES
                .into_iter()
                .min_by_key(|rate| rate.abs_diff(self.kbps))
                .unwrap_or(Self::default().kbps),
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Config {
    pub stereo: bool,
    pub fec: bool,
    pub dtx: bool,
    pub cbr: bool,
    pub bitrate: u32,
    pub bitrate_limit: u32,
    pub packet_ms: u32,
    pub max_playback_rate: u32,
}

impl Config {
    pub(crate) fn negotiated(fmtp: &str, ptime: Option<u32>) -> Result<Self> {
        let params: std::collections::HashMap<_, _> = fmtp
            .split(';')
            .filter_map(|v| v.trim().split_once('='))
            .map(|(k, v)| (k.trim(), v.trim()))
            .collect();
        let number = |key| params.get(key).and_then(|s| s.parse::<u32>().ok());
        let flag = |key| params.get(key).is_some_and(|s| *s == "1");
        let requested = ptime.or_else(|| number("ptime")).unwrap_or(20);
        // Round up to the next supported packet duration, capped at 120.
        let packet_ms = [10, 20, 40, 60, 120]
            .into_iter()
            .find(|v| *v >= requested)
            .unwrap_or(120);
        let bitrate_limit = number("maxaveragebitrate")
            .unwrap_or(510_000)
            .clamp(6_000, 510_000)
            .min(MAX_PACKET as u32 * 8000 / packet_ms);
        Ok(Self {
            stereo: flag("stereo"),
            fec: flag("useinbandfec"),
            dtx: flag("usedtx"),
            cbr: flag("cbr"),
            bitrate: (Quality::default().kbps * 1000).min(bitrate_limit),
            bitrate_limit,
            packet_ms,
            max_playback_rate: number("maxplaybackrate")
                .filter(|rate| (8_000..=RATE).contains(rate))
                .unwrap_or(RATE),
        })
    }
    pub fn quality(mut self, quality: Quality) -> Self {
        self.bitrate = (quality.kbps * 1000).min(self.bitrate_limit);
        self
    }
    pub fn same_stream(mut self, mut other: Self) -> bool {
        self.bitrate = 0;
        other.bitrate = 0;
        self.bitrate_limit = 0;
        other.bitrate_limit = 0;
        self == other
    }
}

pub(crate) fn create_encoder(config: Config) -> std::result::Result<Encoder, opusic_c::ErrorCode> {
    let mut e = Encoder::new(
        if config.stereo {
            Channels::Stereo
        } else {
            Channels::Mono
        },
        SampleRate::Hz48000,
        if config.stereo {
            Application::Audio
        } else {
            Application::Voip
        },
    )?;
    e.set_bitrate(Bitrate::Value(config.bitrate))?;
    e.set_complexity(9)?;
    e.set_vbr(!config.cbr)?;
    e.set_dtx(config.dtx)?;
    e.set_inband_fec(if config.fec {
        InbandFec::Mode1
    } else {
        InbandFec::Off
    })?;
    e.set_max_bandwidth(match config.max_playback_rate {
        0..=8000 => opusic_c::Bandwidth::Narrow,
        8001..=12000 => opusic_c::Bandwidth::Medium,
        12001..=16000 => opusic_c::Bandwidth::Wide,
        16001..=24000 => opusic_c::Bandwidth::Superwide,
        _ => opusic_c::Bandwidth::Full,
    })?;
    Ok(e)
}

/// Remote receive capability controls our encoder, for either offer or answer.
pub(crate) fn remote_config(
    description: &webrtc::peer_connection::sdp::session_description::RTCSessionDescription,
) -> Result<Option<Config>> {
    let sdp = description.unmarshal()?;
    let session_sendonly = sdp
        .attributes
        .iter()
        .any(|a| matches!(a.key.as_str(), "sendonly" | "inactive"));
    for media in sdp.media_descriptions {
        if media.media_name.media != "audio" || media.media_name.port.value == 0 {
            continue;
        }
        let direction = media.attributes.iter().find(|a| {
            matches!(
                a.key.as_str(),
                "sendonly" | "recvonly" | "sendrecv" | "inactive"
            )
        });
        if direction.map_or(session_sendonly, |a| {
            matches!(a.key.as_str(), "sendonly" | "inactive")
        }) {
            return Ok(None);
        }
        let pt = media
            .attributes
            .iter()
            .filter(|a| a.key == "rtpmap")
            .filter_map(|a| a.value.as_deref()?.split_once(' '))
            .find(|(pt, codec)| {
                codec.eq_ignore_ascii_case("opus/48000/2")
                    && media.media_name.formats.iter().any(|f| f == pt)
            })
            .map(|(pt, _)| pt);
        if let Some(pt) = pt {
            let fmtp = media
                .attributes
                .iter()
                .filter(|a| a.key == "fmtp")
                .filter_map(|a| a.value.as_deref()?.split_once(' '))
                .find(|(id, _)| *id == pt)
                .map_or("", |(_, v)| v);
            let ptime = media
                .attributes
                .iter()
                .find(|a| a.key == "ptime")
                .and_then(|a| a.value.as_ref()?.parse().ok());
            return Config::negotiated(fmtp, ptime).map(Some);
        }
        return Ok(None);
    }
    Ok(None)
}
