// One desktop session in the browser (`docs/protocol/web-viewer-brief.md`,
// `web-control-brief.md`): admission (account session or visitor) →
// realtime connection → `session.start` → signed WebRTC offer → verified
// answer → HELLO both ways on the control channel → video_0 and audio_0
// play. Within the granted capabilities (the server limits browsers to its
// web setting): the control token and input, the text clipboard, files and
// the microphone.
import { t } from '../i18n';
import { Annotation, type AnnotationState } from './annotation';
import { call, errorText } from './api';
import { ClipboardSync } from './clipboard';
import { deviceKey, verify } from './device';
import { deviceIdOf, unb64 } from './encoding';
import { FileTransfer, type TransferInfo } from './files';
import { ForwardReceiver } from './forward';
import * as fpp from './fpp';
import { InputController } from './input';
import { PeerRequestError, type PeerIo } from './peerio';
import { Realtime, RealtimeError, reconnectable, type Envelope } from './realtime';
import {
  audioHasSsrc,
  decodeSupport,
  dtlsMessage,
  fingerprints,
  hasDependencyDescriptor,
  insertSessionAttribute,
  offerDependencyDescriptor,
  opusStereo,
  sessionAttribute,
  VideoCodec,
  videoCodecPreferences,
  videoTracks,
  withoutAv1Level,
  type DecodeSupport,
  type VideoCodecId,
} from './sdp';
import { pakeBinding } from './spake2';

export interface SessionTicket {
  session_id: string;
  ticket: string;
  endpoints: string[];
  connect_timeout_ms: number;
  retry_delay_ms: number;
  target: { device_id: string; public_key?: string; alias?: string; granted?: string[] };
  bitrate_limit_mbps?: number | null;
}

interface IceServer {
  urls: string[];
  username: string;
  credential: string;
}

export interface SessionStarted {
  participant_id: string;
  session_id: string;
  ice_servers: IceServer[];
  force_relay: boolean;
  host: { device_id: string; public_key: string };
  relation: string;
  grants: string[];
}

interface RtcDescription {
  attempt_id: string;
  restart: boolean;
  network: number;
  sdp: string;
}

export type ViewerState = 'connecting' | 'negotiating' | 'waiting_media' | 'playing' | 'reconnecting' | 'ended';

/// The host's answer, then the connection from it.
const ANSWER_TIMEOUT_MS = 40_000;
const CONNECT_TIMEOUT_MS = 15_000;

/// How a session ended.
export interface Ending {
  /// `session.ended` reason, or `network`, `identity`, `incompatible`.
  reason: string;
  /// Why, in the current language (called when shown).
  message(): string;
  /// A new session may succeed (the page tries again).
  recoverable: boolean;
}

export interface ViewerEvents {
  state?(state: ViewerState): void;
  ended?(ending: Ending): void;
  stream?(stream: MediaStream): void;
  screens?(screens: fpp.Screens): void;
  quality?(quality: fpp.Quality): void;
  permissions?(grants: string[]): void;
  participants?(value: { participants: fpp.Participant[]; controlling: string }): void;
  cursor?(shape: fpp.CursorShape): void;
  pointers?(pointers: fpp.ParticipantPointer[]): void;
  hostEvent?(event: fpp.HostEvent): void;
  audio?(state: fpp.AudioState): void;
  captureResult?(errors: number[]): void;
  /// Codecs both sides support (the host's encoders ∩ this browser's
  /// decoders), and the host's own preferred codec if it set one.
  codecs?(codecs: VideoCodecId[], hostPreferred?: VideoCodecId): void;
  /// The control token: held, being requested, or why it is not held.
  control?(state: ControlState): void;
  microphone?(state: MicrophoneStatus): void;
  /// Text the host copied (automatic clipboard synchronization).
  clipboardText?(text: string): void;
  transfer?(info: TransferInfo): void;
  annotation?(state: AnnotationState): void;
  /// The host's HELLO features (what it can receive: `touch`, `pen`, …).
  hostFeatures?(features: Record<string, number>): void;
  /// The picture comes through the server's forwarder in this mode
  /// (`e2ee`, `compatible`); '' when it comes from the host.
  forwarding?(mode: string): void;
  log?(line: string): void;
}

export interface ControlState {
  held: boolean;
  pending: boolean;
  /// Why the token was refused or lost.
  message?: string;
}

export interface MicrophoneStatus {
  /// off; starting (asking the host); waiting (no program on the host uses
  /// the virtual microphone yet); on (sending).
  state: 'off' | 'starting' | 'waiting' | 'on';
  message?: string;
}

/// Bulk sends wait while more than this is queued.
const BULK_HIGH = 4 * 1024 * 1024;

/// The automatic-quality tier the host last reported, per host: the next
/// session starts there (as the native viewer does).
const AUTO_TIER_KEY = 'farwing.viewer.auto.';

const HELLO_VIDEO: Record<VideoCodecId, string> = {
  [VideoCodec.H264]: 'video.h264',
  [VideoCodec.H265]: 'video.hevc',
  [VideoCodec.AV1]: 'video.av1',
};

/// Why the host kept control, from its code (`control_turn.rs` `refusal`).
function refusalText(code: number, holder: string): string {
  const m = t().viewer.control;
  switch (code) {
    case 1:
      return m.notAvailable;
    case 2:
      return m.noPermission;
    case 3:
      return m.otherControlling(holder || m.otherParticipant);
    case 4:
      return m.notAutomaticShared;
    default:
      return m.refused;
  }
}

/// Why a session ended, in words for the page (`realtime.rs` `EndReason`);
/// '' when the connection to the server was lost.
export function endText(reason: string): string {
  const m = t().viewer.ended;
  const known = (m.reasons as Record<string, string>)[reason];
  return known ?? (reason ? m.other(reason) : m.serverLost);
}

