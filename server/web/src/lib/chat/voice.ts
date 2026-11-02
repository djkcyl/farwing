// This browser in a voice channel or a call (`docs/protocol/chat-design.md`
// §7, §10, §12.2). A channel goes through the forwarder: an uplink this
// browser offers, a downlink the forwarder offers again as people come and
// go, every frame sealed with its sender's key (encoded transforms). A call
// goes straight to the other device, each side signing its description's
// fingerprints. Components read it with `useVoice()`.
import { useSyncExternalStore } from 'react';
import { t } from '../../i18n';
import { ApiError, call } from '../api';
import { deviceKey, verify } from '../device';
import { unb64, utf8 } from '../encoding';
import type { VoiceWorkerMessage } from '../voiceWorker';
import { open_ as openBox, seal, voiceSecret } from '../voiceKeys';
import { navigate } from '../router';
import { groupOf, play, type Sound, type SoundGroup } from './sounds';
import {
  callSignalMessage,
  sdpFingerprints,
  type Call,
  type CallSignal,
  type Id,
  type IceServer,
  type Snapshot,
  type VoiceChannelState,
  type VoiceJoined,
  type VoiceKeyPush,
  type VoiceMode,
  type VoiceMoved,
  type VoiceOffer,
  type VoiceParticipant,
  type ControlAnswered,
  type ControlEnded,
  type ControlRequest,
} from './types';

export interface MyVoice {
  /// The voice channel, or the direct message of a call.
  channel: Id;
  call?: string;
  participant: number;
  connected: boolean;
  muted: boolean;
  deafened: boolean;
  compat: boolean;
}

export interface VoiceSettings {
  input?: string;
  output?: string;
  echo: boolean;
  noise: boolean;
  gain: boolean;
  /// Microphone volume (percent, 0–200).
  inputVolume: number;
  mode: 'activity' | 'ptt';
  /// Activity threshold (dBFS); unset: follows the background noise.
  threshold?: number;
  /// `KeyboardEvent.code` of the push-to-talk key (while the page has focus).
  key?: string;
  /// The camera's device ID (unset: the default).
  camera?: string;
  /// Output volume and each person's (percent, 0–100: the page plays through
  /// media elements, which the browser's echo cancellation hears).
  volume: number;
  volumes: Record<string, number>;
  /// People this browser does not play.
  muted: string[];
  /// Sounds' volume (percent of the output volume) and kinds turned off.
  soundVolume: number;
  soundsOff: SoundGroup[];
}

export interface VoiceState {
  my?: MyVoice;
  /// Participants speaking now (this session's), and this browser.
  speaking: number[];
  meSpeaking: boolean;
  error?: string;
  /// Something to point out once (not an error).
  notice?: string;
  /// Mute and deafen chosen outside a channel (they hold when joining).
  muted: boolean;
  deafened: boolean;
  /// This browser's camera is on, and its picture.
  camera: boolean;
  localCamera?: MediaStreamTrack;
  /// The pictures received now: (participant, kind) and their track.
  videos: VoiceVideo[];
  /// Control of someone's screen asked for here (§8.3).
  control?: { id: string; sharer: string; allowed: boolean };
}

export interface VoiceVideo {
  participant: number;
  kind: string;
  track: MediaStreamTrack;
}

/// One participant's track this browser watches.
export interface TrackRef {
  participant: number;
  kind: string;
}

const SETTINGS = 'farwing.voice';
const KEY_SWITCH_MS = 2000;
const GATHER_FORWARDER_MS = 300;
const GATHER_CALL_MS = 3000;
/// Activity keeps sending this long after the level falls; push-to-talk
/// this long after the key is let go (§7.4).
const ACTIVITY_HOLD_MS = 400;
const PTT_TAIL_MS = 200;
/// From this channel bitrate on, voice goes in stereo.
const STEREO_FROM_KBPS = 96;
const CALL_KBPS = 64;
const TEST_MS = 5000;

function defaults(): VoiceSettings {
  return {
    echo: true,
    noise: true,
    gain: true,
    inputVolume: 100,
    mode: 'activity',
    volume: 100,
    volumes: {},
    muted: [],
    soundVolume: 60,
    soundsOff: [],
  };
}

export function loadSettings(): VoiceSettings {
  try {
    return { ...defaults(), ...(JSON.parse(localStorage.getItem(SETTINGS) ?? '{}') as Partial<VoiceSettings>) };
  } catch {
    return defaults();
  }
}

function gathered(pc: RTCPeerConnection, ms: number): Promise<void> {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise((resolve) => {
    const done = () => {
      if (pc.iceGatheringState === 'complete') {
        pc.removeEventListener('icegatheringstatechange', done);
        resolve();
      }
    };
    pc.addEventListener('icegatheringstatechange', done);
    window.setTimeout(resolve, ms);
  });
}

function iceConfig(servers: IceServer[]): RTCConfiguration {
  return {
    iceServers: servers.map((s) => ({ urls: s.urls, username: s.username, credential: s.credential })),
    bundlePolicy: 'max-bundle',
    rtcpMuxPolicy: 'require',
  };
}

/// (participant, kind) of a forwarded track's stream, `p<participant>-<kind>`.
export function parseStream(id: string): { participant: number; kind: string } | undefined {
  const m = /^p(\d+)-(\w+)$/.exec(id);
  return m ? { participant: Number(m[1]), kind: m[2] } : undefined;
}

/// The microphone as the settings ask (`stereo`: two channels).
export function openMicrophone(s: VoiceSettings, stereo = false): Promise<MediaStream> {
  return navigator.mediaDevices.getUserMedia({
    audio: {
      deviceId: s.input ? { exact: s.input } : undefined,
      echoCancellation: s.echo,
      noiseSuppression: s.noise,
      autoGainControl: s.gain,
      channelCount: stereo ? 2 : 1,
    },
  });
}

