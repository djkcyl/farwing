//! What the forwarder reads from a session description: the published
//! tracks with their SSRCs, payload types and header extension IDs.

use std::collections::HashMap;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Kind {
    Audio,
    Video,
}

/// A track a sender announces (`a=ssrc:… msid:<stream> <track>`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Track {
    pub kind: Kind,
    /// Its media section's `a=mid` (empty if none).
    pub mid: String,
    pub stream_id: String,
    pub id: String,
    pub ssrc: u32,
    /// RTX and RS-FEC SSRCs (`a=ssrc-group:FID` / `FEC-FR-RS`); 0 if none.
    pub rtx: u32,
    pub fec: u32,
    /// First payload type of the media section (the preferred codec).
    pub payload_type: u8,
}

/// A payload type's codec (`a=rtpmap` and `a=fmtp`).
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Codec {
    /// Lower case, `video/h264`.
    pub mime: String,
    pub clock_rate: u32,
    pub fmtp: String,
}

impl Codec {
    fn parameter(&self, name: &str) -> Option<&str> {
        self.fmtp.split(';').find_map(|part| {
            let (key, value) = part.trim().split_once('=')?;
            (key.eq_ignore_ascii_case(name)).then_some(value.trim())
        })
    }

    /// The repaired payload type of an RTX codec.
    pub fn apt(&self) -> Option<u8> {
        (self.mime == "video/rtx")
            .then(|| self.parameter("apt")?.parse().ok())
            .flatten()
    }

    /// The same bitstream format, so packets of `self` decode as `other`.
    pub fn compatible(&self, other: &Codec) -> bool {
        if self.mime != other.mime || self.clock_rate != other.clock_rate {
            return false;
        }
        match self.mime.as_str() {
            "video/h264" => {
                let profile = |c: &Codec| {
                    c.parameter("profile-level-id")
                        .map(|p| p.get(..4).unwrap_or(p).to_ascii_lowercase())
                };
                let mode = |c: &Codec| c.parameter("packetization-mode").unwrap_or("0").to_owned();
                profile(self) == profile(other) && mode(self) == mode(other)
            }
            "video/av1" => {
                self.parameter("profile").unwrap_or("0") == other.parameter("profile").unwrap_or("0")
            }
            _ => true,
        }
    }
}

/// Payload types and header extensions of a description (one BUNDLE group:
/// both are session-wide).
#[derive(Clone, Debug, Default)]
pub struct Wire {
    pub codecs: HashMap<u8, Codec>,
    /// Extension ID → URI.
    pub extensions: HashMap<u8, String>,
}

impl Wire {
    pub fn extension_id(&self, uri: &str) -> Option<u8> {
        self.extensions
            .iter()
            .find_map(|(id, u)| (u == uri).then_some(*id))
    }
}

struct Section<'a> {
    kind: Option<Kind>,
    lines: Vec<&'a str>,
}

fn sections(sdp: &str) -> Vec<Section<'_>> {
    let mut out = Vec::new();
    for line in sdp.lines().map(str::trim_end) {
        if let Some(media) = line.strip_prefix("m=") {
            let kind = match media.split(' ').next() {
                Some("audio") => Some(Kind::Audio),
                Some("video") => Some(Kind::Video),
                _ => None,
            };
            out.push(Section {
                kind,
                lines: Vec::new(),
            });
        } else if let Some(section) = out.last_mut() {
            section.lines.push(line);
        }
    }
    out
}

/// Media sections per kind, in order (`m=audio`, `m=video`).
pub fn media_sections(sdp: &str) -> Vec<Kind> {
    sections(sdp).into_iter().filter_map(|s| s.kind).collect()
}