/// A failure to start a session, in the current language (errors with a
/// `text()`, such as `RealtimeError` and `VisitError`, put it in words again).
export function failureText(error: unknown): string {
  const text = (error as { text?: unknown } | null | undefined)?.text;
  return typeof text === 'function' ? String(text.call(error)) : errorText(error);
}

/// Sessions that ended for one of these reasons are not re-created.
const FINAL_REASONS = new Set([
  'kicked',
  'revoked',
  'permission_revoked',
  'identity_mismatch',
  'controller_left',
  'replaced',
  'identity',
  'incompatible',
]);

const identityText = () => t().viewer.ended.reasons.identity_mismatch;

/// A session this browser may join: an account session or a visitor
/// admission (`docs/web-console.md` §7.2, §7.3).
export interface Admission {
  ticket: SessionTicket;
  /// The host's device ID, checked against the key the server states.
  hostId: string;
  /// Visitors admitted with an access code bind the DTLS fingerprints to
  /// the PAKE key as well (`a=x-farwing-pake`).
  pakeKey?: Uint8Array;
  /// A visitor's guest token for later requests (none when signed in).
  bearer?: string;
}

function iceServers(started: SessionStarted): RTCIceServer[] {
  return started.ice_servers.map((s) =>
    s.username ? { urls: s.urls, username: s.username, credential: s.credential } : { urls: s.urls },
  );
}

/// An account session on an own or shared device.
export async function accountAdmission(target: string): Promise<Admission> {
  const ticket = await call<SessionTicket>('POST', '/api/v2/sessions', {
    target_device_id: target,
    purpose: 'desktop',
  });
  return { ticket, hostId: target };
}

interface HostCapability {
  preferred_codec?: number;
  video_codec_capability?: { video_codec: number; chroma_sampling: number; bit_depth: number }[];
}

export interface ViewerOptions {
  /// Obtain the session to join.
  admit(): Promise<Admission>;
  /// The video codec chosen in the menu (0: the host chooses), asked for
  /// from the start of each session.
  codec?(): number;
  /// Physical screen size for a host without displays (virtual display).
  screen?: { width: number; height: number };
}

export class Viewer {
  private options: ViewerOptions;
  private events: ViewerEvents;
  private rt?: Realtime;
  private pc?: RTCPeerConnection;
  private channels = new Map<string, RTCDataChannel>();
  private sid = '';
  private attempt = '';
  private admission?: Admission;
  private started?: SessionStarted;
  private gen = 0;
  private remoteSet = false;
  private pendingCandidates: RTCIceCandidateInit[] = [];
  private tracks = new Map<string, number>();
  private helloTimer = 0;
  private helloAnswered = false;
  private hostHello?: fpp.Hello;
  private negotiateTimer = 0;
  private stream = new MediaStream();
  /// The host's own video and sound, played again when forwarding ends.
  private directVideo?: MediaStreamTrack;
  private directAudio?: MediaStreamTrack;
  private forwardedAudio = false;
  private forward?: ForwardReceiver;
  private stopped = false;
  private iceRestarts = 0;
  private disconnectedTimer = 0;
  private pointerSeq = 0;
  private sessionRequest = 1;
  private decoders: DecodeSupport[] = [];
  /// The picture quality asked for (5: automatic).
  private frameQuality = 5;
  /// The host's capability, reported once its session channel works (the
  /// first screen list), since capture settings sent earlier are lost.
  private hostCapability?: HostCapability;
  private screensSeen = false;
  private audioTx?: RTCRtpTransceiver;
  private ssrcChecked = false;
  /// The Dependency Descriptor: asked for through the transceivers (verified
  /// browsers only), accepted by the answer. With it this viewer
  /// acknowledges decoded frames and the host can send recovery frames.
  private descriptorAccepted = false;
  private ackTimer = 0;
  private acked = new Map<number, number>();
  private requests = new Map<string, { resolve(body: Uint8Array): void; reject(error: Error): void; timer: number }>();
  private clipboardParts = new fpp.Reassembly(48 * 1024 * 1024);
  private fileParts = new fpp.Reassembly(32 * 1024 * 1024);
  private myId = '';
  private controlHeld = false;
  private turnPending = false;
  /// Keep the token across reconnections.
  private wantControl = false;
  private mic?: { stream: MediaStream; ok: boolean; active: boolean };
  state: ViewerState = 'connecting';
  grants: string[] = [];
  readonly input: InputController;
  readonly clipboard: ClipboardSync;
  readonly files: FileTransfer;
  readonly annotation: Annotation;

  constructor(options: ViewerOptions, events: ViewerEvents) {
    this.options = options;
    this.events = events;
    const io: PeerIo = {
      request: (type, body, timeoutMs) => this.request(type, body, timeoutMs),
      push: (type, body) => this.sendSession(type, body),
      reply: (type, rid, body) => this.sendSession(type, body, fpp.Flag.RESPONSE, rid),
      replyError: (type, rid, code, message) =>
        this.sendSession(type, fpp.errorBody(code, message), fpp.Flag.RESPONSE | fpp.Flag.ERROR, rid),
      bulk: (type, body) => this.sendBulk(type, body),
    };
    this.input = new InputController({
      send: (body) => this.sendFrame('farwing.control.v2', fpp.encode(fpp.Type.INPUT, body)),
      sendFrame: (type, body) => this.sendFrame('farwing.control.v2', fpp.encode(type, body)),
      buffered: () => this.channels.get('farwing.control.v2')?.bufferedAmount ?? 0,
    });
    this.clipboard = new ClipboardSync(io, {
      remoteText: (text) => this.events.clipboardText?.(text),
      log: (line) => this.log(line),
    });
    this.files = new FileTransfer(io, {
      transfer: (info) => this.events.transfer?.(info),
      log: (line) => this.log(line),
    });
    this.annotation = new Annotation(
      (frame) => this.sendFrame('farwing.session.v2', frame),
      (state) => this.events.annotation?.(state),
    );
  }