/// The rate (kbit/s) the sender of `pc`'s audio may use.
async function capBitrate(pc: RTCPeerConnection, kbps: number) {
  for (const sender of pc.getSenders()) {
    if (sender.track?.kind !== 'audio') continue;
    const params = sender.getParameters();
    params.encodings = (params.encodings.length ? params.encodings : [{}]).map((e) => ({
      ...e,
      maxBitrate: kbps * 1000,
    }));
    await sender.setParameters(params).catch(() => undefined);
  }
}

/// The microphone on its way out: its volume, a level, and a gate that
/// opens while speaking (activity or push-to-talk) and closes when muted.
export class Mic {
  readonly raw: MediaStream;
  readonly track: MediaStreamTrack;
  private context = new AudioContext();
  private input: GainNode;
  private gate: GainNode;
  private analyser: AnalyserNode;
  private data = new Float32Array(1024);
  private floor = -60;
  private openUntil = 0;
  private timer = 0;
  /// Push-to-talk held, and whether anything may be sent now.
  held = false;
  allowed = true;
  open = false;
  private settings: () => VoiceSettings;

  constructor(raw: MediaStream, settings: () => VoiceSettings, stereo: boolean) {
    this.raw = raw;
    this.settings = settings;
    const source = this.context.createMediaStreamSource(raw);
    this.input = this.context.createGain();
    this.gate = this.context.createGain();
    this.gate.gain.value = 0;
    this.analyser = this.context.createAnalyser();
    this.analyser.fftSize = 1024;
    const out = this.context.createMediaStreamDestination();
    out.channelCount = stereo ? 2 : 1;
    out.channelCountMode = 'explicit';
    source.connect(this.input);
    this.input.connect(this.analyser);
    this.input.connect(this.gate);
    this.gate.connect(out);
    this.track = out.stream.getAudioTracks()[0];
    this.apply();
    this.timer = window.setInterval(() => this.step(), 20);
  }

  static async open(settings: () => VoiceSettings, stereo = false): Promise<Mic> {
    return new Mic(await openMicrophone(settings(), stereo), settings, stereo);
  }

  apply() {
    this.input.gain.value = this.settings().inputVolume / 100;
  }

  /// The level after the volume (0–1), and its loudness (dBFS).
  measure(): [number, number] {
    this.analyser.getFloatTimeDomainData(this.data);
    let peak = 0;
    let sum = 0;
    for (const s of this.data) {
      peak = Math.max(peak, Math.abs(s));
      sum += s * s;
    }
    const rms = Math.sqrt(sum / this.data.length);
    return [peak, rms > 0 ? 20 * Math.log10(rms) : -127];
  }

  private step() {
    const [, db] = this.measure();
    this.floor = db < this.floor ? this.floor + (db - this.floor) * 0.2 : this.floor + (db - this.floor) * 0.005;
    const s = this.settings();
    const now = performance.now();
    if (s.mode === 'ptt') {
      if (this.held) this.openUntil = now + PTT_TAIL_MS;
    } else {
      const threshold = s.threshold ?? Math.min(-30, Math.max(-55, this.floor + 12));
      if (db > threshold) this.openUntil = now + ACTIVITY_HOLD_MS;
    }
    const open = this.allowed && now < this.openUntil;
    if (open !== this.open) {
      this.open = open;
      this.gate.gain.setTargetAtTime(open ? 1 : 0, this.context.currentTime, 0.005);
    }
  }

  close() {
    window.clearInterval(this.timer);
    this.raw.getTracks().forEach((t) => t.stop());
    this.track.stop();
    void this.context.close();
  }
}

/// Plays one sender's track (its own element: its own volume).
class Player {
  readonly element = new Audio();
  constructor(track: MediaStreamTrack) {
    this.element.autoplay = true;
    this.element.srcObject = new MediaStream([track]);
  }
  apply(volume: number, muted: boolean, output?: string) {
    this.element.volume = Math.min(1, Math.max(0, volume / 100));
    this.element.muted = muted;
    const sink = this.element as HTMLAudioElement & { setSinkId?(id: string): Promise<void> };
    if (output && sink.setSinkId) void sink.setSinkId(output).catch(() => undefined);
    void this.element.play().catch(() => undefined);
  }
  stop() {
    this.element.pause();
    this.element.srcObject = null;
  }
}

type Listener = () => void;

class VoiceController {
  state: VoiceState = { speaking: [], meSpeaking: false, muted: false, deafened: false, camera: false, videos: [] };
  settings = loadSettings();
  private listeners = new Set<Listener>();
  private session?: ChannelSession | CallSession;
  private ticker = 0;
  /// Pushes that came before the session they belong to (while joining).
  private early: { type: string; data: unknown }[] = [];
  /// Sends on the chat connection (set by the chat store).
  send: (type: string, data: unknown) => void = () => undefined;
  /// Puts a call into the chat state (set by the chat store).
  upsertCall: (c: Call) => void = () => undefined;
  /// Do not disturb, or within its hours (set by the chat store).
  dnd: () => boolean = () => false;
  snap: () => Snapshot | undefined = () => undefined;
  private watcher = 0;
  private device = '';
  /// What the sounds last saw.
  private heard: { ring: number; ringback: number; key?: string; joined: boolean; people?: Set<number> } = {
    ring: 0,
    ringback: 0,
    joined: false,
  };

  subscribe = (l: Listener) => {
    this.listeners.add(l);
    return () => {
      this.listeners.delete(l);
    };
  };
  get = () => this.state;
  set(patch: Partial<VoiceState>) {
    this.state = { ...this.state, ...patch };
    this.listeners.forEach((l) => l());
  }

  /// Sounds for the chat state `snap` reads (calls ringing, people coming
  /// and going) while the chat runs.
  attach(snap: () => Snapshot | undefined) {
    this.snap = snap;
    window.clearInterval(this.watcher);
    void deviceKey().then((k) => (this.device = k.deviceId));
    this.watcher = window.setInterval(() => this.watch(snap()), 1000);
  }

  detach() {
    window.clearInterval(this.watcher);
  }

  /// Plays `s` as the settings say (its kind on, its volume).
  sound(s: Sound) {
    const v = this.settings;
    if (v.soundsOff.includes(groupOf(s))) return;
    play(s, ((v.soundVolume / 100) * v.volume) / 100, v.output);
  }

