// Connection statistics from `RTCPeerConnection.getStats()`: picture,
// rates, loss, delay, codec and the path (LAN, P2P or relay).
import { t } from '../i18n';

export interface ViewStats {
  width: number;
  height: number;
  fps: number;
  bitrate: number;
  /// Packets lost per packets expected since the last sample, 0..1.
  loss: number;
  rttMs: number;
  jitterMs: number;
  /// Average decode time per frame.
  decodeMs: number;
  framesDropped: number;
  codec: string;
  decoder: string;
  hardware: boolean | undefined;
  path: 'lan' | 'p2p' | 'relay' | 'forwarded' | '';
  protocol: string;
  local: string;
  remote: string;
  audioBitrate: number;
}

interface Sample {
  at: number;
  bytes: number;
  audioBytes: number;
  received: number;
  lost: number;
  decodeTime: number;
  decoded: number;
}

/// Turns successive reports into rates.
export class StatsSampler {
  private last?: Sample;

  /// `forwarded`: the forwarder connection's report while the picture
  /// comes through it (the video, its path, and the sound when forwarded).
  sample(report: RTCStatsReport, forwarded?: RTCStatsReport): ViewStats {
    const media = forwarded ?? report;
    const out: ViewStats = {
      width: 0,
      height: 0,
      fps: 0,
      bitrate: 0,
      loss: 0,
      rttMs: 0,
      jitterMs: 0,
      decodeMs: 0,
      framesDropped: 0,
      codec: '',
      decoder: '',
      hardware: undefined,
      path: '',
      protocol: '',
      local: '',
      remote: '',
      audioBitrate: 0,
    };
    const now: Sample = {
      at: performance.now(),
      bytes: 0,
      audioBytes: 0,
      received: 0,
      lost: 0,
      decodeTime: 0,
      decoded: 0,
    };
    let pairId: string | undefined;
    media.forEach((s) => {
      if (s.type === 'inbound-rtp' && s.kind === 'video' && (s.framesDecoded ?? 0) > 0) {
        out.width = s.frameWidth ?? 0;
        out.height = s.frameHeight ?? 0;
        out.fps = s.framesPerSecond ?? 0;
        out.jitterMs = (s.jitter ?? 0) * 1000;
        out.framesDropped = s.framesDropped ?? 0;
        out.decoder = s.decoderImplementation ?? '';
        out.hardware = s.powerEfficientDecoder;
        const codec = media.get(s.codecId);
        out.codec = codec?.mimeType?.replace(/^video\//, '') ?? '';
        now.bytes = s.bytesReceived ?? 0;
        now.received = s.packetsReceived ?? 0;
        now.lost = s.packetsLost ?? 0;
        now.decodeTime = s.totalDecodeTime ?? 0;
        now.decoded = s.framesDecoded ?? 0;
      }
      if (s.type === 'transport' && s.selectedCandidatePairId) pairId = s.selectedCandidatePairId;
    });
    // The sound comes through the forwarder too when it carries it.
    const audio = (r: RTCStatsReport) => {
      let bytes: number | undefined;
      r.forEach((s) => {
        if (s.type === 'inbound-rtp' && s.kind === 'audio') bytes = s.bytesReceived ?? 0;
      });
      return bytes;
    };
    now.audioBytes = (forwarded && audio(forwarded)) ?? audio(report) ?? 0;
    let pair: RTCIceCandidatePairStats | undefined;
    if (pairId) pair = media.get(pairId);
    if (!pair) {
      media.forEach((s) => {
        if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') pair = s;
      });
    }
    if (pair) {
      out.rttMs = (pair.currentRoundTripTime ?? 0) * 1000;
      const local = media.get(pair.localCandidateId);
      const remote = media.get(pair.remoteCandidateId);
      if (local && remote) {
        out.protocol = local.relayProtocol ?? local.protocol ?? '';
        out.local = `${local.address ?? local.ip ?? ''}:${local.port ?? ''}`;
        out.remote = `${remote.address ?? remote.ip ?? ''}:${remote.port ?? ''}`;
        out.path = forwarded
          ? 'forwarded'
          : local.candidateType === 'relay' || remote.candidateType === 'relay'
            ? 'relay'
            : local.candidateType === 'host' && remote.candidateType === 'host'
              ? 'lan'
              : 'p2p';
      }
    }
    const last = this.last;
    if (last && now.at > last.at) {
      const seconds = (now.at - last.at) / 1000;
      out.bitrate = Math.max(0, ((now.bytes - last.bytes) * 8) / seconds);
      out.audioBitrate = Math.max(0, ((now.audioBytes - last.audioBytes) * 8) / seconds);
      const received = now.received - last.received;
      const lost = now.lost - last.lost;
      out.loss = received + lost > 0 ? Math.max(0, lost) / (received + lost) : 0;
      const decoded = now.decoded - last.decoded;
      out.decodeMs = decoded > 0 ? ((now.decodeTime - last.decodeTime) / decoded) * 1000 : 0;
    }
    this.last = now;
    return out;
  }
}

/// How the media travels: LAN, P2P or relayed ('' before it is known).
export function pathText(path: string): string {
  return (t().viewer.stats.paths as Record<string, string>)[path] ?? '—';
}