  private log(line: string) {
    this.events.log?.(line);
  }

  private setState(state: ViewerState) {
    this.state = state;
    this.events.state?.(state);
  }

  /// Open the session; resolves once the offer is out (or throws).
  async start(): Promise<void> {
    this.setState('connecting');
    const admission = await this.options.admit();
    if (this.stopped) return;
    this.admission = admission;
    this.log(`session ${admission.ticket.session_id}`);
    this.rt = await Realtime.connect(admission.ticket.ticket, {
      message: (envelope) => this.onMessage(envelope),
      closed: (code, reason) => void this.onClosed(code, reason),
    });
    if (this.stopped) {
      this.rt.bye();
      return;
    }
    this.sid = this.rt.hello.session_id;
    await this.sessionStart();
  }

  private async sessionStart() {
    this.attempt = crypto.randomUUID();
    const width = Math.round((this.options.screen?.width ?? screen.width) * devicePixelRatio);
    const height = Math.round((this.options.screen?.height ?? screen.height) * devicePixelRatio);
    if (!this.decoders.length) this.decoders = await decodeSupport();
    this.log(`decoders ${this.decoders.map((d) => `${d.codec}${d.hardware ? '' : '(sw)'}`).join(',')}`);
    const started = await this.rt!.request<SessionStarted>(
      'session.start',
      {
        attempt_id: this.attempt,
        purpose: 'desktop',
        options: {
          screen_id: -1,
          capture: {
            fps: 2,
            quality: 5,
            cursor_capture: true,
            // Follow the host's resolution: the web never changes displays.
            resolution_type: 3,
            local_resolution: { width, height },
            chosen_resolution: null,
            chroma: 1,
            bitrate: 0,
            hdr: false,
            auto_quality: this.autoTier(),
            fps_count: 60,
            weak_network: 0,
            cinema: false,
            codec: this.options.codec?.() ?? 0,
          },
          decoders: this.decoders.map((d) => ({ fps: 60, codec: d.codec, width: 3840, height: 2160, chroma: 1 })),
          force_virtual: false,
          virtual_modes: [],
          virtual_initial: null,
          version: 'web-0.1.0',
        },
        capability: {
          sdr_10bit: false,
          session_id: '',
          display_info: [],
          // codec_impl 32 is hardware decoding, 37 software (§2.4).
          video_codec_capability: this.decoders.map((d) => ({
            video_codec: d.codec,
            width: 3840,
            height: 2160,
            chroma_sampling: 1,
            bit_depth: 8,
            codec_impl: d.hardware ? 32 : 37,
          })),
        },
      },
      { sid: this.sid },
    );
    // The host's key must be the one its device ID was derived from.
    const hostKey = unb64(started.host.public_key);
    if (
      hostKey.length !== 32 ||
      started.host.device_id !== this.admission!.hostId ||
      (await deviceIdOf(hostKey)) !== started.host.device_id
    ) {
      this.finish({ reason: 'identity', message: identityText, recoverable: false });
      throw new RealtimeError('identity_mismatch');
    }
    this.started = started;
    this.grants = started.grants;
    this.events.permissions?.(this.grants);
    this.log(`started relation=${started.relation} grants=${started.grants.join(',')}`);
    await this.negotiate(false);
  }

  private createPeer() {
    const started = this.started!;
    const pc = new RTCPeerConnection({
      iceServers: iceServers(started),
      iceTransportPolicy: started.force_relay ? 'relay' : 'all',
      bundlePolicy: 'max-bundle',
      rtcpMuxPolicy: 'require',
    });
    this.pc = pc;
    // sendrecv from the first offer: the microphone needs no renegotiation.
    this.audioTx = pc.addTransceiver('audio', { direction: 'sendrecv' });
    const codecs = videoCodecPreferences();
    for (let i = 0; i < 5; i++) {
      const tx = pc.addTransceiver('video', { direction: 'recvonly' });
      if (codecs) tx.setCodecPreferences(codecs);
      offerDependencyDescriptor(tx);
    }
    for (const label of fpp.CHANNELS) {
      const dc = pc.createDataChannel(
        label,
        label === 'farwing.dgram.v2' ? { ordered: false, maxRetransmits: 0 } : { ordered: true },
      );
      dc.binaryType = 'arraybuffer';
      if (label === 'farwing.bulk.v2') dc.bufferedAmountLowThreshold = 1024 * 1024;
      this.channels.set(label, dc);
      dc.onmessage = (event) => {
        if (event.data instanceof ArrayBuffer) this.onFrame(label, event.data);
      };
      if (label === 'farwing.control.v2') dc.onopen = () => this.sendHello();
    }
    pc.onicecandidate = (event) => {
      const c = event.candidate;
      if (!c || !c.candidate) return;
      const json = c.toJSON();
      this.rt?.send(
        'rtc.candidate',
        {
          attempt_id: this.attempt,
          candidate: {
            candidate: json.candidate,
            sdpMid: json.sdpMid || '0',
            sdpMLineIndex: json.sdpMLineIndex ?? undefined,
            usernameFragment: json.usernameFragment ?? undefined,
          },
        },
        { sid: this.sid, gen: this.gen },
      );
    };
    pc.ontrack = (event) => this.onTrack(event);
    pc.onconnectionstatechange = () => this.onConnectionState();
  }