  private watch(snap: Snapshot | undefined) {
    if (!snap) return;
    const now = Date.now();
    const me = snap.me.user_id;
    if (
      !this.dnd() &&
      snap.calls.some((c) => c.state === 'ringing' && c.callee.user_id === me) &&
      now - this.heard.ring > 2500
    ) {
      this.heard.ring = now;
      this.sound('ring');
    }
    if (
      snap.calls.some((c) => c.state === 'ringing' && c.caller_device === this.device) &&
      now - this.heard.ringback > 3000
    ) {
      this.heard.ringback = now;
      this.sound('ringback');
    }
    const my = this.state.my;
    const key = my ? `${my.channel}/${my.call ?? ''}` : undefined;
    if (key !== this.heard.key) {
      if (this.heard.joined) this.sound('left');
      this.heard = { ...this.heard, key, joined: false, people: undefined };
    }
    if (my?.connected && !this.heard.joined) {
      this.heard.joined = true;
      this.sound('joined');
    }
    if (my && !my.call && this.heard.joined) {
      const people = new Set(
        snap.voice.find((v) => v.channel_id === my.channel)?.participants.map((p) => p.participant),
      );
      const before = this.heard.people;
      if (before) {
        if ([...people].some((p) => !before.has(p) && p !== my.participant)) this.sound('came');
        else if ([...before].some((p) => !people.has(p))) this.sound('went');
      }
      this.heard.people = people;
    }
  }

  saveSettings(s: VoiceSettings) {
    this.settings = s;
    localStorage.setItem(SETTINGS, JSON.stringify(s));
    this.session?.applySettings();
    this.listeners.forEach((l) => l());
  }

  /// The microphone's level now (0–1) while in voice.
  inputLevel(): number | undefined {
    return this.session?.inputLevel();
  }

  inVoice(): boolean {
    return !!this.session;
  }

  private start(session: ChannelSession | CallSession) {
    this.session = session;
    window.clearInterval(this.ticker);
    this.ticker = window.setInterval(() => this.session?.tick(), 200);
    const early = this.early;
    this.early = [];
    for (const e of early) this.push(e.type, e.data);
  }

  async stop(tell: boolean) {
    const s = this.session;
    this.session = undefined;
    this.early = [];
    window.clearInterval(this.ticker);
    this.set({
      my: undefined,
      speaking: [],
      meSpeaking: false,
      camera: false,
      localCamera: undefined,
      videos: [],
      control: undefined,
    });
    if (s) await s.close(tell);
  }

  /// Turns this browser's camera on or off (in a voice channel).
  async setCamera(on: boolean) {
    if (!this.session) return;
    try {
      await this.session.setCamera(on);
      this.set({ camera: on, localCamera: this.session.cameraTrack() });
    } catch (e) {
      this.set({ camera: false, localCamera: undefined });
      this.note(e instanceof DOMException ? t().chat.voice.noCamera : t().chat.voice.failed);
    }
  }

  /// The cameras and screens this browser watches (every mic comes anyway).
  watchTracks(refs: TrackRef[]) {
    if (this.session instanceof ChannelSession) this.session.watch(refs);
  }

  /// Asks the device sharing its whole screen for control.
  async requestControl(sharer: string) {
    if (!(this.session instanceof ChannelSession)) return;
    try {
      const { id } = await call<{ id: string }>(
        'POST',
        `/api/v2/chat/channels/${this.session.channel}/voice/control-request`,
        { sharer_device: sharer },
      );
      this.set({ control: { id, sharer, allowed: false } });
    } catch {
      this.note(t().chat.voice.failed);
    }
  }

  /// Gives control back.
  endControl() {
    const c = this.state.control;
    if (!c || !(this.session instanceof ChannelSession)) return;
    void call('DELETE', `/api/v2/chat/channels/${this.session.channel}/voice/control-request/${c.id}`).catch(
      () => undefined,
    );
    this.set({ control: undefined });
  }

  /// Ends this browser's voice with `why` (`tell`: the server too).
  fail(why: string, tell = false) {
    void this.stop(tell);
    this.set({ error: why });
    window.setTimeout(() => this.set({ error: undefined }), 5000);
  }

  /// Points something out once.
  note(text: string) {
    this.set({ notice: text });
    window.setTimeout(() => this.set({ notice: undefined }), 5000);
  }

  async join(channel: Id) {
    await this.stop(true);
    const { muted, deafened } = this.state;
    this.set({ my: { channel, participant: 0, connected: false, muted, deafened, compat: false }, error: undefined });
    try {
      await voiceSecret().catch(() => undefined);
      const joined = await call<VoiceJoined>('POST', `/api/v2/chat/channels/${channel}/voice/join`, {
        self_muted: muted,
        self_deafened: deafened,
      });
      if (joined.mode === 'e2ee' && typeof RTCRtpScriptTransform === 'undefined') {
        await call('POST', `/api/v2/chat/channels/${channel}/voice/leave`);
        throw new Error(t().chat.voice.noTransform);
      }
      const session = new ChannelSession(this, joined);
      this.start(session);
      this.set({ my: { ...this.state.my!, participant: joined.participant, compat: joined.mode === 'compatible' } });
      await session.begin();
    } catch (e) {
      const reason = e instanceof ApiError ? (e.details as { reason?: string } | undefined)?.reason : undefined;
      const texts = t().chat.voice;
      this.fail(
        reason === 'voice_full'
          ? texts.full
          : reason === 'voice_no_forwarding'
            ? texts.noForwarding
            : reason === 'voice_quota'
              ? texts.quota
              : e instanceof DOMException
                ? texts.noMicrophone
                : e instanceof Error && !(e instanceof ApiError)
                  ? e.message
                  : texts.failed,
        true,
      );
    }
  }

  /// A person's volume (percent), by account.
  setVolume(user: string, percent: number) {
    this.saveSettings({ ...this.settings, volumes: { ...this.settings.volumes, [user]: percent } });
  }