/// Tracks announced by the sender of `sdp`, in media-section order.
pub fn tracks(sdp: &str) -> Vec<Track> {
    let mut out = Vec::new();
    for section in sections(sdp) {
        let Some(kind) = section.kind else { continue };
        if section
            .lines
            .iter()
            .any(|l| *l == "a=recvonly" || *l == "a=inactive")
        {
            continue;
        }
        let payload_type = sdp_payload_types(&section).first().copied().unwrap_or(0);
        let mid = section
            .lines
            .iter()
            .find_map(|l| l.strip_prefix("a=mid:"))
            .unwrap_or_default()
            .to_owned();
        let mut msids: Vec<(u32, String, String)> = Vec::new();
        let mut rtx = HashMap::new();
        let mut fec = HashMap::new();
        for line in &section.lines {
            if let Some(rest) = line.strip_prefix("a=ssrc:") {
                let mut parts = rest.splitn(2, ' ');
                let (Some(ssrc), Some(attribute)) = (parts.next(), parts.next()) else {
                    continue;
                };
                let (Ok(ssrc), Some(msid)) = (ssrc.parse::<u32>(), attribute.strip_prefix("msid:"))
                else {
                    continue;
                };
                let mut ids = msid.split_whitespace();
                if let (Some(stream), Some(track)) = (ids.next(), ids.next())
                    && !msids.iter().any(|(s, _, _)| *s == ssrc)
                {
                    msids.push((ssrc, stream.to_owned(), track.to_owned()));
                }
            } else if let Some(rest) = line.strip_prefix("a=ssrc-group:") {
                let mut parts = rest.split_whitespace();
                let semantics = parts.next();
                let ssrcs: Vec<u32> = parts.filter_map(|s| s.parse().ok()).collect();
                if let [primary, repair] = ssrcs[..] {
                    match semantics {
                        Some("FID") => {
                            rtx.insert(primary, repair);
                        }
                        Some("FEC-FR-RS") => {
                            fec.insert(primary, repair);
                        }
                        _ => {}
                    }
                }
            }
        }
        let repairs: Vec<u32> = rtx.values().chain(fec.values()).copied().collect();
        // Browsers may name the track only at the media level (`a=msid:`)
        // and give its SSRCs a CNAME alone.
        if msids.is_empty()
            && let Some((stream, track)) = section.lines.iter().find_map(|l| {
                let mut ids = l.strip_prefix("a=msid:")?.split_whitespace();
                Some((ids.next()?.to_owned(), ids.next()?.to_owned()))
            })
            && let Some(ssrc) = section.lines.iter().find_map(|l| {
                let ssrc: u32 = l.strip_prefix("a=ssrc:")?.split(' ').next()?.parse().ok()?;
                (!repairs.contains(&ssrc)).then_some(ssrc)
            })
        {
            msids.push((ssrc, stream, track));
        }
        for (ssrc, stream_id, id) in msids {
            if repairs.contains(&ssrc) {
                continue;
            }
            out.push(Track {
                kind,
                mid: mid.clone(),
                stream_id,
                id,
                ssrc,
                rtx: rtx.get(&ssrc).copied().unwrap_or(0),
                fec: fec.get(&ssrc).copied().unwrap_or(0),
                payload_type,
            });
        }
    }
    out
}

fn sdp_payload_types(section: &Section<'_>) -> Vec<u8> {
    // The m= line itself is not kept; take rtpmap order instead.
    section
        .lines
        .iter()
        .filter_map(|l| l.strip_prefix("a=rtpmap:"))
        .filter_map(|r| r.split(' ').next()?.parse().ok())
        .collect()
}

/// Each media section's first payload type by its `a=mid` (an answer: the
/// codec the sender will use).
pub fn first_payload_types(sdp: &str) -> HashMap<String, u8> {
    sections(sdp)
        .iter()
        .filter_map(|s| {
            let mid = s.lines.iter().find_map(|l| l.strip_prefix("a=mid:"))?;
            Some((mid.to_owned(), *sdp_payload_types(s).first()?))
        })
        .collect()
}