  /// Create, sign and send an offer (a fresh one or an ICE restart).
  private async negotiate(restart: boolean) {
    if (!this.pc) this.createPeer();
    const pc = this.pc!;
    if (!restart) this.setState('negotiating');
    this.gen += 1;
    this.remoteSet = false;
    if (restart) pc.restartIce();
    let offer = await pc.createOffer();
    // The host receives the microphone only from an audio section that
    // declares its SSRC (control brief §5.3): without one, offer with a
    // silent track once.
    let silent: AudioContext | undefined;
    if (!this.ssrcChecked) {
      this.ssrcChecked = true;
      if (!audioHasSsrc(offer.sdp ?? '') && this.audioTx) {
        silent = new AudioContext();
        const track = silent.createMediaStreamDestination().stream.getAudioTracks()[0];
        await this.audioTx.sender.replaceTrack(track);
        offer = await pc.createOffer();
        this.log(`audio ssrc via silent track: ${audioHasSsrc(offer.sdp ?? '')}`);
      }
    }
    const local = opusStereo(offer.sdp ?? '');
    await pc.setLocalDescription({ type: 'offer', sdp: local });
    if (silent) {
      await this.audioTx?.sender.replaceTrack(null);
      void silent.close();
    }
    let wire = pc.localDescription!.sdp;
    wire = withoutAv1Level(wire.replace(/^a=x-farwing-fct:.*\r?\n/gm, ''));
    const prints = fingerprints(wire);
    if (!prints.length) throw new Error('offer without DTLS fingerprint');
    const key = await deviceKey();
    const signature = await key.sign(dtlsMessage(this.sid, this.attempt, 'controller', prints));
    wire = insertSessionAttribute(wire, `a=x-farwing-identity:${signature}`);
    const pake = this.admission?.pakeKey;
    if (pake) {
      wire = insertSessionAttribute(wire, `a=x-farwing-pake:${pakeBinding(pake, this.sid, 'controller', prints)}`);
    }
    this.rt!.send(
      'rtc.offer',
      { attempt_id: this.attempt, restart, network: 3, sdp: wire },
      { sid: this.sid, gen: this.gen },
    );
    this.log(`offer gen=${this.gen}${restart ? ' restart' : ''}`);
    // The host answers once its screen and encoder are ready (a host that
    // must check its encoders again takes 10–15 s); then the connection.
    this.armNegotiation(pc, ANSWER_TIMEOUT_MS);
  }

  private armNegotiation(pc: RTCPeerConnection, ms: number) {
    window.clearTimeout(this.negotiateTimer);
    this.negotiateTimer = window.setTimeout(() => {
      if (pc.connectionState !== 'connected') {
        this.finish({
          reason: 'network',
          message: () => t().viewer.ended.noConnection,
          recoverable: true,
        });
      }
    }, ms);
  }

  private async onAnswer(envelope: Envelope<RtcDescription>) {
    const data = envelope.data!;
    if (data.attempt_id !== this.attempt) return;
    if (envelope.gen && envelope.gen !== this.gen) return;
    if (!data.sdp || data.sdp.length > 1 << 20) return;
    const signature = sessionAttribute(data.sdp, 'x-farwing-identity');
    const prints = fingerprints(data.sdp);
    let ok =
      signature !== undefined &&
      prints.length > 0 &&
      verify(
        unb64(this.started!.host.public_key),
        dtlsMessage(this.sid, this.attempt, 'host', prints),
        unb64(signature),
      );
    const pake = this.admission?.pakeKey;
    if (ok && pake) {
      ok = sessionAttribute(data.sdp, 'x-farwing-pake') === pakeBinding(pake, this.sid, 'host', prints);
    }
    if (!ok) {
      this.log('answer identity invalid');
      this.finish({ reason: 'identity', message: identityText, recoverable: false });
      return;
    }
    this.tracks = videoTracks(data.sdp);
    try {
      await this.pc!.setRemoteDescription({ type: 'answer', sdp: data.sdp });
    } catch (error) {
      this.log(`answer rejected: ${(error as Error).message}`);
      return;
    }
    this.remoteSet = true;
    if (this.pc && this.pc.connectionState !== 'connected') this.armNegotiation(this.pc, CONNECT_TIMEOUT_MS);
    this.descriptorAccepted = hasDependencyDescriptor(data.sdp);
    this.startAcknowledgements();
    this.log(
      `answer gen=${envelope.gen ?? 0} tracks=${[...this.tracks].map(([m, t]) => `${m}:${t}`).join(' ')} descriptor=${this.descriptorAccepted}`,
    );
    for (const c of this.pendingCandidates.splice(0)) {
      await this.pc!.addIceCandidate(c).catch(() => undefined);
    }
  }

  private async onCandidate(envelope: Envelope<{ attempt_id: string; candidate: RTCIceCandidateInit }>) {
    const data = envelope.data!;
    if (data.attempt_id !== this.attempt || !data.candidate?.candidate) return;
    if (!this.remoteSet) {
      if (this.pendingCandidates.length < 256) this.pendingCandidates.push(data.candidate);
      return;
    }
    await this.pc?.addIceCandidate(data.candidate).catch(() => undefined);
  }

  private onTrack(event: RTCTrackEvent) {
    const track = event.track;
    if (track.kind === 'audio') {
      this.directAudio = track;
      if (!this.forwardedAudio) this.addTrack(track);
      return;
    }
    const mid = event.transceiver.mid ?? '';
    const index =
      this.tracks.get(mid) ?? (event.streams[0]?.id.startsWith('video_') ? Number(event.streams[0].id.slice(6)) : -1);
    if (index === 0) {
      // Lower the playout delay where the browser allows it.
      const receiver = event.receiver as RTCRtpReceiver & { jitterBufferTarget?: number | null };
      if ('jitterBufferTarget' in receiver) receiver.jitterBufferTarget = 0;
      this.directVideo = track;
      if (!this.forward?.connection) this.addTrack(track);
    }
  }

  private addTrack(track: MediaStreamTrack) {
    for (const old of this.stream.getTracks()) if (old.kind === track.kind) this.stream.removeTrack(old);
    this.stream.addTrack(track);
    // A new MediaStream object makes <video> pick up the change.
    this.stream = new MediaStream(this.stream.getTracks());
    this.events.stream?.(this.stream);
    if (track.kind === 'video') {
      track.onunmute = () => {
        if (this.state !== 'playing' && this.state !== 'ended') this.setState('playing');
      };
    }
  }