  /// This browser stops (or starts again) playing `user`.
  muteUser(user: string, muted: boolean) {
    const rest = this.settings.muted.filter((u) => u !== user);
    this.saveSettings({ ...this.settings, muted: muted ? [...rest, user] : rest });
  }

  /// A moderator's change: server mute or deafen, or a move to another
  /// voice channel.
  moderate(
    channel: Id,
    participant: number,
    change: { server_muted?: boolean; server_deafened?: boolean; move_to?: Id },
  ) {
    return call('PATCH', `/api/v2/chat/channels/${channel}/voice/participants/${participant}`, change);
  }

  disconnect(channel: Id, participant: number) {
    return call('DELETE', `/api/v2/chat/channels/${channel}/voice/participants/${participant}`);
  }

  setSelf(muted: boolean, deafened: boolean) {
    if (this.state.my && (muted || deafened) !== (this.state.muted || this.state.deafened)) {
      this.sound(muted || deafened ? 'muted' : 'unmuted');
    }
    this.set({ muted, deafened, my: this.state.my ? { ...this.state.my, muted, deafened } : undefined });
    this.session?.setSelf(muted, deafened);
  }

  async startCall(channel: Id) {
    await this.stop(true);
    try {
      const c = await call<Call>('POST', `/api/v2/chat/dm/${channel}/call`, {});
      this.beginCall(c);
    } catch {
      this.fail(t().chat.voice.failed);
    }
  }

  async answer(id: string, take: boolean) {
    await this.stop(true);
    try {
      const c = await call<Call>('POST', `/api/v2/chat/calls/${id}/${take ? 'move' : 'answer'}`);
      this.beginCall(c);
    } catch {
      this.fail(t().chat.voice.failed);
    }
  }

  decline(id: string) {
    void call('POST', `/api/v2/chat/calls/${id}/decline`).catch(() => undefined);
  }

  private beginCall(c: Call) {
    this.upsertCall(c);
    const { muted, deafened } = this.state;
    const session = new CallSession(this, c);
    this.start(session);
    this.set({
      my: { channel: c.channel_id, call: c.id, participant: 0, connected: false, muted, deafened, compat: false },
    });
    session
      .begin()
      .catch((e) => this.fail(e instanceof DOMException ? t().chat.voice.noMicrophone : t().chat.voice.failed, true));
  }

  /// A `voice.*` or `call.*` push.
  push(type: string, data: unknown) {
    const s = this.session;
    if (!s) {
      if (this.state.my && this.early.length < 100) this.early.push({ type, data });
      return;
    }
    if (s instanceof ChannelSession) {
      const d = data as { channel_id?: Id };
      if (d.channel_id !== s.channel) return;
      if (type === 'voice.control_answered') {
        const a = data as ControlAnswered;
        if (a.id !== this.state.control?.id) return;
        if (!a.allowed) {
          this.set({ control: undefined });
          this.note(t().chat.voice.controlDeclined);
          return;
        }
        this.set({ control: { ...this.state.control, allowed: true } });
        // A remote desktop session to the sharer, under the share's grant.
        navigate(`/view/${a.sharer_device}`);
        return;
      }
      if (type === 'voice.control_ended') {
        if ((data as ControlEnded).id === this.state.control?.id) {
          this.set({ control: undefined });
          this.note(t().chat.voice.controlEnded);
        }
        return;
      }
      if (type === 'voice.control_request') {
        // Only devices that share their screen are asked: not a browser.
        void call(
          'POST',
          `/api/v2/chat/channels/${s.channel}/voice/control-request/${(data as ControlRequest).id}/answer`,
          { allowed: false, clipboard: false },
        ).catch(() => undefined);
        return;
      }
      if (type === 'voice.offer') void s.offer(data as VoiceOffer);
      else if (type === 'voice.keys') void s.key(data as VoiceKeyPush);
      else if (type === 'voice.state') void s.state((data as VoiceChannelState).participants);
      else if (type === 'voice.moved') {
        // Moved by a moderator, or the channel's node went away: join there.
        const moved = data as VoiceMoved;
        if (moved.to) this.sound('removed');
        void this.join(moved.to ?? moved.channel_id);
      } else if (type === 'voice.left') {
        const why = (data as { reason: string }).reason;
        const texts = t().chat.voice;
        if (why === 'disconnected') this.sound('removed');
        this.fail(
          (
            { disconnected: texts.leftDisconnected, forbidden: texts.leftForbidden, idle: texts.leftIdle } as Record<
              string,
              string
            >
          )[why] ?? texts.failed,
        );
      }
    } else if (s instanceof CallSession) {
      const id = (data as { id?: string; call_id?: string }).id ?? (data as { call_id?: string }).call_id;
      if (id !== s.id) return;
      if (type === 'call.answered') void s.answered(data as Call);
      else if (type === 'call.signal') void s.signal(data as CallSignal);
      else if (type === 'call.ended') void this.stop(false);
    }
  }
}

class ChannelSession {
  readonly channel: Id;
  private participant: number;
  private mode: VoiceMode;
  private bitrate: number;
  private ice: IceServer[];
  private worker?: Worker;
  private mic?: Mic;
  private uplink?: RTCPeerConnection;
  private downlink?: RTCPeerConnection;
  private micTx?: RTCRtpTransceiver;
  /// The camera's two encodings: 720p30 and 320×180@15.
  private cam?: { stream: MediaStream; hi: RTCRtpTransceiver; lo: RTCRtpTransceiver };
  private watched = '';
  /// Voices by participant; screen sounds by `participant`.
  private players = new Map<number, Player>();
  private screenPlayers = new Map<number, Player>();
  private receivers = new Map<number, RTCRtpReceiver>();
  private videos = new Map<string, VoiceVideo>();
  private people = new Map<number, VoiceParticipant>();
  private own = { id: 0, material: crypto.getRandomValues(new Uint8Array(20)) };
  private closed = false;
  private hinted = false;
  private voice: VoiceController;