/// Codecs and header extensions of `sdp`.
pub fn wire(sdp: &str) -> Wire {
    let mut wire = Wire::default();
    let mut fmtp: HashMap<u8, String> = HashMap::new();
    for line in sdp.lines().map(str::trim_end) {
        if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            let Some((pt, encoding)) = rest.split_once(' ') else { continue };
            let Ok(pt) = pt.parse::<u8>() else { continue };
            let mut parts = encoding.split('/');
            let name = parts.next().unwrap_or_default();
            let clock_rate = parts.next().and_then(|c| c.parse().ok()).unwrap_or(0);
            let kind = if clock_rate == 48_000 && name.eq_ignore_ascii_case("opus") {
                "audio"
            } else {
                "video"
            };
            wire.codecs.entry(pt).or_insert(Codec {
                mime: format!("{kind}/{}", name.to_ascii_lowercase()),
                clock_rate,
                fmtp: String::new(),
            });
        } else if let Some(rest) = line.strip_prefix("a=fmtp:") {
            let Some((pt, params)) = rest.split_once(' ') else { continue };
            if let Ok(pt) = pt.parse::<u8>() {
                fmtp.insert(pt, params.trim().to_owned());
            }
        } else if let Some(rest) = line.strip_prefix("a=extmap:") {
            let Some((id, uri)) = rest.split_once(' ') else { continue };
            let id = id.split('/').next().unwrap_or(id);
            if let Ok(id) = id.parse::<u8>() {
                wire.extensions
                    .insert(id, uri.split_whitespace().next().unwrap_or(uri).to_owned());
            }
        }
    }
    for (pt, params) in fmtp {
        if let Some(codec) = wire.codecs.get_mut(&pt) {
            codec.fmtp = params;
        }
    }
    wire
}

#[cfg(test)]
mod tests {
    use super::*;

    const OFFER: &str = "v=0\r\n\
o=- 1 2 IN IP4 0.0.0.0\r\n\
s=-\r\n\
t=0 0\r\n\
a=group:BUNDLE 0 1\r\n\
m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n\
a=mid:0\r\n\
a=extmap:1 http://www.ietf.org/id/draft-holmer-rmcat-transport-wide-cc-extensions-01\r\n\
a=sendonly\r\n\
a=rtpmap:111 opus/48000/2\r\n\
a=fmtp:111 minptime=10;stereo=1;useinbandfec=1\r\n\
a=ssrc:11 cname:host\r\n\
a=ssrc:11 msid:host audio_0\r\n\
m=video 9 UDP/TLS/RTP/SAVPF 98 99 36\r\n\
a=mid:1\r\n\
a=extmap:3 http://www.webrtc.org/experiments/rtp-hdrext/video-timing\r\n\
a=sendonly\r\n\
a=rtpmap:98 H264/90000\r\n\
a=fmtp:98 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n\
a=rtpmap:99 rtx/90000\r\n\
a=fmtp:99 apt=98\r\n\
a=rtpmap:36 x-farwing-rsfec/90000\r\n\
a=ssrc-group:FID 21 22\r\n\
a=ssrc-group:FEC-FR-RS 21 23\r\n\
a=ssrc:21 msid:host video_0\r\n\
a=ssrc:22 msid:host video_0\r\n\
a=ssrc:23 msid:host video_0\r\n";

    #[test]
    fn tracks_and_repair_streams() {
        let tracks = tracks(OFFER);
        assert_eq!(tracks.len(), 2);
        assert_eq!(tracks[0].id, "audio_0");
        assert_eq!((tracks[0].ssrc, tracks[0].rtx), (11, 0));
        assert_eq!(tracks[1].kind, Kind::Video);
        assert_eq!((tracks[1].ssrc, tracks[1].rtx, tracks[1].fec), (21, 22, 23));
        assert_eq!(tracks[1].payload_type, 98);
    }

    #[test]
    fn codecs_and_extensions() {
        let wire = wire(OFFER);
        assert_eq!(wire.codecs[&99].apt(), Some(98));
        assert_eq!(wire.codecs[&98].mime, "video/h264");
        assert_eq!(wire.codecs[&111].mime, "audio/opus");
        assert_eq!(
            wire.extension_id("http://www.webrtc.org/experiments/rtp-hdrext/video-timing"),
            Some(3)
        );
        let browser = Codec {
            mime: "video/h264".into(),
            clock_rate: 90_000,
            fmtp: "profile-level-id=42e01f;packetization-mode=1".into(),
        };
        assert!(wire.codecs[&98].compatible(&browser));
    }
}