  /// The host's forwarding instruction (§6.7).
  private onForward(instruction: fpp.Forward) {
    this.log(
      `host forwarding instruction room=${instruction.room} generation=${instruction.generation} tracks=${instruction.tracks.length}`,
    );
    const started = this.started;
    if (!started) return;
    this.forward ??= new ForwardReceiver({
      iceServers: iceServers(started),
      relay: started.force_relay,
      bearer: this.admission?.bearer,
      log: (line) => this.log(line),
      status: (generation, receiving, error) =>
        this.sendFrame(
          'farwing.control.v2',
          fpp.encode(fpp.Type.SESSION, fpp.forwardStatus(generation, receiving, error)),
        ),
      media: (video, audio, mode) => {
        this.forwardedAudio = !!audio;
        const picture = video ?? this.directVideo;
        if (picture) this.addTrack(picture);
        const sound = audio ?? this.directAudio;
        if (sound) this.addTrack(sound);
        this.events.forwarding?.(video ? mode : '');
      },
    });
    this.forward.follow(instruction);
  }

  private onConnectionState() {
    const pc = this.pc;
    if (!pc) return;
    this.log(`peer ${pc.connectionState}`);
    switch (pc.connectionState) {
      case 'connected':
        window.clearTimeout(this.negotiateTimer);
        window.clearTimeout(this.disconnectedTimer);
        this.iceRestarts = 0;
        if (this.state === 'negotiating') this.setState('waiting_media');
        else if (this.state === 'reconnecting') {
          this.setState('playing');
          void this.recover();
        }
        this.sendHello();
        break;
      case 'disconnected':
        window.clearTimeout(this.disconnectedTimer);
        this.disconnectedTimer = window.setTimeout(() => void this.restartIce(), 2000);
        break;
      case 'failed':
        void this.restartIce();
        break;
    }
  }

  private async restartIce() {
    if (this.stopped || !this.pc || this.pc.connectionState === 'connected') return;
    if (this.iceRestarts >= 3 || !this.rt?.connected) {
      this.finish({ reason: 'network', message: () => t().viewer.ended.connectionLost, recoverable: true });
      return;
    }
    this.iceRestarts += 1;
    this.setState('reconnecting');
    await this.negotiate(true).catch((error: Error) => this.log(`restart failed: ${error.message}`));
  }

  /// Every 100 ms, report the newest frame each video track decoded (the
  /// RTP timestamp libwebrtc reports for the frame it last delivered).
  private startAcknowledgements() {
    window.clearInterval(this.ackTimer);
    this.ackTimer = 0;
    if (!this.descriptorAccepted) return;
    this.ackTimer = window.setInterval(() => {
      for (const receiver of this.pc?.getReceivers() ?? []) {
        if (receiver.track.kind !== 'video') continue;
        const source = receiver.getSynchronizationSources?.()[0];
        if (!source || typeof source.rtpTimestamp !== 'number') continue;
        if (this.acked.get(source.source) === source.rtpTimestamp) continue;
        this.acked.set(source.source, source.rtpTimestamp);
        this.sendFrame(
          'farwing.control.v2',
          fpp.encode(fpp.Type.SESSION, fpp.videoAcknowledgement(source.source, source.rtpTimestamp)),
        );
      }
    }, 100);
  }

  // Peer protocol -------------------------------------------------------------

  private control(): RTCDataChannel | undefined {
    const dc = this.channels.get('farwing.control.v2');
    return dc?.readyState === 'open' ? dc : undefined;
  }

  private sendFrame(label: fpp.ChannelLabel, frame: Uint8Array) {
    const dc = this.channels.get(label);
    if (dc?.readyState === 'open') dc.send(frame as Uint8Array<ArrayBuffer>);
  }

  private nextRequest(): number {
    const rid = this.sessionRequest;
    this.sessionRequest = (this.sessionRequest % 0xffffffff) + 1;
    return rid;
  }

  /// A frame on the session channel, split as CLIPBOARD (60 KiB) and FILE
  /// (65526 B) messages are.
  private sendSession(type: number, body: Uint8Array, flags = 0, requestId = 0) {
    const chunk = type === fpp.Type.FILE ? 65526 : type === fpp.Type.CLIPBOARD ? 60 * 1024 : fpp.MAX_FRAME;
    for (const frame of fpp.encodeFragments(type, body, chunk, flags, requestId)) {
      this.sendFrame('farwing.session.v2', frame);
    }
  }

  /// A request on the session channel and its response body.
  private request(type: number, body: Uint8Array, timeoutMs = 15_000): Promise<Uint8Array> {
    const dc = this.channels.get('farwing.session.v2');
    if (dc?.readyState !== 'open') return Promise.reject(new PeerRequestError('unavailable', t().viewer.peer.notReady));
    const rid = this.nextRequest();
    const key = `${type}:${rid}`;
    return new Promise((resolve, reject) => {
      const timer = window.setTimeout(() => {
        this.requests.delete(key);
        reject(new PeerRequestError('timeout', t().viewer.peer.noResponse));
      }, timeoutMs);
      this.requests.set(key, { resolve, reject, timer });
      this.sendSession(type, body, 0, rid);
    });
  }

  /// A frame on the bulk channel once it has room.
  private async sendBulk(type: number, body: Uint8Array) {
    const dc = this.channels.get('farwing.bulk.v2');
    const closed = () => new PeerRequestError('unavailable', t().viewer.peer.closed);
    if (dc?.readyState !== 'open') throw closed();
    while (dc.bufferedAmount > BULK_HIGH) {
      await new Promise<void>((resolve) => {
        dc.addEventListener('bufferedamountlow', () => resolve(), { once: true });
        window.setTimeout(resolve, 1000);
      });
      if (dc.readyState !== 'open') throw closed();
    }
    dc.send(fpp.encode(type, body) as Uint8Array<ArrayBuffer>);
  }