  constructor(voice: VoiceController, joined: VoiceJoined) {
    this.voice = voice;
    this.channel = joined.channel_id;
    this.participant = joined.participant;
    this.mode = joined.mode;
    this.bitrate = joined.bitrate_kbps;
    this.ice = joined.ice_servers;
    for (const p of joined.participants) this.people.set(p.participant, p);
  }

  private e2ee() {
    return this.mode === 'e2ee';
  }

  async begin() {
    if (this.e2ee()) {
      this.worker = new Worker(new URL('../voiceWorker.ts', import.meta.url), { type: 'module' });
      this.post({ own: { id: this.own.id, material: this.own.material } });
      await this.handKey([...this.people.values()]);
    }
    window.addEventListener('keydown', this.keyDown);
    window.addEventListener('keyup', this.keyUp);
    this.mic = await Mic.open(() => this.voice.settings, this.bitrate >= STEREO_FROM_KBPS);
    this.applySelf();
    const pc = new RTCPeerConnection(iceConfig(this.ice));
    this.uplink = pc;
    const stream = new MediaStream([this.mic.track]);
    const tx = pc.addTransceiver(this.mic.track, { direction: 'sendonly', streams: [stream] });
    this.micTx = tx;
    if (this.worker) tx.sender.transform = new RTCRtpScriptTransform(this.worker, { kind: 'send' });
    pc.onconnectionstatechange = () => {
      const my = this.voice.state.my;
      if (my) this.voice.set({ my: { ...my, connected: pc.connectionState === 'connected' } });
    };
    await this.publish();
  }

  /// Offers the uplink as it is now, naming each section's kind.
  private async publish() {
    const pc = this.uplink;
    if (!pc) return;
    await pc.setLocalDescription(await pc.createOffer());
    await gathered(pc, GATHER_FORWARDER_MS);
    const tracks = [{ mid: this.micTx?.mid ?? '0', kind: 'mic' }];
    if (this.cam) {
      tracks.push({ mid: this.cam.hi.mid ?? '', kind: 'cam_hi' }, { mid: this.cam.lo.mid ?? '', kind: 'cam_lo' });
    }
    const answer = await call<{ sdp: string }>('PUT', `/api/v2/chat/channels/${this.channel}/voice/publish`, {
      sdp: pc.localDescription!.sdp,
      tracks,
    });
    if (this.closed) return;
    await pc.setRemoteDescription({ type: 'answer', sdp: answer.sdp });
    await capBitrate(pc, this.bitrate);
  }

  /// The camera on (two encodings of one capture, H.264) or off.
  async setCamera(on: boolean) {
    const pc = this.uplink;
    if (!pc || on === !!this.cam) return;
    if (!on) {
      const cam = this.cam!;
      this.cam = undefined;
      for (const tx of [cam.hi, cam.lo]) {
        await tx.sender.replaceTrack(null);
        tx.direction = 'inactive';
      }
      cam.stream.getTracks().forEach((t) => t.stop());
      await this.publish();
      return;
    }
    const stream = await navigator.mediaDevices.getUserMedia({
      video: {
        deviceId: this.voice.settings.camera ? { exact: this.voice.settings.camera } : undefined,
        width: { ideal: 1280 },
        height: { ideal: 720 },
        frameRate: { ideal: 30 },
      },
    });
    const track = stream.getVideoTracks()[0];
    // Sections of an earlier camera are used again.
    const idle = pc.getTransceivers().filter((t) => t.direction === 'inactive' && t.receiver.track.kind === 'video');
    const section = async (
      source: MediaStreamTrack,
      encoding: RTCRtpEncodingParameters,
      reuse?: RTCRtpTransceiver,
    ): Promise<RTCRtpTransceiver> => {
      let tx = reuse;
      if (tx) {
        tx.direction = 'sendonly';
        await tx.sender.replaceTrack(source);
        const params = tx.sender.getParameters();
        params.encodings = [{ ...params.encodings[0], ...encoding }];
        await tx.sender.setParameters(params).catch(() => undefined);
      } else {
        tx = pc.addTransceiver(source, { direction: 'sendonly', streams: [stream], sendEncodings: [encoding] });
        if (this.worker) tx.sender.transform = new RTCRtpScriptTransform(this.worker, { kind: 'send', video: true });
      }
      // H.264 as every receiver decodes it; constrained baseline first, as
      // the forwarder offers it.
      const h264 = (RTCRtpSender.getCapabilities('video')?.codecs ?? [])
        .filter((c) => c.mimeType.toLowerCase() === 'video/h264' && c.sdpFmtpLine?.includes('packetization-mode=1'))
        .sort((a, b) => Number(!a.sdpFmtpLine?.includes('42e01f')) - Number(!b.sdpFmtpLine?.includes('42e01f')));
      if (h264.length) tx.setCodecPreferences(h264);
      return tx;
    };
    const hi = await section(track, { maxBitrate: 1_200_000, maxFramerate: 30 }, idle[0]);
    const lo = await section(
      track.clone(),
      { maxBitrate: 80_000, maxFramerate: 15, scaleResolutionDownBy: 4 },
      idle[1],
    );
    this.cam = { stream, hi, lo };
    await this.publish();
  }

  cameraTrack(): MediaStreamTrack | undefined {
    return this.cam?.stream.getVideoTracks()[0];
  }

  /// The chosen cameras and screens, sent when they change.
  watch(refs: TrackRef[]) {
    const key = refs
      .map((r) => `${r.participant}-${r.kind}`)
      .sort()
      .join(',');
    if (key === this.watched) return;
    this.watched = key;
    void call('PUT', `/api/v2/chat/channels/${this.channel}/voice/subscribe`, { tracks: refs }).catch((e) => {
      this.watched = '';
      const reason = e instanceof ApiError ? (e.details as { reason?: string } | undefined)?.reason : undefined;
      if (reason === 'voice_quota') this.voice.note(t().chat.voice.quota);
    });
  }

  private publishVideos() {
    this.voice.set({ videos: [...this.videos.values()] });
  }

  private post(message: VoiceWorkerMessage) {
    this.worker?.postMessage(message);
  }

