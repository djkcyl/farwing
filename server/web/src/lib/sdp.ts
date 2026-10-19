// SDP handling of the viewer (`docs/protocol/web-viewer-brief.md` §2.9, §3):
// DTLS identity, session attributes, Opus stereo, codec choice, track map.
import { utf8 } from './encoding';

// SDP helpers ----------------------------------------------------------------

/// All DTLS fingerprints of an SDP, lower case, sorted and de-duplicated
/// (`src/transport/signal/identity.rs`).
export function fingerprints(sdp: string): string[] {
  const set = new Set<string>();
  for (const raw of sdp.split('\n')) {
    const line = raw.replace(/\s+$/, '');
    if (line.startsWith('a=fingerprint:')) set.add(line.slice(14).trim().toLowerCase());
  }
  return [...set].sort();
}

export function dtlsMessage(sid: string, attempt: string, role: 'controller' | 'host', prints: string[]): Uint8Array {
  return utf8(`FWDTLS2\n${sid}\n${attempt}\n${role}\n${prints.join(',')}`);
}

/// Insert a session-level attribute before the first media section.
export function insertSessionAttribute(sdp: string, line: string): string {
  const eol = sdp.includes('\r\n') ? '\r\n' : '\n';
  const at = sdp.search(/^m=/m);
  return at < 0 ? sdp + line + eol : sdp.slice(0, at) + line + eol + sdp.slice(at);
}

export function sessionAttribute(sdp: string, name: string): string | undefined {
  for (const raw of sdp.split('\n')) {
    const line = raw.trim();
    if (line.startsWith(`a=${name}:`)) return line.slice(name.length + 3).trim();
  }
  return undefined;
}

/// Opus in stereo: the host encodes stereo music only when the offer asks
/// for it, and the browser decodes as its local description says (§3.4).
export function opusStereo(sdp: string): string {
  const pt = /^a=rtpmap:(\d+) opus\/48000\/2/im.exec(sdp)?.[1];
  if (!pt) return sdp;
  return sdp.replace(new RegExp(`^(a=fmtp:${pt} [^\\r\\n]*)`, 'm'), (line) =>
    /stereo=1/.test(line) ? line : `${line};stereo=1;sprop-stereo=1`,
  );
}

/// The audio section declares its SSRC (the host's receiver needs it).
export function audioHasSsrc(sdp: string): boolean {
  const section = sdp.split(/^m=/m).find((m) => m.startsWith('audio'));
  return !!section && /^a=ssrc:\d+/m.test(section);
}

/// mid → video track index (`video_N`) from the answer's msid lines.
export function videoTracks(sdp: string): Map<string, number> {
  const map = new Map<string, number>();
  for (const section of sdp.split(/^m=/m).slice(1)) {
    if (!section.startsWith('video')) continue;
    const mid = /^a=mid:(\S+)/m.exec(section)?.[1];
    const track = /^a=msid:\S+ video_(\d+)/m.exec(section)?.[1] ?? /msid:video_(\d+)/m.exec(section)?.[1];
    if (mid !== undefined && track !== undefined) map.set(mid, Number(track));
  }
  return map;
}

// Video codecs ----------------------------------------------------------------

/// Codec numbers of `session.start` capabilities and capture settings.
export const VideoCodec = { H264: 1, H265: 2, AV1: 5 } as const;
export type VideoCodecId = (typeof VideoCodec)[keyof typeof VideoCodec];

/// Receive codecs in preference order, limited to the formats the host
/// registers (§3.3): AV1 profile 0, H.265 Main (profile-id 1), H.264
/// constrained baseline with packetization-mode 1, and RTX.
function receiveCodecs() {
  const codecs = RTCRtpReceiver.getCapabilities?.('video')?.codecs ?? [];
  const mime = (c: RTCRtpCodec) => c.mimeType.toLowerCase();
  const fmtp = (c: RTCRtpCodec) => c.sdpFmtpLine ?? '';
  return {
    av1: codecs.filter((c) => mime(c) === 'video/av1' && !/(^|;)\s*profile=[^0]/.test(fmtp(c))),
    h265: codecs.filter((c) => mime(c) === 'video/h265' && /(^|;)\s*profile-id=1(;|$)/.test(fmtp(c))),
    h264: codecs.filter(
      (c) =>
        mime(c) === 'video/h264' && /profile-level-id=42e01f/i.test(fmtp(c)) && /packetization-mode=1/.test(fmtp(c)),
    ),
    rtx: codecs.filter((c) => mime(c) === 'video/rtx'),
  };
}

/// What `setCodecPreferences` gets on every video transceiver, or undefined
/// to leave the browser's defaults.
export function videoCodecPreferences(): RTCRtpCodec[] | undefined {
  const c = receiveCodecs();
  const media = [...c.av1, ...c.h265, ...c.h264];
  return media.length ? [...media, ...c.rtx] : undefined;
}