  private maxMessage(): number {
    return Math.min(this.pc?.sctp?.maxMessageSize ?? 262144, fpp.MAX_FRAME) || 262144;
  }

  /// Our HELLO request every 500 ms (30 times) until the host answers.
  private sendHello() {
    if (this.helloAnswered || this.helloTimer || !this.control()) return;
    let count = 0;
    const once = () => {
      if (this.helloAnswered || count++ >= 30 || !this.control()) {
        window.clearInterval(this.helloTimer);
        this.helloTimer = 0;
        return;
      }
      this.sendFrame('farwing.control.v2', fpp.encode(fpp.Type.HELLO, this.helloBody()));
    };
    once();
    this.helloTimer = window.setInterval(once, 500);
  }

  private onFrame(label: string, data: ArrayBuffer) {
    const frame = fpp.decode(data);
    if (!frame) return;
    try {
      this.handleFrame(label, frame);
    } catch (error) {
      this.log(`bad frame 0x${frame.type.toString(16)}: ${(error as Error).message}`);
    }
  }

  private handleFrame(label: string, received: fpp.Frame) {
    let frame: fpp.Frame | undefined = received;
    if (frame.type === fpp.Type.CLIPBOARD) frame = this.clipboardParts.push(frame);
    else if (frame.type === fpp.Type.FILE) frame = this.fileParts.push(frame);
    if (!frame) return;
    const response = (frame.flags & fpp.Flag.RESPONSE) !== 0;
    const error = (frame.flags & fpp.Flag.ERROR) !== 0;
    if (response) {
      const key = `${frame.type}:${frame.requestId}`;
      const pending = this.requests.get(key);
      if (pending) {
        this.requests.delete(key);
        window.clearTimeout(pending.timer);
        if (error) {
          const e = fpp.parseError(frame.body);
          pending.reject(new PeerRequestError(e.code, e.message));
        } else {
          pending.resolve(frame.body);
        }
        if (error || frame.type !== fpp.Type.SESSION) return;
      }
    }
    if (error) {
      const e = fpp.parseError(frame.body);
      this.log(`peer error 0x${frame.type.toString(16)}: ${e.code} ${e.message}`);
      return;
    }
    switch (frame.type) {
      case fpp.Type.HELLO: {
        const hello = fpp.parseHello(frame.body);
        if (!fpp.compatible(hello)) {
          this.finish({ reason: 'incompatible', message: () => t().viewer.ended.incompatible, recoverable: false });
          return;
        }
        if (response) {
          this.helloAnswered = true;
          this.hostHello = hello;
          this.events.hostFeatures?.(hello.features);
          window.clearInterval(this.helloTimer);
          this.helloTimer = 0;
          this.log(`hello answered by ${hello.clientVersion} (${label})`);
        } else {
          // The host's own request: answer on the control channel.
          this.sendFrame(
            'farwing.control.v2',
            fpp.encode(fpp.Type.HELLO, this.helloBody(), fpp.Flag.RESPONSE, frame.requestId),
          );
        }
        return;
      }
      case fpp.Type.SESSION: {
        const message = fpp.parseSession(frame.body);
        if (message.kind === 'screens') {
          this.events.screens?.(message.value);
          if (!this.screensSeen) {
            this.screensSeen = true;
            this.reportCodecs();
          }
        } else if (message.kind === 'quality') {
          this.rememberTier(message.value.videoQuality);
          this.events.quality?.(message.value);
        } else if (message.kind === 'capture_setting_result') this.events.captureResult?.(message.errors);
        else if (message.kind === 'microphone_state') this.onMicrophoneState(message.state);
        else if (message.kind === 'forward') this.onForward(message.value);
        return;
      }
      case fpp.Type.PERMISSIONS:
        this.setGrants(fpp.parsePermissions(frame.body));
        return;
      case fpp.Type.PARTICIPANTS: {
        const people = fpp.parseParticipants(frame.body);
        this.myId = people.participants.find((p) => p.you)?.id ?? this.myId;
        // The host took the token back (another participant, or the owner).
        if (this.controlHeld && !this.turnPending && people.controlling !== this.myId) {
          this.loseControl(t().viewer.control.takenBack);
        }
        this.events.participants?.(people);
        return;
      }
      case fpp.Type.CLIPBOARD:
        if (!response) void this.clipboard.handle(frame);
        return;
      case fpp.Type.ANNOTATION:
        this.annotation.handle(frame.body);
        return;
      case fpp.Type.FILE:
        if (!response) this.files.handle(frame);
        return;
      case fpp.Type.TRANSFER: {
        const chunk = fpp.parseChunk(frame.body);
        if (chunk.kind === fpp.TransferKind.CLIPBOARD) this.clipboard.onChunk(chunk);
        else if (chunk.kind === fpp.TransferKind.FILE) this.files.onChunk(chunk);
        return;
      }
      case fpp.Type.FLOW: {
        const credit = fpp.parseFlow(frame.body);
        if (credit.kind === fpp.TransferKind.CLIPBOARD) this.clipboard.onFlow(credit);
        else if (credit.kind === fpp.TransferKind.FILE) this.files.onFlow(credit);
        return;
      }
      case fpp.Type.CURSOR: {
        const cursor = fpp.parseCursor(frame.body);
        if (cursor?.kind === 'shape') this.events.cursor?.(cursor.shape);
        else if (cursor?.kind === 'pointers') this.events.pointers?.(cursor.pointers);
        return;
      }
      case fpp.Type.HOST_EVENT:
        this.events.hostEvent?.(fpp.parseHostEvent(frame.body));
        return;
      case fpp.Type.AUDIO: {
        const state = fpp.parseJson<fpp.AudioState>(frame.body);
        if (state) this.events.audio?.(state);
        return;
      }
    }
  }

  /// Show another screen of the host (single track: the host reuses
  /// video_0 for it).
  switchScreen(screenId: number) {
    this.sendFrame('farwing.session.v2', fpp.encode(fpp.Type.SESSION, fpp.captureControl(screenId, true)));
  }