  private async handKey(to: VoiceParticipant[]) {
    if (!this.e2ee()) return;
    const me = (await deviceKey()).deviceId;
    const boxes = await Promise.all(
      to
        .filter((p) => p.participant !== this.participant && p.device_id !== me && p.x25519)
        .map((p) =>
          seal(String(this.channel), this.participant, p.device_id, p.x25519, this.own.id, this.own.material).catch(
            () => undefined,
          ),
        ),
    );
    const sealed = boxes.filter((b) => b !== undefined);
    if (!sealed.length) return;
    await call('POST', `/api/v2/chat/channels/${this.channel}/voice/keys`, {
      key_id: this.own.id,
      boxes: sealed,
    }).catch(() => undefined);
  }

  async offer(offer: VoiceOffer) {
    if (!this.downlink) {
      const pc = new RTCPeerConnection(iceConfig(this.ice));
      this.downlink = pc;
      pc.ontrack = (e) => {
        const stream = parseStream(e.streams[0]?.id ?? '');
        if (!stream) return;
        const { participant, kind } = stream;
        const video = e.track.kind === 'video';
        if (this.worker) {
          e.receiver.transform = new RTCRtpScriptTransform(this.worker, { kind: 'receive', participant, video });
        }
        if (video) {
          const id = `${participant}-${kind}`;
          this.videos.set(id, { participant, kind, track: e.track });
          e.track.onended = () => {
            if (this.videos.get(id)?.track === e.track) {
              this.videos.delete(id);
              this.publishVideos();
            }
          };
          this.publishVideos();
          return;
        }
        const players = kind === 'mic' ? this.players : this.screenPlayers;
        players.get(participant)?.stop();
        players.set(participant, new Player(e.track));
        if (kind === 'mic') this.receivers.set(participant, e.receiver);
        this.applyPlayer(participant);
      };
    }
    const pc = this.downlink;
    await pc.setRemoteDescription({ type: 'offer', sdp: offer.sdp });
    await pc.setLocalDescription(await pc.createAnswer());
    await gathered(pc, GATHER_FORWARDER_MS);
    this.voice.send('voice.answer', {
      channel_id: this.channel,
      generation: offer.generation,
      sdp: pc.localDescription!.sdp,
    });
  }

  async key(push: VoiceKeyPush) {
    const sender = this.people.get(push.participant);
    if (!sender) return;
    try {
      const { key, id } = await openBox(String(this.channel), push, sender.ed25519);
      this.post({ participant: push.participant, key: { id, material: key } });
    } catch {
      // Not for this device, or not signed by its sender.
    }
  }

  async state(list: VoiceParticipant[]) {
    const next = new Map(list.map((p) => [p.participant, p]));
    const joined = list.filter((p) => !this.people.has(p.participant));
    const left = [...this.people.keys()].filter((p) => !next.has(p));
    this.people = next;
    for (const p of left) {
      this.players.get(p)?.stop();
      this.players.delete(p);
      this.screenPlayers.get(p)?.stop();
      this.screenPlayers.delete(p);
      this.receivers.delete(p);
    }
    // Pictures of people gone, or of tracks they stopped sending.
    for (const [id, v] of this.videos) {
      if (!next.get(v.participant)?.tracks.includes(v.kind)) this.videos.delete(id);
    }
    this.publishVideos();
    this.applySelf();
    for (const p of this.players.keys()) this.applyPlayer(p);
    // Another device of this account here: said once.
    const me = this.people.get(this.participant)?.user_id;
    if (!this.hinted && me && list.some((p) => p.user_id === me && p.participant !== this.participant)) {
      this.hinted = true;
      this.voice.note(t().chat.voice.otherDevice);
    }
    if (left.length && this.e2ee()) {
      // Those who left must not hear what comes next.
      this.own = { id: (this.own.id + 1) % 256, material: crypto.getRandomValues(new Uint8Array(20)) };
      const next = this.own;
      await this.handKey([...this.people.values()]);
      window.setTimeout(() => this.post({ own: { id: next.id, material: next.material } }), KEY_SWITCH_MS);
    } else if (joined.length) {
      await this.handKey(joined);
    }
  }

  /// Deafened by itself or by a moderator.
  private deaf() {
    return this.voice.state.deafened || !!this.people.get(this.participant)?.server_deafened;
  }

  private applyPlayer(participant: number) {
    const s = this.voice.settings;
    const user = this.people.get(participant)?.user_id ?? '';
    const volume = ((s.volumes[user] ?? 100) * s.volume) / 100;
    const muted = this.deaf() || s.muted.includes(user);
    this.players.get(participant)?.apply(volume, muted, s.output);
    this.screenPlayers.get(participant)?.apply(volume, muted, s.output);
  }

  applySettings() {
    for (const p of new Set([...this.players.keys(), ...this.screenPlayers.keys()])) this.applyPlayer(p);
    this.mic?.apply();
    this.applySelf();
  }

  /// The microphone may send unless muted, deafened, muted by a moderator
  /// or without `speak` (the gate opens while speaking).
  private applySelf() {
    const me = this.people.get(this.participant);
    if (!this.mic) return;
    this.mic.allowed = !this.voice.state.muted && !this.deaf() && !me?.server_muted && !me?.cannot_speak;
  }

  setSelf(muted: boolean, deafened: boolean) {
    this.applySelf();
    for (const p of this.players.keys()) this.applyPlayer(p);
    void call('PUT', `/api/v2/chat/channels/${this.channel}/voice/state`, {
      self_muted: muted,
      self_deafened: deafened,
    }).catch(() => undefined);
  }

  private keyDown = (e: KeyboardEvent) => {
    if (this.voice.settings.mode === 'ptt' && e.code === this.voice.settings.key && this.mic) this.mic.held = true;
  };
  private keyUp = (e: KeyboardEvent) => {
    if (e.code === this.voice.settings.key && this.mic) this.mic.held = false;
  };

  inputLevel(): number {
    return this.mic?.measure()[0] ?? 0;
  }