export interface DecodeSupport {
  codec: VideoCodecId;
  /// Hardware (power-efficient) decoding; software decoding is reported as
  /// such so the host prefers another codec when it can.
  hardware: boolean;
}

const CONTENT_TYPES: Record<VideoCodecId, string> = {
  [VideoCodec.H264]: 'video/H264',
  [VideoCodec.H265]: 'video/H265',
  [VideoCodec.AV1]: 'video/AV1',
};

/// The codecs this browser receives over WebRTC, best first.
export async function decodeSupport(): Promise<DecodeSupport[]> {
  const c = receiveCodecs();
  const present: VideoCodecId[] = [];
  if (c.av1.length) present.push(VideoCodec.AV1);
  if (c.h265.length) present.push(VideoCodec.H265);
  if (c.h264.length || !present.length) present.push(VideoCodec.H264);
  const out: DecodeSupport[] = [];
  for (const codec of present) {
    let hardware = codec === VideoCodec.H264;
    try {
      const info = await navigator.mediaCapabilities?.decodingInfo({
        type: 'webrtc',
        video: { contentType: CONTENT_TYPES[codec], width: 3840, height: 2160, bitrate: 30_000_000, framerate: 60 },
      });
      if (info && !info.supported && codec !== VideoCodec.H264) continue;
      if (info) hardware = info.powerEfficient;
    } catch {
      // No WebRTC media capabilities: trust getCapabilities().
    }
    out.push({ codec, hardware });
  }
  return out;
}

/// The host matches fmtp parameters the two sides share exactly, and its AV1
/// registration says level-idx=17 where browsers say 5: leave the level out
/// of the offer it sees. Only the signed wire copy changes, not the local
/// description.
export function withoutAv1Level(sdp: string): string {
  const pts = [...sdp.matchAll(/^a=rtpmap:(\d+) AV1\/90000/gim)].map((m) => m[1]);
  let out = sdp;
  for (const pt of new Set(pts)) {
    out = out.replace(new RegExp(`^a=fmtp:${pt} ([^\\r\\n]*)(\\r?\\n)`, 'gm'), (_line, params: string, eol: string) => {
      const kept = params.split(';').filter((p) => !/^\s*level-idx=/.test(p));
      return kept.length ? `a=fmtp:${pt} ${kept.join(';')}${eol}` : '';
    });
  }
  return out;
}

// Dependency Descriptor ---------------------------------------------------------

/// With the Dependency Descriptor (AV1 RTP specification, appendix A)
/// libwebrtc decides from declared dependencies whether a frame can decode,
/// so after a loss the host's recovery frame decodes without a keyframe
/// (`docs/protocol/rfi-ltr-brief.md` §4.1). Chrome does not offer it unless
/// asked.
export const DEPENDENCY_DESCRIPTOR =
  'https://aomediacodec.github.io/av1-rtp-spec/#dependency-descriptor-rtp-header-extension';

type HeaderExtensionControl = RTCRtpTransceiver & {
  getHeaderExtensionsToNegotiate?: () => { uri: string; direction: RTCRtpTransceiverDirection }[];
  setHeaderExtensionsToNegotiate?: (extensions: { uri: string; direction: RTCRtpTransceiverDirection }[]) => void;
};

/// Browsers where receiving with the descriptor was verified: Chromium
/// (Chrome, Edge). Others keep sequence-based decoding.
function verifiedBrowser(): boolean {
  const data = (navigator as Navigator & { userAgentData?: { brands?: { brand: string }[] } }).userAgentData;
  return !!data?.brands?.some((b) => b.brand === 'Chromium');
}

/// Ask for the descriptor on a receiving video transceiver, in a verified
/// browser that lets the page choose its header extensions; false otherwise.
export function offerDependencyDescriptor(transceiver: RTCRtpTransceiver): boolean {
  const tx = transceiver as HeaderExtensionControl;
  if (!verifiedBrowser()) return false;
  if (!tx.getHeaderExtensionsToNegotiate || !tx.setHeaderExtensionsToNegotiate) return false;
  try {
    const extensions = tx.getHeaderExtensionsToNegotiate();
    const descriptor = extensions.find((e) => e.uri === DEPENDENCY_DESCRIPTOR);
    if (!descriptor) return false;
    if (descriptor.direction === 'stopped') {
      descriptor.direction = 'recvonly';
      tx.setHeaderExtensionsToNegotiate(extensions);
    }
    return true;
  } catch {
    return false;
  }
}

/// Whether the answer accepted the descriptor (some video section has it).
export function hasDependencyDescriptor(sdp: string): boolean {
  return sdp.includes(DEPENDENCY_DESCRIPTOR);
}