  /// Restart the capture of a screen: a fresh key frame.
  refresh(screenId: number) {
    this.sendFrame('farwing.session.v2', fpp.encode(fpp.Type.SESSION, fpp.captureControl(screenId, false)));
    window.setTimeout(() => this.switchScreen(screenId), 100);
  }

  /// The automatic tier is this viewer's (the host's last report).
  setCapture(setting: Omit<fpp.CaptureSetting, 'autoFrameQuality'>) {
    this.frameQuality = setting.frameQuality;
    const rid = this.nextRequest();
    const automatic = { ...setting, autoFrameQuality: this.autoTier() };
    this.sendFrame('farwing.session.v2', fpp.encode(fpp.Type.SESSION, fpp.captureSetting(automatic), 0, rid));
  }

  private autoTier(): number {
    const tier = Number(localStorage.getItem(AUTO_TIER_KEY + this.admission?.hostId));
    return tier >= 2 && tier <= 4 ? tier : 2;
  }

  private rememberTier(tier: number) {
    const host = this.admission?.hostId;
    if (host && this.frameQuality === 5 && tier >= 1 && tier <= 4) {
      localStorage.setItem(AUTO_TIER_KEY + host, String(tier));
    }
  }

  setAudioKbps(kbps: number) {
    this.sendFrame(
      'farwing.session.v2',
      fpp.encode(fpp.Type.AUDIO, fpp.json({ kind: 'set', request: this.nextRequest(), kbps })),
    );
  }

  // Control token ---------------------------------------------------------------

  private setGrants(grants: string[]) {
    this.grants = grants;
    this.events.permissions?.(grants);
    if (this.controlHeld && !grants.includes('control')) this.loseControl(t().viewer.control.revoked);
    if (this.mic && !grants.includes('microphone')) void this.stopMicrophone(t().viewer.microphone.revoked);
  }

  private emitControl(message?: string) {
    this.events.control?.({ held: this.controlHeld, pending: this.turnPending, message });
  }

  private loseControl(message: string) {
    this.input.stop();
    this.controlHeld = false;
    this.wantControl = false;
    this.emitControl(message);
  }

  get controlling(): boolean {
    return this.controlHeld;
  }

  /// Ask for the control token; input is forwarded only once it is held.
  async takeControl(automatic = false): Promise<ControlState> {
    if (!this.grants.includes('control')) {
      const state = { held: false, pending: false, message: t().viewer.control.noPermission };
      this.events.control?.(state);
      return state;
    }
    this.turnPending = true;
    this.emitControl();
    let message: string | undefined;
    try {
      const m = fpp.parseSession(await this.request(fpp.Type.SESSION, fpp.turnRequest(true, automatic)));
      const held = m.kind === 'turn_result' && m.held;
      message = m.kind === 'turn_result' && !held ? refusalText(m.refusal, m.holder) : undefined;
      this.controlHeld = held;
      this.wantControl = held;
      if (held) this.input.start();
      else this.input.stop();
    } catch (error) {
      message = (error as Error).message;
    } finally {
      this.turnPending = false;
    }
    // Automatic re-takes after a reconnection fail quietly.
    this.emitControl(automatic ? undefined : message);
    return { held: this.controlHeld, pending: false, message };
  }

  async releaseControl() {
    this.wantControl = false;
    const held = this.controlHeld;
    this.input.stop();
    this.controlHeld = false;
    this.emitControl();
    if (held) await this.request(fpp.Type.SESSION, fpp.turnRequest(false)).catch(() => undefined);
  }

  /// After an ICE restart: take the token again and re-enable the
  /// microphone (the host turned it off with the connection).
  private async recover() {
    if (this.wantControl) await this.takeControl(true);
    if (this.mic) {
      this.mic.ok = false;
      this.applyMicrophone();
      await this.microphonePolicy(true).catch((error: Error) => void this.stopMicrophone(error.message));
    }
  }

  // Microphone -------------------------------------------------------------------

  private async microphonePolicy(on: boolean) {
    const m = fpp.parseSession(await this.request(fpp.Type.SESSION, fpp.microphonePolicy(on), 10_000));
    if (m.kind === 'microphone_result' && m.code !== 0) throw new Error(fpp.microphoneError(m.code));
    if (on && this.mic) {
      this.mic.ok = true;
      this.applyMicrophone();
    }
  }

  /// Send the local microphone into the host's virtual microphone.
  async startMicrophone() {
    if (this.mic) return;
    if (!this.grants.includes('microphone')) throw new Error(t().viewer.microphone.noPermission);
    if (!navigator.mediaDevices?.getUserMedia) throw new Error(t().viewer.microphone.unsupported);
    const stream = await navigator.mediaDevices.getUserMedia({
      audio: {
        // The browser also plays the host's sound: cancel its echo.
        echoCancellation: true,
        noiseSuppression: true,
        autoGainControl: true,
        channelCount: { ideal: 2 },
        sampleRate: 48000,
      },
    });
    this.mic = { stream, ok: false, active: false };
    this.applyMicrophone();
    try {
      await this.microphonePolicy(true);
    } catch (error) {
      await this.stopMicrophone();
      throw error;
    }
  }

  async stopMicrophone(message?: string) {
    const mic = this.mic;
    if (!mic) return;
    this.mic = undefined;
    this.applyMicrophone(message);
    for (const track of mic.stream.getTracks()) track.stop();
    await this.microphonePolicy(false).catch(() => undefined);
  }

  private onMicrophoneState(state: number) {
    if (!this.mic) return;
    this.mic.active = state === fpp.MicrophoneState.ACTIVE || state === fpp.MicrophoneState.RESTART;
    this.applyMicrophone();
  }