  tick() {
    const speaking: number[] = [];
    for (const [p, r] of this.receivers) {
      const source = r.getSynchronizationSources()[0];
      if (
        source &&
        (source.audioLevel ?? 0) > 0.03 &&
        performance.timeOrigin + performance.now() - source.timestamp < 400
      ) {
        speaking.push(p);
      }
    }
    this.voice.set({ speaking, meSpeaking: !!this.mic?.open });
  }

  async close(tell: boolean) {
    this.closed = true;
    window.removeEventListener('keydown', this.keyDown);
    window.removeEventListener('keyup', this.keyUp);
    if (tell) await call('POST', `/api/v2/chat/channels/${this.channel}/voice/leave`).catch(() => undefined);
    for (const p of [...this.players.values(), ...this.screenPlayers.values()]) p.stop();
    this.players.clear();
    this.screenPlayers.clear();
    this.cam?.stream.getTracks().forEach((t) => t.stop());
    this.uplink?.close();
    this.downlink?.close();
    this.mic?.close();
    this.worker?.terminate();
  }
}

/// A call's sections, the same on both devices, by MID order: the kind of
/// a received track is its section's (§7.6).
const CALL_SECTIONS = ['mic', 'cam_hi', 'screen', 'screen_audio'];

/// H.264 (constrained baseline first) with its retransmissions: pictures in
/// calls are H.264 as every device decodes it.
function h264Preferences(): RTCRtpCodec[] {
  const codecs = RTCRtpReceiver.getCapabilities('video')?.codecs ?? [];
  const h264 = codecs
    .filter((c) => c.mimeType.toLowerCase() === 'video/h264' && c.sdpFmtpLine?.includes('packetization-mode=1'))
    .sort((a, b) => Number(!a.sdpFmtpLine?.includes('42e01f')) - Number(!b.sdpFmtpLine?.includes('42e01f')));
  return h264.length ? [...h264, ...codecs.filter((c) => c.mimeType.toLowerCase() === 'video/rtx')] : [];
}

function preferH264(transceiver: RTCRtpTransceiver | undefined) {
  const codecs = h264Preferences();
  if (transceiver && codecs.length) {
    try {
      transceiver.setCodecPreferences(codecs);
    } catch {
      // The browser's own choice then.
    }
  }
}

class CallSession {
  readonly id: string;
  private c: Call;
  private pc?: Promise<RTCPeerConnection>;
  private micReady?: Promise<Mic>;
  private mic?: Mic;
  private player?: Player;
  private sharePlayer?: Player;
  private receiver?: RTCRtpReceiver;
  private cam?: MediaStream;
  private me = '';
  private closed = false;
  private voice: VoiceController;

  constructor(voice: VoiceController, c: Call) {
    this.voice = voice;
    this.c = c;
    this.id = c.id;
  }

  async begin() {
    this.me = (await deviceKey()).deviceId;
    await this.microphone();
    window.addEventListener('keydown', this.keyDown);
    window.addEventListener('keyup', this.keyUp);
    // The callee waits for the caller's offer.
    if (this.c.state === 'active' && this.c.caller_device === this.me) await this.answered(this.c);
  }

  private microphone(): Promise<Mic> {
    this.micReady ??= Mic.open(() => this.voice.settings).then((mic) => {
      if (this.closed) mic.close();
      this.mic = mic;
      this.setSelf(this.voice.state.muted, this.voice.state.deafened);
      return mic;
    });
    return this.micReady;
  }

  /// The connection to the other device: made once its relays are known.
  private peer(): Promise<RTCPeerConnection> {
    this.pc ??= this.microphone().then((mic) => {
      const pc = new RTCPeerConnection({ iceServers: iceConfig(this.c.ice_servers).iceServers });
      // A track (not a transceiver): the answering side's goes into the offer's audio.
      pc.addTrack(mic.track, new MediaStream([mic.track]));
      if (this.c.caller_device === this.me) {
        // The caller lays out every section at once: camera (sent when on),
        // the other's screen and its sound.
        preferH264(pc.addTransceiver('video', { direction: 'sendrecv' }));
        preferH264(pc.addTransceiver('video', { direction: 'recvonly' }));
        pc.addTransceiver('audio', { direction: 'recvonly' });
      }
      pc.ontrack = (e) => {
        const kind = CALL_SECTIONS[Number(e.transceiver.mid)] ?? 'mic';
        if (kind === 'mic') {
          this.player?.stop();
          this.player = new Player(e.track);
          this.receiver = e.receiver;
        } else if (kind === 'screen_audio') {
          this.sharePlayer?.stop();
          this.sharePlayer = new Player(e.track);
        } else {
          const track = e.track;
          const show = (on: boolean) => {
            const others = this.voice.state.videos.filter((x) => x.kind !== kind);
            this.voice.set({ videos: on ? [...others, { participant: 1, kind, track }] : others });
          };
          // A picture shows while it comes.
          track.onunmute = () => show(true);
          track.onmute = () => show(false);
          track.onended = () => show(false);
          if (!track.muted) show(true);
        }
        this.applySettings();
      };
      pc.onconnectionstatechange = () => {
        const my = this.voice.state.my;
        if (my) this.voice.set({ my: { ...my, connected: pc.connectionState === 'connected' } });
        if (pc.connectionState === 'connected') void capBitrate(pc, CALL_KBPS);
      };
      return pc;
    });
    return this.pc;
  }

  private async signalOut(kind: 'offer' | 'answer', sdp: string) {
    const signature = await (await deviceKey()).sign(utf8(callSignalMessage(this.id, sdpFingerprints(sdp))));
    this.voice.send('call.signal', { call_id: this.id, kind, data: sdp, signature });
  }

  /// Answered (or moved): the caller offers.
  async answered(c: Call) {
    const moved = this.pc && (c.callee_device !== this.c.callee_device || c.caller_device !== this.c.caller_device);
    this.c = c;
    if (moved) {
      void this.pc?.then((pc) => pc.close());
      this.pc = undefined;
    }
    if (c.caller_device !== this.me) return;
    const pc = await this.peer();
    await pc.setLocalDescription(await pc.createOffer());
    await gathered(pc, GATHER_CALL_MS);
    await this.signalOut('offer', pc.localDescription!.sdp);
  }

  async signal(s: CallSignal) {
    if (s.kind === 'candidate') return;
    const peerKey = this.c.caller_device === this.me ? this.c.callee_key : this.c.caller_key;
    const ok = verify(
      unb64(peerKey),
      utf8(callSignalMessage(this.id, sdpFingerprints(s.data))),
      unb64(s.signature ?? ''),
    );
    if (!ok) {
      this.voice.fail(t().chat.voice.unverified);
      return;
    }
    const pc = await this.peer();
    if (s.kind === 'offer') {
      await pc.setRemoteDescription({ type: 'offer', sdp: s.data });
      // The camera's section sends too (once the camera is on); pictures are H.264.
      const camera = pc.getTransceivers().find((x) => x.mid === '1');
      if (camera && camera.direction !== 'sendrecv') camera.direction = 'sendrecv';
      preferH264(camera);
      preferH264(pc.getTransceivers().find((x) => x.mid === '2'));
      await pc.setLocalDescription(await pc.createAnswer());
      await gathered(pc, GATHER_CALL_MS);
      await this.signalOut('answer', pc.localDescription!.sdp);
    } else {
      await pc.setRemoteDescription({ type: 'answer', sdp: s.data });
    }
  }

  private peerUser() {
    return this.c.caller_device === this.me ? this.c.callee.user_id : this.c.caller.user_id;
  }

  applySettings() {
    const s = this.voice.settings;
    const user = this.peerUser();
    const muted = this.voice.state.deafened || s.muted.includes(user);
    this.player?.apply(((s.volumes[user] ?? 100) * s.volume) / 100, muted, s.output);
    this.sharePlayer?.apply(s.volume, this.voice.state.deafened, s.output);
    this.mic?.apply();
  }

  /// The camera on (in the camera's section, no new negotiation) or off.
  async setCamera(on: boolean) {
    const pc = await this.peer();
    const section = pc.getTransceivers().find((x) => x.mid === '1');
    if (!section) throw new Error('no camera section');
    if (!on) {
      await section.sender.replaceTrack(null);
      this.cam?.getTracks().forEach((x) => x.stop());
      this.cam = undefined;
      return;
    }
    if (this.cam) return;
    const stream = await navigator.mediaDevices.getUserMedia({
      video: {
        deviceId: this.voice.settings.camera ? { exact: this.voice.settings.camera } : undefined,
        width: { ideal: 1280 },
        height: { ideal: 720 },
        frameRate: { ideal: 30 },
      },
    });
    this.cam = stream;
    await section.sender.replaceTrack(stream.getVideoTracks()[0]);
    const params = section.sender.getParameters();
    if (params.encodings?.[0]) {
      params.encodings[0].maxBitrate = 1_200_000;
      await section.sender.setParameters(params).catch(() => undefined);
    }
  }

  cameraTrack() {
    return this.cam?.getVideoTracks()[0];
  }

  setSelf(muted: boolean, deafened: boolean) {
    if (this.mic) this.mic.allowed = !muted && !deafened;
    this.applySettings();
  }

  private keyDown = (e: KeyboardEvent) => {
    if (this.voice.settings.mode === 'ptt' && e.code === this.voice.settings.key && this.mic) this.mic.held = true;
  };
  private keyUp = (e: KeyboardEvent) => {
    if (e.code === this.voice.settings.key && this.mic) this.mic.held = false;
  };

  inputLevel(): number {
    return this.mic?.measure()[0] ?? 0;
  }

  tick() {
    const source = this.receiver?.getSynchronizationSources()[0];
    const talking = !!source && (source.audioLevel ?? 0) > 0.03;
    this.voice.set({ speaking: talking ? [1] : [], meSpeaking: !!this.mic?.open });
  }

  async close(tell: boolean) {
    this.closed = true;
    window.removeEventListener('keydown', this.keyDown);
    window.removeEventListener('keyup', this.keyUp);
    if (tell) await call('POST', `/api/v2/chat/calls/${this.id}/hangup`).catch(() => undefined);
    this.player?.stop();
    this.sharePlayer?.stop();
    this.cam?.getTracks().forEach((x) => x.stop());
    void this.pc?.then((pc) => pc.close());
    this.mic?.close();
  }
}

/// Records five seconds of the microphone and plays them back (`state`:
/// seconds left while recording, `playing`, then `idle`).
export async function testMicrophone(
  settings: VoiceSettings,
  state: (s: { recording?: number; playing?: boolean }) => void,
): Promise<void> {
  const mic = await Mic.open(() => ({ ...settings, mode: 'activity', threshold: -127 }));
  const chunks: Blob[] = [];
  const recorder = new MediaRecorder(new MediaStream([mic.track]));
  recorder.ondataavailable = (e) => chunks.push(e.data);
  const stopped = new Promise((resolve) => (recorder.onstop = resolve));
  recorder.start();
  const started = Date.now();
  const timer = window.setInterval(() => {
    state({ recording: Math.max(1, Math.ceil((TEST_MS - (Date.now() - started)) / 1000)) });
  }, 200);
  state({ recording: TEST_MS / 1000 });
  await new Promise((resolve) => window.setTimeout(resolve, TEST_MS));
  window.clearInterval(timer);
  recorder.stop();
  await stopped;
  mic.close();
  state({ playing: true });
  const audio = new Audio(URL.createObjectURL(new Blob(chunks, { type: recorder.mimeType })));
  audio.volume = Math.min(1, settings.volume / 100);
  const sink = audio as HTMLAudioElement & { setSinkId?(id: string): Promise<void> };
  if (settings.output && sink.setSinkId) await sink.setSinkId(settings.output).catch(() => undefined);
  await new Promise<void>((resolve) => {
    audio.onended = () => resolve();
    audio.onerror = () => resolve();
    void audio.play().catch(() => resolve());
  });
  URL.revokeObjectURL(audio.src);
  state({});
}

export const voice = new VoiceController();

export function useVoice(): VoiceState {
  return useSyncExternalStore(voice.subscribe, voice.get);
}