  /// Send only once the host accepted and a program there listens.
  private applyMicrophone(message?: string) {
    const mic = this.mic;
    const sender = this.audioTx?.sender;
    const track = mic?.ok && mic.active ? (mic.stream.getAudioTracks()[0] ?? null) : null;
    if (sender && sender.track !== track) {
      void sender.replaceTrack(track).then(() => {
        if (!track) return;
        // The host sends no feedback for this stream: fix the bitrate.
        const parameters = sender.getParameters();
        if (parameters.encodings?.length) {
          parameters.encodings[0].maxBitrate = 128_000;
          void sender.setParameters(parameters).catch(() => undefined);
        }
      });
    }
    this.events.microphone?.({
      state: !mic ? 'off' : !mic.ok ? 'starting' : mic.active ? 'on' : 'waiting',
      message,
    });
  }

  /// This viewer's indicator pointer (not input; shown to the others).
  sendPointer(screenId: number, x: number, y: number, visible: boolean, click?: { button: number; phase: number }) {
    this.sendFrame(
      'farwing.control.v2',
      fpp.encode(fpp.Type.POINTER, fpp.pointer(++this.pointerSeq, screenId, x, y, visible, click)),
    );
  }

  get peer(): RTCPeerConnection | undefined {
    return this.pc;
  }

  /// The forwarder connection while the picture comes through it.
  get forwardPeer(): RTCPeerConnection | undefined {
    return this.forward?.connection;
  }

  get host(): fpp.Hello | undefined {
    return this.hostHello;
  }

  get relation(): string | undefined {
    return this.started?.relation;
  }

  // Realtime ------------------------------------------------------------------

  private onMessage(envelope: Envelope) {
    if (envelope.sid && envelope.sid !== this.sid) return;
    switch (envelope.type) {
      case 'rtc.answer':
        void this.onAnswer(envelope as Envelope<RtcDescription>);
        break;
      case 'rtc.candidate':
        void this.onCandidate(envelope as Envelope<{ attempt_id: string; candidate: RTCIceCandidateInit }>);
        break;
      case 'session.setting':
        this.onSetting(envelope.data as { kind?: string; capability?: HostCapability });
        break;
      case 'session.permissions':
        this.setGrants((envelope.data as { grants: string[] }).grants ?? this.grants);
        break;
      case 'session.ended': {
        const reason = (envelope.data as { reason?: string })?.reason ?? '';
        this.end(reason);
        break;
      }
      case 'error':
        this.log(`server error ${JSON.stringify(envelope.data)}`);
        break;
    }
  }

  private helloBody(): Uint8Array {
    return fpp.helloBody(
      this.maxMessage(),
      this.decoders.map((d) => HELLO_VIDEO[d.codec]),
    );
  }

  /// The host's capability (`session.setting` kind capability).
  private onSetting(setting: { kind?: string; capability?: HostCapability }) {
    if (setting.kind !== 'capability' || !setting.capability) return;
    this.hostCapability = setting.capability;
    this.reportCodecs();
  }

  private reportCodecs() {
    const capability = this.hostCapability;
    if (!capability || !this.screensSeen) return;
    const encoders = new Set(
      (capability.video_codec_capability ?? [])
        .filter((c) => c.chroma_sampling === 1 && c.bit_depth === 8)
        .map((c) => c.video_codec),
    );
    const common = this.decoders.map((d) => d.codec).filter((c) => encoders.has(c));
    const preferred = capability.preferred_codec;
    this.log(`host encoders ${[...encoders].join(',')} preferred=${preferred ?? '-'}`);
    this.events.codecs?.(
      common,
      common.find((c) => c === preferred),
    );
  }

  private async onClosed(code: number, reason: string) {
    this.log(`realtime closed ${code} ${reason}`);
    if (this.stopped || this.state === 'ended') return;
    if (!reconnectable(code)) {
      this.end(code === 4004 ? 'host_closed' : '');
      return;
    }
    // The peer connection may still be alive; get the realtime link back.
    for (let attempt = 0; attempt < 8 && !this.stopped && !this.isEnded(); attempt++) {
      await new Promise((r) => window.setTimeout(r, Math.min(25000, 1000 * 1.5 ** attempt)));
      try {
        await this.rt!.resume();
        this.log('realtime resumed');
        return;
      } catch {
        // Try again.
      }
    }
    this.end('');
  }

  private isEnded(): boolean {
    return this.state === 'ended';
  }

  private end(reason: string) {
    this.finish({
      reason,
      message: () => endText(reason),
      recoverable: !FINAL_REASONS.has(reason),
    });
  }

  /// End this session (once) and tell the page.
  private finish(ending: Ending) {
    if (this.state === 'ended') return;
    this.log(`ended: ${ending.reason} ${ending.message()}`);
    this.teardown();
    this.rt?.bye();
    this.setState('ended');
    this.events.ended?.(ending);
  }

  private teardown() {
    this.input.stop();
    this.controlHeld = false;
    this.clipboard.reset();
    this.files.reset();
    this.annotation.reset();
    for (const track of this.mic?.stream.getTracks() ?? []) track.stop();
    this.mic = undefined;
    for (const pending of this.requests.values()) {
      window.clearTimeout(pending.timer);
      pending.reject(new PeerRequestError('unavailable', t().viewer.peer.closed));
    }
    this.requests.clear();
    window.clearInterval(this.helloTimer);
    window.clearInterval(this.ackTimer);
    window.clearTimeout(this.negotiateTimer);
    window.clearTimeout(this.disconnectedTimer);
    this.helloTimer = 0;
    this.ackTimer = 0;
    this.acked.clear();
    for (const dc of this.channels.values()) dc.close();
    this.channels.clear();
    this.pc?.close();
    this.pc = undefined;
    if (this.forward) {
      this.forward.close();
      this.forward = undefined;
      this.events.forwarding?.('');
    }
  }

  /// Leave the session.
  stop() {
    if (this.stopped) return;
    this.stopped = true;
    this.teardown();
    this.rt?.bye();
    if (this.state !== 'ended') this.setState('ended');
  }
}
