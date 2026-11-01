// The browser viewer (`docs/web-console.md` §7.4, §7.7): the remote picture
// with scaling, full screen, picture-in-picture, display switching, sound,
// other participants' pointers, screenshot, statistics, participants,
// host events and automatic recovery; within the granted capabilities,
// keyboard and mouse control (special keys, pointer lock, touch), the text
// clipboard, files and the microphone.
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { t as messagesNow, type Messages } from '../i18n';
import { useT } from '../i18n/react';
import { argbCss, type AnnotationState } from '../lib/annotation';
import { ApiError, call, errorText, hasToken } from '../lib/api';
import { DEFAULT_DIR, droppedEntries, type TransferInfo } from '../lib/files';
import { capText, displayName, num } from '../lib/format';
import type {
  CameraInfo,
  CursorShape,
  HostEvent,
  Participant,
  ParticipantPointer,
  Screens,
  StreamState,
} from '../lib/fpp';
import { DisplayKind, HostEventKind } from '../lib/fpp';
import { SystemCommand, Usage } from '../lib/input';
import { PeerRequestError } from '../lib/peerio';
import { sendViewerReport, VisitSummary } from '../lib/record';
import { VideoCodec, type VideoCodecId } from '../lib/sdp';
import { mbps, tierRate } from '../lib/quality';
import { pathLabel, StatsSampler, type ViewStats } from '../lib/stats';
import {
  failureText,
  Viewer,
  type Admission,
  type ControlState,
  type Ending,
  type MicrophoneStatus,
  type ViewerState,
  WIRE_CODEC,
} from '../lib/viewer';
import { FilesPanel, TransferList } from './FilesPanel';
import { CameraMenu, orientationStyle } from './CameraMenu';
import { Menu, MenuItem } from './ViewerMenu';
import { Icon, type IconName } from './icons';
import { Button, Spinner } from './kit';
import { Dialog, useOverlay } from './overlay';
import { AnnotationLayer } from './AnnotationLayer';
import { useRemoteInput, type PictureMap } from './remoteInput';
import {
  PERF_HISTORY,
  PerformanceOverlay,
  PerformanceSummary,
  type PerfMode,
  type PerfSample,
} from './PerformanceOverlay';
import { RoomNotices } from './RoomNotices';

type Scale = 'fit' | 'fill' | 'original';
type Panel = 'none' | 'people' | 'files';

/// Take the control token when a session starts ('0': the user turned it off).
const CONTROL_KEY = 'farwing.viewer.control';
/// Automatic clipboard synchronization ('0': off).
const CLIPBOARD_KEY = 'farwing.viewer.clipboard';
/// Show this viewer's pointer to the host and the others ('0': off).
const POINTER_KEY = 'farwing.viewer.pointer';
/// 'mouse': touch acts as mouse gestures even when the host takes touch.
const TOUCH_KEY = 'farwing.viewer.touch';
/// The performance monitor: compact (the default), detailed or off.
const PERF_KEY = 'farwing.viewer.perf';
/// While a session is open the web login counts as in use: the server
/// expires idle web logins after `session_idle_minutes` (30 by default).
const KEEP_ALIVE_MS = 5 * 60 * 1000;

/// The host's cursor as a CSS cursor (absolute control draws it locally).
/// A hidden host cursor (no mouse on the host, or a program hid it) shows
/// the ordinary arrow: the pointer must stay visible to be aimed.
function cursorCss(shape: CursorShape): string {
  if (shape.hidden) return 'default';
  if (!shape.png.length || shape.width > 128 || shape.height > 128) return 'default';
  let binary = '';
  for (const byte of shape.png) binary += String.fromCharCode(byte);
  const x = Math.min(Math.max(shape.hotX, 0), shape.width - 1);
  const y = Math.min(Math.max(shape.hotY, 0), shape.height - 1);
  return `url(data:image/png;base64,${btoa(binary)}) ${x} ${y}, default`;
}

interface KeyboardLock {
  lock?(keys?: string[]): Promise<void>;
  unlock?(): void;
}

function keyboardLock(): KeyboardLock | undefined {
  return (navigator as unknown as { keyboard?: KeyboardLock }).keyboard;
}

const RETRY_DELAYS = [1, 2, 4, 8, 15];
const RETRY_WINDOW_MS = 5 * 60 * 1000;
/// Start failures worth retrying (the device may come back).
const RETRY_CODES = new Set(['device_offline', 'timeout', 'busy', 'unavailable', 'closed', 'error', 'network']);

type ViewerMessages = Messages['viewer'];

const QUALITIES: { id: number; key: keyof ViewerMessages['screen']['qualities'] }[] = [
  { id: 5, key: 'auto' },
  { id: 4, key: 'original' },
  { id: 3, key: 'ultra' },
  { id: 2, key: 'high' },
  { id: 1, key: 'smooth' },
];
/// Sharing the picture with others: 自动, 高清 or 流畅 (the forwarder's two
/// streams); a stored higher tier counts as 高清 meanwhile.
const SHARED_QUALITIES = [5, 2, 1];
/// The next lower tier to suggest when the picture stalls often.
const LOWER: Record<number, keyof ViewerMessages['screen']['qualities']> = { 4: 'ultra', 3: 'high', 2: 'smooth' };
/// Stalls: this many within this long (or one lasting 3 s) suggest a lower
/// tier, at most once per tier this often.
const STALLS = 3;
const STALLS_WITHIN_MS = 30_000;
const SUGGEST_AGAIN_MS = 300_000;

const CODECS: { id: VideoCodecId; label: string; mime: string }[] = [
  { id: VideoCodec.AV1, label: 'AV1', mime: 'AV1' },
  { id: VideoCodec.H265, label: 'H.265', mime: 'H265' },
  { id: VideoCodec.H264, label: 'H.264', mime: 'H264' },
];

/// The codec chosen in the menu, kept for later sessions; 0 is automatic.
const CODEC_KEY = 'farwing.viewer.codec';

function storedCodec(): number {
  const value = Number(localStorage.getItem(CODEC_KEY));
  return CODECS.some((c) => c.id === value) ? value : 0;
}

const INK_COLORS: { argb: number; key: keyof ViewerMessages['annotation']['colors'] }[] = [
  { argb: 0xffff4444, key: 'red' },
  { argb: 0xffffc83d, key: 'yellow' },
  { argb: 0xff3ccf7a, key: 'green' },
  { argb: 0xff4b88eb, key: 'blue' },
  { argb: 0xffffffff, key: 'white' },
  { argb: 0xff202020, key: 'black' },
];
const INK_WIDTHS: { width: number; key: keyof ViewerMessages['annotation']['widths'] }[] = [
  { width: 2, key: 'thin' },
  { width: 4, key: 'medium' },
  { width: 8, key: 'thick' },
];

const PALETTE = [0x4b88eb, 0x66cf9c, 0xeebe6f, 0xbe84f0, 0xf17d84, 0x5cc4d6];

function colorOf(p: Participant | undefined, id: string): string {
  const value = p?.color || PALETTE[Number(BigInt(id) % 6n)];
  return `#${value.toString(16).padStart(6, '0')}`;
}

/// A host event as a banner, from its kind.
function hostEventText(e: HostEvent): string {
  const m = messagesNow().viewer.hostEvents;
  switch (e.kind) {
    case HostEventKind.UPDATING:
      return m.updating;
    case HostEventKind.RESTARTING:
      return m.restarting;
    case HostEventKind.SLEEPING:
      return m.sleeping;
    case HostEventKind.SHUTTING_DOWN:
      return m.shuttingDown;
    case HostEventKind.SESSION_SWITCH:
      return m.sessionSwitch;
    case HostEventKind.DISPLAY_CHANGING:
      return e.actor ? m.displayChangedBy(e.actor.slice(0, 40)) : m.displayChanging;
    default:
      return m.changed;
  }
}

/// The picture's box inside the <video> element.
function contentBox(video: HTMLVideoElement, scale: Scale) {
  const ew = video.clientWidth;
  const eh = video.clientHeight;
  const vw = video.videoWidth || ew;
  const vh = video.videoHeight || eh;
  if (scale === 'original') return { x: 0, y: 0, w: ew, h: eh };
  const ratio = scale === 'fit' ? Math.min(ew / vw, eh / vh) : Math.max(ew / vw, eh / vh);
  const w = vw * ratio;
  const h = vh * ratio;
  return { x: (ew - w) / 2, y: (eh - h) / 2, w, h };
}

function ToolButton({
  icon,
  label,
  onClick,
  active,
  disabled,
  dim,
}: {
  icon: IconName;
  label: string;
  onClick?(): void;
  active?: boolean;
  disabled?: boolean;
  /// Shown as unavailable, still clickable.
  dim?: boolean;
}) {
  return (
    <button
      type="button"
      className={`tool ${active ? 'active' : ''} ${dim ? 'dim' : ''}`}
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
    >
      <Icon name={icon} size={18} />
    </button>
  );
}

export interface ViewerScreenProps {
  title: string;
  subtitle?: ReactNode;
  admit(): Promise<Admission>;
  /// The visit `admit` asks for: its sessions make one connection record.
  visit: string;
  onLeave(): void;
  /// Re-create lost sessions automatically (5 minutes).
  autoRetry?: boolean;
  /// The host's cameras only.
  camera?: boolean;
}

/// Ask the host for this picture: the tier (5 automatic), frame rate,
/// codec (0: the host chooses) and whether the cursor is drawn into it (off
/// while controlling with an absolute mouse: the cursor is local).
function sendCapture(v: Viewer, quality: number, fps: number, codec: number, cursor = true): Promise<void> {
  return v.setStream({
    quality: quality === 5 ? 0 : quality,
    fps,
    codec: codec ? WIRE_CODEC[codec as VideoCodecId] : 0,
    cursor,
  });
}

export function ViewerScreen({
  title,
  subtitle,
  admit,
  visit,
  onLeave,
  autoRetry = true,
  camera = false,
}: ViewerScreenProps) {
  const t = useT();
  const { toast } = useOverlay();
  const root = useRef<HTMLDivElement>(null);
  const stage = useRef<HTMLDivElement>(null);
  const video = useRef<HTMLVideoElement>(null);
  const viewer = useRef<Viewer | undefined>(undefined);
  const firstFailure = useRef(0);
  const retries = useRef(0);
  const sampler = useRef(new StatsSampler());
  const summary = useRef(new VisitSummary());
  const admitted = useRef<Admission | undefined>(undefined);
  const [generation, setGeneration] = useState(0);
  const [state, setState] = useState<ViewerState>('connecting');
  const [ending, setEnding] = useState<Ending>();
  const [retryAt, setRetryAt] = useState<number>();
  const [now, setNow] = useState(() => Date.now());
  const [screens, setScreens] = useState<Screens>();
  const [cameras, setCameras] = useState<CameraInfo[]>([]);
  const [cameraPip, setCameraPip] = useState<{ camera: number; stream?: MediaStream }>();
  const pipVideo = useRef<HTMLVideoElement>(null);
  const [streamState, setStreamState] = useState<StreamState>();
  const [grants, setGrants] = useState<string[]>([]);
  const [people, setPeople] = useState<{ participants: Participant[]; controlling: string }>();
  /// The picture comes through the server's forwarder in this mode.
  const [forwarding, setForwarding] = useState('');
  const [budgetKbps, setBudgetKbps] = useState(0);
  const [pointers, setPointers] = useState<ParticipantPointer[]>([]);
  const [banner, setBanner] = useState<string>();
  const [audioKbps, setAudioKbps] = useState<number>();
  const [stats, setStats] = useState<ViewStats>();
  const [scale, setScale] = useState<Scale>('fit');
  const [panel, setPanel] = useState<Panel>('none');
  const [perf, setPerfState] = useState<PerfMode>(() => {
    const stored = localStorage.getItem(PERF_KEY);
    return stored === 'detailed' || stored === 'off' ? stored : 'compact';
  });
  const setPerf = (mode: PerfMode) => {
    localStorage.setItem(PERF_KEY, mode);
    setPerfState(mode);
  };
  const [audioTrack, setAudioTrack] = useState<MediaStreamTrack>();
  const [perfHistory, setPerfHistory] = useState<PerfSample[]>([]);
  const playingSince = useRef<number>(undefined);
  const [uptime, setUptime] = useState(0);
  const [muted, setMuted] = useState(true);
  const [volume, setVolume] = useState(1);
  const [fullscreen, setFullscreen] = useState(false);
  const [barVisible, setBarVisible] = useState(true);
  const [videoSize, setVideoSize] = useState<[number, number]>([0, 0]);
  const [frameQuality, setFrameQuality] = useState(5);
  /// A suggestion shown with the room notices.
  const [hint, setHint] = useState<{ id: number; text: string }>();
  const stalls = useRef<number[]>([]);
  const suggested = useRef(new Map<number, number>());
  /// The tier in use, for the stall watch: a stored higher one counts as
  /// 高清 while the picture is shared.
  const tierNow = useRef(5);
  const [fps, setFps] = useState(60);
  const [sharePointer, setSharePointer] = useState(() => localStorage.getItem(POINTER_KEY) !== '0');
  // Refused for lack of the grant: offer to ask the host's user.
  const [askControl, setAskControl] = useState(false);
  const [codec, setCodec] = useState(storedCodec);
  const [codecs, setCodecs] = useState<VideoCodecId[]>();
  const pointerSent = useRef(0);
  // The capture setting last sent, re-sent with a stored codec choice once
  // the host's capability arrives.
  const capture = useRef({ quality: 5, fps: 60, codec: storedCodec(), cursor: true });
  const [control, setControl] = useState<ControlState>({ held: false, pending: false });
  const [relative, setRelative] = useState(false);
  const [cursor, setCursor] = useState<string>();
  const [mic, setMic] = useState<MicrophoneStatus>({ state: 'off' });
  const [transfers, setTransfers] = useState<TransferInfo[]>([]);
  const [clipboardAuto, setClipboardAuto] = useState(() => localStorage.getItem(CLIPBOARD_KEY) !== '0');
  /// Text the host copied that could not be written locally yet.
  const [pendingCopy, setPendingCopy] = useState<string>();
  const [typing, setTyping] = useState<string>();
  const [dragOver, setDragOver] = useState(false);
  const [keyboardLocked, setKeyboardLocked] = useState(false);
  const [ink, setInk] = useState<AnnotationState>({ available: false, enabled: false });
  const [inkColor, setInkColor] = useState(INK_COLORS[0].argb);
  const [inkWidth, setInkWidth] = useState(4);
  const [inkRevision, setInkRevision] = useState(0);
  const [hostFeatures, setHostFeatures] = useState<Record<string, number>>({});
  const [touchMouse, setTouchMouse] = useState(() => localStorage.getItem(TOUCH_KEY) === 'mouse');
  const autoControlTried = useRef(false);
  const pictureMap = useRef<PictureMap | undefined>(undefined);

  const audioAllowed = grants.includes('audio');
  const controlAllowed = grants.includes('control');
  const clipboardAllowed = grants.includes('clipboard');
  const filesAllowed = grants.includes('files');
  const micAllowed = grants.includes('microphone');
  const inkAllowed = grants.includes('annotation');
  /// Why something is unavailable: its permission was not granted.
  const missing = (cap: string) => t.viewer.screen.permissionMissing(capText(cap));

  // One session per generation; a timer keeps React's double effects in
  // development from opening two.
  useEffect(() => {
    let current: Viewer | undefined;
    const timer = window.setTimeout(() => {
      const v = new Viewer(
        {
          admit: async () => {
            const admission = await admit();
            admitted.current = admission;
            return admission;
          },
          codec: () => capture.current.codec,
          camera,
        },
        {
          state: (s) => {
            if (s === 'reconnecting') summary.current.reconnects += 1;
            setState(s);
            if (s === 'playing') {
              firstFailure.current = 0;
              retries.current = 0;
            }
          },
          ended: (e) => onEnded(e),
          stream: (stream) => {
            if (video.current) video.current.srcObject = stream;
            setAudioTrack(stream.getAudioTracks()[0]);
          },
          screens: setScreens,
          cameras: setCameras,
          pip: (stream) => setCameraPip((p) => (p && stream ? { ...p, stream } : undefined)),
          streamState: setStreamState,
          permissions: (g) => setGrants([...g]),
          participants: setPeople,
          forwarding: setForwarding,
          budget: setBudgetKbps,
          stalled: (long) => {
            if (!long) summary.current.stalls += 1;
            const at = Date.now();
            stalls.current = [...stalls.current.filter((t) => at - t < STALLS_WITHIN_MS), at];
            if (!long && stalls.current.length < STALLS) return;
            stalls.current = [];
            const tier = tierNow.current;
            const lower = LOWER[tier];
            if (!lower || at - (suggested.current.get(tier) ?? 0) < SUGGEST_AGAIN_MS) return;
            suggested.current.set(tier, at);
            const m = messagesNow().viewer.screen;
            setHint({ id: at, text: m.stallingTry(m.qualities[lower]) });
          },
          pointers: setPointers,
          hostEvent: (e) => setBanner(hostEventText(e)),
          audio: (a) => setAudioKbps(a.kbps),
          codecs: setCodecs,
          control: (c) => {
            setControl(c);
            if (c.message) setBanner(c.message);
          },
          cursor: (shape) => setCursor(cursorCss(shape)),
          microphone: (m) => {
            setMic(m);
            if (m.message) setBanner(m.message);
          },
          clipboardText: (text) => copyLocally(text),
          annotation: (a) => {
            setInk(a);
            if (a.error) setBanner(a.error);
          },
          hostFeatures: setHostFeatures,
          transfer: (info) => {
            setTransfers((list) => [...list.filter((x) => x.id !== info.id || x.direction !== info.direction), info]);
            if (info.state !== 'running') {
              window.setTimeout(
                () => setTransfers((list) => list.filter((x) => x.id !== info.id || x.direction !== info.direction)),
                5000,
              );
            }
          },
        },
      );
      current = v;
      viewer.current = v;
      v.setQualityChoice(capture.current.quality);
      // For testing from the developer tools of the dev server only.
      if (import.meta.env.DEV) (window as unknown as { farwingViewer?: Viewer }).farwingViewer = v;
      v.start().catch((error: unknown) => {
        if (v.state === 'ended') return;
        const code = error instanceof ApiError ? error.code : ((error as { code?: string })?.code ?? 'error');
        onEnded({
          reason: code,
          message: () => failureText(error),
          recoverable: RETRY_CODES.has(code) || error instanceof TypeError,
        });
      });
    }, 0);
    // Leaving the page ends the session at once; otherwise the host keeps
    // it open for a reconnection.
    const leave = () => current?.stop();
    window.addEventListener('pagehide', leave);
    return () => {
      window.removeEventListener('pagehide', leave);
      window.clearTimeout(timer);
      current?.stop();
      autoControlTried.current = false;
      setControl({ held: false, pending: false });
      setMic({ state: 'off' });
      setInk({ available: false, enabled: false });
      setRelative(false);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [generation]);

  const retryNow = useCallback(() => {
    if (summary.current.watched) summary.current.reconnects += 1;
    setRetryAt(undefined);
    setEnding(undefined);
    setBanner(undefined);
    setPointers([]);
    setGeneration((g) => g + 1);
  }, []);

  function onEnded(e: Ending) {
    setState('ended');
    setEnding(e);
    if (!autoRetry || !e.recoverable) return;
    const at = Date.now();
    if (!firstFailure.current) firstFailure.current = at;
    if (at - firstFailure.current > RETRY_WINDOW_MS) return;
    const delay = RETRY_DELAYS[Math.min(retries.current, RETRY_DELAYS.length - 1)] * 1000;
    retries.current += 1;
    setRetryAt(at + delay);
  }

  // Retry timer and countdown.
  useEffect(() => {
    if (!retryAt) return;
    const tick = window.setInterval(() => setNow(Date.now()), 250);
    const fire = window.setTimeout(retryNow, Math.max(0, retryAt - Date.now()));
    return () => {
      window.clearInterval(tick);
      window.clearTimeout(fire);
    };
  }, [retryAt, retryNow]);

  // Statistics once a second.
  useEffect(() => {
    const timer = window.setInterval(async () => {
      const report = await viewer.current?.peer?.getStats().catch(() => undefined);
      const forwarded = await viewer.current?.forwardPeer?.getStats().catch(() => undefined);
      if (!report) return;
      const sampled = sampler.current.sample(report, forwarded);
      if (viewer.current?.state === 'playing') {
        summary.current.add(sampled);
        playingSince.current ??= Date.now();
        setUptime((Date.now() - playingSince.current) / 1000);
        setPerfHistory((history) =>
          [...history, { fps: sampled.fps, rttMs: sampled.rttMs, mbps: sampled.bitrate / 1e6 }].slice(-PERF_HISTORY),
        );
      }
      setStats(sampled);
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);

  // The viewer's summary of the visit, when it leaves or the page closes.
  useEffect(() => {
    let sent = false;
    const send = () => {
      const admission = admitted.current;
      if (sent || !admission) return;
      sent = true;
      void sendViewerReport(visit, admission.hostId, summary.current, admission.bearer);
    };
    window.addEventListener('pagehide', send);
    return () => {
      window.removeEventListener('pagehide', send);
      send();
    };
  }, [visit]);

  // Keep the account login alive during the session (not past the
  // server's maximum login age). Quiet: an ended login must not tear the
  // session page down.
  const live = state === 'playing' || state === 'reconnecting';
  useEffect(() => {
    if (!live || !hasToken()) return;
    const timer = window.setInterval(() => {
      void call('GET', '/api/v2/me', undefined, { quiet: true }).catch(() => undefined);
    }, KEEP_ALIVE_MS);
    return () => window.clearInterval(timer);
  }, [live]);

  // Banners fade after a while.
  useEffect(() => {
    if (!banner) return;
    const timer = window.setTimeout(() => setBanner(undefined), 8000);
    return () => window.clearTimeout(timer);
  }, [banner]);

  // Full screen state and the toolbar that hides in it.
  useEffect(() => {
    const change = () => setFullscreen(!!document.fullscreenElement);
    document.addEventListener('fullscreenchange', change);
    return () => document.removeEventListener('fullscreenchange', change);
  }, []);
  const hideTimer = useRef(0);
  const showBar = useCallback(() => {
    setBarVisible(true);
    window.clearTimeout(hideTimer.current);
    hideTimer.current = window.setTimeout(() => setBarVisible(false), 3000);
  }, []);

  const toggleFullscreen = useCallback(() => {
    if (document.fullscreenElement) void document.exitFullscreen();
    else void root.current?.requestFullscreen().catch(() => undefined);
  }, []);

  const toggleMute = useCallback(() => {
    const v = video.current;
    if (!v) return;
    v.muted = !v.muted;
    setMuted(v.muted);
    if (!v.muted) void v.play().catch(() => undefined);
  }, []);

  const screenshot = useCallback(() => {
    const v = video.current;
    if (!v || !v.videoWidth) return;
    const canvas = document.createElement('canvas');
    canvas.width = v.videoWidth;
    canvas.height = v.videoHeight;
    canvas.getContext('2d')?.drawImage(v, 0, 0);
    canvas.toBlob((blob) => {
      if (!blob) return;
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      const stamp = new Date().toISOString().replace(/[:T]/g, '-').slice(0, 19);
      a.href = url;
      a.download = `${title}-${stamp}.png`.replace(/[\\/:*?"<>|]/g, '_');
      a.click();
      window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      toast(messagesNow().viewer.screen.screenshotSaved);
    }, 'image/png');
  }, [title, toast]);

  const pip = useCallback(() => {
    const v = video.current;
    if (!v) return;
    if (document.pictureInPictureElement) void document.exitPictureInPicture();
    else void v.requestPictureInPicture?.().catch(() => toast(messagesNow().viewer.screen.pipUnsupported, 'red'));
  }, [toast]);

  // Keyboard: F full screen, M sound, S screenshot (not while the keys go
  // to the host).
  useEffect(() => {
    const key = (e: KeyboardEvent) => {
      if (e.defaultPrevented || viewer.current?.controlling) return;
      if (e.ctrlKey || e.metaKey || e.altKey) return;
      const target = e.target as HTMLElement;
      if (target.closest('input, textarea, select')) return;
      if (e.key === 'f' || e.key === 'F') toggleFullscreen();
      else if ((e.key === 'm' || e.key === 'M') && audioAllowed) toggleMute();
      else if (e.key === 's' || e.key === 'S') screenshot();
      else return;
      e.preventDefault();
    };
    window.addEventListener('keydown', key);
    return () => window.removeEventListener('keydown', key);
  }, [toggleFullscreen, toggleMute, screenshot, audioAllowed]);

  useEffect(() => {
    if (video.current) video.current.volume = volume;
  }, [volume]);

  const applyCapture = (q: number, f: number, c = codec) => {
    setFrameQuality(q);
    setFps(f);
    setCodec(c);
    capture.current = { ...capture.current, quality: q, fps: f, codec: c };
    viewer.current?.setQualityChoice(q);
    if (c) localStorage.setItem(CODEC_KEY, String(c));
    else localStorage.removeItem(CODEC_KEY);
    if (viewer.current)
      sendCapture(viewer.current, q, f, c, capture.current.cursor).catch(() =>
        toast(messagesNow().viewer.screen.captureRejected, 'red'),
      );
  };

  const setAudio = (kbps: number) =>
    viewer.current?.setAudioKbps(kbps).catch((error: unknown) => {
      const m = messagesNow().viewer.screen;
      const reasons: Record<string, string> = m.audioReasons;
      toast((error instanceof PeerRequestError && reasons[error.reason]) || m.audioRejected, 'red');
    });

  // Control ------------------------------------------------------------------

  const playing0 = state === 'playing';
  // Take the token once the picture plays, unless the user turned control
  // off; automatic: refused quietly when others are in the session.
  useEffect(() => {
    if (!playing0 || !controlAllowed || autoControlTried.current) return;
    if (localStorage.getItem(CONTROL_KEY) === '0') return;
    autoControlTried.current = true;
    void viewer.current?.takeControl(true);
  }, [playing0, controlAllowed]);

  const toggleControl = async () => {
    const v = viewer.current;
    if (!v) return;
    if (v.controlling) {
      localStorage.setItem(CONTROL_KEY, '0');
      setRelative(false);
      await v.releaseControl();
      return;
    }
    const result = await v.takeControl(false);
    if (result.held) {
      localStorage.removeItem(CONTROL_KEY);
      stage.current?.focus({ preventScroll: true });
    } else if (result.canAsk) setAskControl(true);
  };

  // Ask the host's user for control in this connection.
  const askForControl = async () => {
    setAskControl(false);
    const result = await viewer.current?.takeControl(false, true);
    if (result?.held) {
      localStorage.removeItem(CONTROL_KEY);
      stage.current?.focus({ preventScroll: true });
    }
  };

  // Drawing takes the pointer: no input goes to the host meanwhile.
  const annotating = ink.enabled && inkAllowed;
  // The host draws the cursor into the picture unless we draw it.
  const cursorInPicture = !(control.held && !relative && !annotating);
  useEffect(() => {
    const v = viewer.current;
    if (!v || !playing0 || capture.current.cursor === cursorInPicture) return;
    capture.current = { ...capture.current, cursor: cursorInPicture };
    const c = capture.current;
    void sendCapture(v, c.quality, c.fps, c.codec, c.cursor).catch(() => undefined);
  }, [cursorInPicture, playing0]);

  // Keyboard Lock in full screen: Win, Alt+Tab and Esc go to the host.
  useEffect(() => {
    const kb = keyboardLock();
    if (!kb?.lock) return;
    if (fullscreen && control.held) {
      kb.lock()
        .then(() => setKeyboardLocked(true))
        .catch(() => setKeyboardLocked(false));
    } else {
      kb.unlock?.();
      setKeyboardLocked(false);
    }
  }, [fullscreen, control.held]);

  const onUnlocked = useCallback(() => setRelative(false), []);
  useRemoteInput({
    stage,
    video,
    viewer,
    map: pictureMap,
    active: control.held && playing0 && !annotating,
    relative,
    onUnlocked,
    touchNative: !!hostFeatures.touch && !touchMouse,
    penNative: !!hostFeatures.pen,
  });

  const special = (run: (v: Viewer) => void) => {
    const v = viewer.current;
    if (!v?.controlling) return;
    run(v);
    stage.current?.focus({ preventScroll: true });
  };

  // Clipboard ------------------------------------------------------------------

  function copyLocally(text: string) {
    if (!document.hasFocus()) {
      setPendingCopy(text);
      return;
    }
    navigator.clipboard
      .writeText(text)
      .then(() => setPendingCopy(undefined))
      .catch(() => setPendingCopy(text));
  }

  useEffect(() => {
    if (viewer.current) viewer.current.clipboard.auto = clipboardAuto;
  }, [clipboardAuto, generation]);

  // Offer the local clipboard when the page comes back into focus, and
  // write what the host copied meanwhile.
  useEffect(() => {
    if (!clipboardAllowed || !clipboardAuto || !control.held) return;
    const sync = () => {
      if (!document.hasFocus()) return;
      if (pendingCopy !== undefined) copyLocally(pendingCopy);
      navigator.clipboard
        ?.readText()
        .then((text) => viewer.current?.clipboard.offer(text))
        .catch(() => undefined);
    };
    sync();
    window.addEventListener('focus', sync);
    return () => window.removeEventListener('focus', sync);
  }, [clipboardAllowed, clipboardAuto, control.held, pendingCopy]);

  const sendClipboard = async () => {
    try {
      const text = await navigator.clipboard.readText();
      if (!text) toast(t.viewer.screen.localClipboardEmpty, 'red');
      else {
        viewer.current?.clipboard.offer(text);
        toast(t.viewer.screen.sentToHost);
      }
    } catch {
      toast(t.viewer.screen.clipboardDenied, 'red');
    }
  };

  const pullClipboard = async () => {
    try {
      const text = await viewer.current?.clipboard.pull();
      if (!text) toast(t.viewer.screen.hostClipboardEmpty, 'red');
      else {
        copyLocally(text);
        toast(t.viewer.screen.copiedLocally);
      }
    } catch (error) {
      const reasons: Record<string, string> = t.viewer.screen.clipboardReasons;
      toast(
        error instanceof PeerRequestError
          ? (reasons[error.reason] ?? t.viewer.screen.clipboardFailed)
          : errorText(error),
        'red',
      );
    }
  };

  /// Type text through the clipboard: offer it, then Ctrl+V.
  const typeText = (text: string) => {
    const v = viewer.current;
    if (!v?.controlling || !text) return;
    v.clipboard.offer(text);
    window.setTimeout(() => v.input.chord([Usage.CTRL, Usage.V]), 150);
  };

  // Annotation ------------------------------------------------------------------

  const toggleInk = async () => {
    const a = viewer.current?.annotation;
    if (!a) return;
    if (!(await a.setEnabled(!a.active)) && !ink.error) toast(t.viewer.annotation.notOn, 'red');
    setInkRevision((n) => n + 1);
  };
  const inkAction = (run: () => void) => {
    run();
    setInkRevision((n) => n + 1);
  };

  // Microphone and files --------------------------------------------------------

  const toggleMic = () => {
    const v = viewer.current;
    if (!v) return;
    if (mic.state === 'off') v.startMicrophone().catch((error: Error) => toast(errorText(error), 'red'));
    else void v.stopMicrophone();
  };

  const uploadDropped = async (data: DataTransfer) => {
    const v = viewer.current;
    if (!v) return;
    try {
      // Folders arrive with their whole trees.
      const { files, folders } = await droppedEntries(data);
      if (files.length) await v.files.uploadFiles(files, DEFAULT_DIR);
      for (const group of folders) await v.files.upload(group.entries, DEFAULT_DIR, group.folder);
      if (files.length || folders.length) toast(t.viewer.screen.uploadedToDownloads);
    } catch (error) {
      toast(errorText(error), 'red');
    }
  };
  const usedCodec = CODECS.find((c) => c.mime === stats?.codec?.toUpperCase());
  // A stored choice this host cannot encode counts as automatic.
  const chosenCodec = codecs?.includes(codec as VideoCodecId) ? codec : 0;

  const current = screens?.screens.find((s) => s.id === screens.current);
  // The screen in video_0: what input positions refer to (a camera takes no input).
  const shown = screens?.screens.find((s) => s.track === 0) ?? current;
  const showsCamera = shown?.kind === DisplayKind.CAMERA;
  pictureMap.current =
    shown && !showsCamera
      ? {
          screenId: shown.id,
          width: shown.width,
          height: shown.height,
          box: () => (video.current?.videoWidth ? contentBox(video.current, scale) : undefined),
        }
      : undefined;
  const byId = useMemo(() => new Map((people?.participants ?? []).map((p) => [p.id, p])), [people]);
  const visiblePointers = pointers.filter(
    (p) => p.visible && p.idleMs < 3000 && (!current || p.screenId === current.id),
  );
  const box = video.current && videoSize[0] ? contentBox(video.current, scale) : undefined;
  const playing = state === 'playing';
  // Panels about the session close with it.
  useEffect(() => {
    if (state === 'ended') setPanel('none');
  }, [state]);
  const overlayText = ending ? ending.message() : state === 'playing' ? '' : t.viewer.states[state];
  const others = (people?.participants ?? []).filter((p) => !p.you);
  // Several watching and another one (or nobody) in control: one shared
  // picture, 高清 or 流畅.
  const me = people?.participants.find((p) => p.you)?.id;
  const sharedView = (people?.participants.filter((p) => !p.listening).length ?? 0) > 1 && people?.controlling !== me;
  const shownQuality = sharedView && (frameQuality === 4 || frameQuality === 3) ? 2 : frameQuality;
  const tierName = (id: number) => {
    const tier = QUALITIES.find((q) => q.id === id);
    return tier ? t.viewer.screen.qualities[tier.key] : '';
  };
  // Automatic names the tier the host reports.
  const reportedTier = streamState?.tracks.find((track) => track.track === 0)?.quality ?? 0;
  const qualityText =
    shownQuality === 5 && reportedTier >= 1 && reportedTier <= 4
      ? t.viewer.stats.autoWith(tierName(reportedTier))
      : tierName(shownQuality);
  tierNow.current = shownQuality;
  const sent = streamState?.tracks.find((track) => track.track === 0);
  const encoderText = sent ? `${sent.encoder || '—'} · ${sent.capture || '—'}` : '—';
  const controller = people?.participants.find((p) => p.id === people.controlling);

  // Our indicator pointer and clicks, when shared: normalized to the picture.
  const onPointer = (e: React.PointerEvent<HTMLDivElement>, visible: boolean, phase?: number) => {
    showBar();
    if (control.held || !sharePointer || !video.current || !current || !box) return;
    // Wire buttons: 1 left, 2 right, 3 middle.
    const button = ({ 0: 1, 1: 3, 2: 2 } as Record<number, number>)[e.button];
    const click = phase && button ? { button, phase } : undefined;
    const stamp = performance.now();
    if (visible && !click && stamp - pointerSent.current < 30) return;
    pointerSent.current = stamp;
    const rect = video.current.getBoundingClientRect();
    const x = (e.clientX - rect.left - box.x) / box.w;
    const y = (e.clientY - rect.top - box.y) / box.h;
    const inside = x >= 0 && x <= 1 && y >= 0 && y <= 1;
    if (click && !inside) return;
    viewer.current?.sendPointer(
      current.id,
      Math.min(1, Math.max(0, x)),
      Math.min(1, Math.max(0, y)),
      visible && inside,
      click,
    );
  };

  return (
    <div ref={root} className={`viewer ${fullscreen && !barVisible ? 'bar-hidden' : ''}`} onMouseMove={showBar}>
      <header className="viewer-bar">
        <button type="button" className="tool" title={t.viewer.back} onClick={onLeave}>
          <Icon name="back" size={18} />
        </button>
        <div className="viewer-title">
          <div className="ellipsis">{title}</div>
          <div className="small muted ellipsis">
            {playing && stats
              ? `${stats.width}×${stats.height} · ${num(stats.fps)} fps · ${num(stats.rttMs)} ms · ${pathLabel(stats, forwarding)}`
              : subtitle}
          </div>
        </div>
        {/* Open menus close when the picture stops (their contents no longer apply). */}
        <div className="viewer-tools" key={playing ? 'playing' : 'stopped'}>
          {screens && screens.screens.length > 1 && (
            <Menu
              label={t.viewer.screen.display}
              icon="monitor"
              text={
                current?.name ||
                t.viewer.screen.displayN(screens.screens.findIndex((s) => s.id === screens.current) + 1)
              }
            >
              {screens.screens.map((s, i) => (
                <MenuItem
                  key={s.id}
                  selected={s.id === screens.current}
                  hint={`${s.width}×${s.height}`}
                  onClick={() => viewer.current?.switchScreen(s.id)}
                >
                  {s.name || t.viewer.screen.displayN(i + 1)}
                  {s.primary ? t.viewer.screen.primary : ''}
                </MenuItem>
              ))}
            </Menu>
          )}
          {!showsCamera && (
            <ToolButton
              icon="mouse"
              label={
                control.held
                  ? t.viewer.control.stop
                  : control.pending
                    ? control.asking
                      ? t.viewer.control.asking
                      : t.viewer.control.pending
                    : !controlAllowed
                      ? t.viewer.control.noPermission
                      : controller && !controller.you
                        ? t.viewer.control.otherControlling(controller.nickname || t.viewer.control.otherParticipant)
                        : t.viewer.control.start
              }
              active={control.held}
              dim={!controlAllowed && !control.held}
              disabled={!playing || control.pending}
              onClick={() => void toggleControl()}
            />
          )}
          {cameras.length > 0 && (
            <CameraMenu
              viewer={viewer.current}
              cameras={cameras}
              shown={showsCamera ? shown?.id : undefined}
              pip={cameraPip?.camera}
              onPip={(id) => {
                setCameraPip(id === undefined ? undefined : { camera: id });
                void viewer.current?.pictureInPicture(id);
              }}
            />
          )}
          {control.held && (
            <Menu label={t.viewer.screen.keys} icon="keyboard">
              <div className="menu-label">{t.viewer.screen.mouse}</div>
              <MenuItem selected={!relative} onClick={() => setRelative(false)}>
                {t.viewer.screen.absolute}
              </MenuItem>
              <MenuItem
                selected={relative}
                onClick={() => {
                  setRelative(true);
                  void stage.current?.requestPointerLock?.();
                }}
              >
                {t.viewer.screen.relative}
              </MenuItem>
              {!!hostFeatures.touch && (
                <>
                  <div className="menu-label">{t.viewer.screen.touch}</div>
                  <MenuItem
                    selected={!touchMouse}
                    onClick={() => {
                      localStorage.removeItem(TOUCH_KEY);
                      setTouchMouse(false);
                    }}
                  >
                    {t.viewer.screen.nativeTouch}
                  </MenuItem>
                  <MenuItem
                    selected={touchMouse}
                    onClick={() => {
                      localStorage.setItem(TOUCH_KEY, 'mouse');
                      setTouchMouse(true);
                    }}
                  >
                    {t.viewer.screen.emulateMouse}
                  </MenuItem>
                </>
              )}
              <div className="menu-label">{t.viewer.screen.shortcuts}</div>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.SAS))}>Ctrl+Alt+Del</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.WIN]))}>Win</MenuItem>
              <MenuItem hint={t.viewer.screen.run} onClick={() => special((v) => v.input.chord([Usage.WIN, Usage.R]))}>
                Win+R
              </MenuItem>
              <MenuItem
                hint={t.viewer.screen.explorer}
                onClick={() => special((v) => v.input.chord([Usage.WIN, Usage.E]))}
              >
                Win+E
              </MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.ALT, Usage.TAB]))}>Alt+Tab</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.ALT, Usage.F4]))}>Alt+F4</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.ESC]))}>Esc</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.PRINT_SCREEN]))}>PrintScreen</MenuItem>
              <div className="menu-label">{t.viewer.screen.system}</div>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.LOCK))}>
                {t.viewer.screen.lock}
              </MenuItem>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.TASK_MANAGER))}>
                {t.viewer.screen.taskManager}
              </MenuItem>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.SHOW_DESKTOP))}>
                {t.viewer.screen.showDesktop}
              </MenuItem>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.TASK_VIEW))}>
                {t.viewer.screen.taskView}
              </MenuItem>
              {clipboardAllowed && (
                <>
                  <div className="menu-label">{t.viewer.screen.text}</div>
                  <MenuItem onClick={() => setTyping('')}>{t.viewer.screen.typeText}</MenuItem>
                </>
              )}
            </Menu>
          )}
          {!inkAllowed && ink.available && !showsCamera && (
            <ToolButton icon="edit" label={missing('annotation')} disabled />
          )}
          {inkAllowed && ink.available && !showsCamera && (
            <Menu label={t.viewer.annotation.title} icon="edit">
              <MenuItem selected={ink.enabled} onClick={() => void toggleInk()}>
                {t.viewer.annotation.turnOn}
              </MenuItem>
              {ink.enabled && shown && (
                <>
                  <div className="menu-label">{t.viewer.annotation.colorWidth}</div>
                  <div className="ink-swatches" onClick={(e) => e.stopPropagation()}>
                    {INK_COLORS.map((c) => (
                      <button
                        key={c.argb}
                        type="button"
                        className={`ink-swatch ${inkColor === c.argb ? 'selected' : ''}`}
                        style={{ background: argbCss(c.argb) }}
                        title={t.viewer.annotation.colors[c.key]}
                        aria-label={t.viewer.annotation.colors[c.key]}
                        onClick={() => setInkColor(c.argb)}
                      />
                    ))}
                  </div>
                  <div className="ink-swatches" onClick={(e) => e.stopPropagation()}>
                    {INK_WIDTHS.map((w) => (
                      <button
                        key={w.width}
                        type="button"
                        className={`ink-width ${inkWidth === w.width ? 'selected' : ''}`}
                        onClick={() => setInkWidth(w.width)}
                      >
                        {t.viewer.annotation.widths[w.key]}
                      </button>
                    ))}
                  </div>
                  <div className="menu-label">{t.viewer.annotation.edit}</div>
                  <MenuItem
                    onClick={() => inkAction(() => viewer.current?.annotation.undo())}
                    hint={
                      inkRevision >= 0 && viewer.current?.annotation.canUndo ? undefined : t.viewer.annotation.noStrokes
                    }
                  >
                    {t.viewer.annotation.undo}
                  </MenuItem>
                  <MenuItem onClick={() => inkAction(() => viewer.current?.annotation.clear())}>
                    {t.viewer.annotation.clear}
                  </MenuItem>
                  <MenuItem
                    selected={!!viewer.current?.annotation.hasBoard(shown.id)}
                    onClick={() =>
                      inkAction(() => {
                        const a = viewer.current?.annotation;
                        a?.setBoard(shown.id, !a.hasBoard(shown.id));
                      })
                    }
                  >
                    {t.viewer.annotation.whiteboard}
                  </MenuItem>
                </>
              )}
            </Menu>
          )}
          {!clipboardAllowed && <ToolButton icon="clipboard" label={missing('clipboard')} disabled />}
          {clipboardAllowed && (
            <Menu label={t.viewer.screen.clipboard} icon="clipboard">
              {/* Writing the host clipboard is control: it also needs that grant. */}
              <MenuItem
                selected={clipboardAuto && controlAllowed}
                disabled={!controlAllowed}
                title={controlAllowed ? undefined : missing('control')}
                onClick={() => {
                  localStorage.setItem(CLIPBOARD_KEY, clipboardAuto ? '0' : '1');
                  setClipboardAuto(!clipboardAuto);
                }}
              >
                {t.viewer.screen.autoSync}
              </MenuItem>
              <MenuItem
                disabled={!controlAllowed}
                title={controlAllowed ? undefined : missing('control')}
                onClick={() => void sendClipboard()}
              >
                {t.viewer.screen.sendClipboard}
              </MenuItem>
              <MenuItem onClick={() => void pullClipboard()}>{t.viewer.screen.pullClipboard}</MenuItem>
            </Menu>
          )}
          {!filesAllowed && <ToolButton icon="folder" label={missing('files')} disabled />}
          {filesAllowed && (
            <ToolButton
              icon="folder"
              label={t.viewer.screen.files}
              active={panel === 'files'}
              disabled={!playing}
              onClick={() => setPanel(panel === 'files' ? 'none' : 'files')}
            />
          )}
          {!micAllowed && <ToolButton icon="micOff" label={missing('microphone')} disabled />}
          {micAllowed && (
            <ToolButton
              icon={mic.state === 'off' ? 'micOff' : 'mic'}
              label={
                mic.state === 'off'
                  ? t.viewer.microphone.turnOn
                  : mic.state === 'starting'
                    ? t.viewer.microphone.starting
                    : mic.state === 'waiting'
                      ? t.viewer.microphone.waiting
                      : t.viewer.microphone.turnOff
              }
              active={mic.state === 'on'}
              disabled={!playing}
              onClick={toggleMic}
            />
          )}
          <Menu label={t.viewer.screen.scale} icon="fit">
            <MenuItem selected={scale === 'fit'} onClick={() => setScale('fit')}>
              {t.viewer.screen.fit}
            </MenuItem>
            <MenuItem selected={scale === 'fill'} onClick={() => setScale('fill')}>
              {t.viewer.screen.fill}
            </MenuItem>
            <MenuItem selected={scale === 'original'} onClick={() => setScale('original')}>
              {t.viewer.screen.original}
            </MenuItem>
          </Menu>
          <Menu label={t.viewer.screen.quality} icon="settings">
            <div className="menu-label">{t.viewer.screen.quality}</div>
            {QUALITIES.filter((q) => !sharedView || SHARED_QUALITIES.includes(q.id)).map((q) => (
              <MenuItem
                key={q.id}
                selected={shownQuality === q.id}
                hint={
                  // The tier's ceiling for this screen at the chosen frame
                  // rate (the host caps it at the screen's refresh rate).
                  q.id <= 4 && current
                    ? mbps(tierRate(q.id, current.width, current.height, Math.min(fps, current.fps || fps)))
                    : undefined
                }
                onClick={() => applyCapture(q.id, fps)}
              >
                {t.viewer.screen.qualities[q.key]}
              </MenuItem>
            ))}
            <div className="menu-label">{t.viewer.screen.frameRate}</div>
            {[30, 60].map((f) => (
              <MenuItem key={f} selected={fps === f} onClick={() => applyCapture(frameQuality, f)}>
                {f} FPS
              </MenuItem>
            ))}
            {codecs && codecs.length > 1 && (
              <>
                <div className="menu-label">{t.viewer.screen.codec}</div>
                <MenuItem
                  selected={!chosenCodec}
                  hint={!chosenCodec && usedCodec ? usedCodec.label : undefined}
                  onClick={() => applyCapture(frameQuality, fps, 0)}
                >
                  {t.viewer.screen.auto}
                </MenuItem>
                {CODECS.filter((c) => codecs.includes(c.id)).map((c) => (
                  <MenuItem
                    key={c.id}
                    selected={chosenCodec === c.id}
                    onClick={() => applyCapture(frameQuality, fps, c.id)}
                  >
                    {c.label}
                  </MenuItem>
                ))}
              </>
            )}
            {audioAllowed && (
              <>
                <div className="menu-label">{t.viewer.screen.audioQuality}</div>
                {[64, 128, 192, 256].map((k) => (
                  <MenuItem key={k} selected={audioKbps === k} onClick={() => setAudio(k)}>
                    {k} kbps
                  </MenuItem>
                ))}
              </>
            )}
            <div className="menu-label">{t.viewer.screen.other}</div>
            <MenuItem
              selected={sharePointer}
              onClick={() => {
                localStorage.setItem(POINTER_KEY, sharePointer ? '0' : '1');
                setSharePointer(!sharePointer);
              }}
            >
              {t.viewer.screen.sharePointer}
            </MenuItem>
          </Menu>
          <div className="volume">
            <ToolButton
              icon={muted || !audioAllowed ? 'mute' : 'volume'}
              label={audioAllowed ? (muted ? t.viewer.screen.unmute : t.viewer.screen.mute) : t.viewer.screen.noAudio}
              onClick={toggleMute}
              disabled={!audioAllowed}
            />
            {audioAllowed && !muted && (
              <input
                type="range"
                name="volume"
                min={0}
                max={1}
                step={0.05}
                value={volume}
                onChange={(e) => setVolume(Number(e.target.value))}
                aria-label={t.viewer.screen.volume}
              />
            )}
          </div>
          <ToolButton icon="camera" label={t.viewer.screen.screenshot} onClick={screenshot} disabled={!playing} />
          <ToolButton icon="pip" label={t.viewer.screen.pip} onClick={pip} disabled={!playing} />
          <Menu label={t.viewer.screen.stats} icon="chart" active={perf !== 'off'}>
            <div className="menu-label">{t.viewer.screen.stats}</div>
            {(['compact', 'detailed', 'off'] as const).map((mode) => (
              <MenuItem key={mode} selected={perf === mode} onClick={() => setPerf(mode)}>
                {t.viewer.stats[mode]}
              </MenuItem>
            ))}
          </Menu>
          <ToolButton
            icon="users"
            label={t.viewer.screen.people}
            active={panel === 'people'}
            onClick={() => setPanel(panel === 'people' ? 'none' : 'people')}
          />
          <ToolButton
            icon={fullscreen ? 'exitFullscreen' : 'fullscreen'}
            label={fullscreen ? t.viewer.screen.exitFullscreen : t.viewer.screen.fullscreen}
            onClick={toggleFullscreen}
          />
        </div>
      </header>

      <div
        ref={stage}
        tabIndex={0}
        className={`stage stage-${scale} ${control.held ? 'controlling' : ''} ${dragOver ? 'drag-over' : ''}`}
        style={control.held && !relative && cursor ? { cursor } : undefined}
        onDoubleClick={() => !control.held && toggleFullscreen()}
        onPointerMove={(e) => onPointer(e, true)}
        onPointerDown={(e) => onPointer(e, true, 1)}
        onPointerUp={(e) => onPointer(e, true, 2)}
        onPointerLeave={(e) => onPointer(e, false)}
        onDragOver={(e) => {
          if (!filesAllowed || !playing || !e.dataTransfer.types.includes('Files')) return;
          e.preventDefault();
          setDragOver(true);
        }}
        onDragLeave={() => setDragOver(false)}
        onDrop={(e) => {
          if (!filesAllowed || !e.dataTransfer.items.length) return;
          e.preventDefault();
          setDragOver(false);
          void uploadDropped(e.dataTransfer);
        }}
      >
        <div className="stage-inner">
          <video
            ref={video}
            autoPlay
            playsInline
            muted
            onResize={(e) => setVideoSize([e.currentTarget.videoWidth, e.currentTarget.videoHeight])}
            style={{
              ...(scale === 'original' && videoSize[0]
                ? { width: videoSize[0] / devicePixelRatio, height: videoSize[1] / devicePixelRatio }
                : undefined),
              ...(showsCamera && shown ? orientationStyle(shown.mirror, shown.quarterTurns) : undefined),
            }}
          />
          {cameraPip?.stream && (
            <div className="pip">
              <video
                ref={(element) => {
                  pipVideo.current = element;
                  if (element && element.srcObject !== cameraPip.stream) element.srcObject = cameraPip.stream ?? null;
                }}
                autoPlay
                playsInline
                muted
                style={(() => {
                  const s = screens?.screens.find((x) => x.id === cameraPip.camera);
                  return s ? orientationStyle(s.mirror, s.quarterTurns) : undefined;
                })()}
              />
              <button
                type="button"
                className="tool pip-close"
                title={t.viewer.camera.closePip}
                aria-label={t.viewer.camera.closePip}
                onClick={() => {
                  setCameraPip(undefined);
                  void viewer.current?.pictureInPicture(undefined);
                }}
              >
                <Icon name="close" size={14} />
              </button>
            </div>
          )}
          {box && playing && annotating && shown && viewer.current && (
            <AnnotationLayer
              annotation={viewer.current.annotation}
              box={box}
              screenId={shown.id}
              screenWidth={shown.width}
              color={inkColor}
              width={inkWidth}
            />
          )}
          {box && playing && (
            <div className="pointers" style={{ left: box.x, top: box.y, width: box.w, height: box.h }}>
              {visiblePointers.map((p) => {
                const who = byId.get(p.id);
                const color = colorOf(who, p.id);
                return (
                  <div key={p.id} className="pointer" style={{ left: `${p.x * 100}%`, top: `${p.y * 100}%` }}>
                    <svg width="18" height="18" viewBox="0 0 24 24">
                      <path d="M4 2l16 9-7 2-3 7z" fill={color} stroke="#000" strokeWidth="1.5" />
                    </svg>
                    <span style={{ background: color }}>
                      {who ? displayName({ nickname: who.nickname }) : t.viewer.screen.participant}
                    </span>
                  </div>
                );
              })}
            </div>
          )}
        </div>

        {!playing && (
          <div className="stage-overlay">
            {ending ? (
              <div className="stack" style={{ alignItems: 'center' }}>
                <Icon name={ending.recoverable ? 'refresh' : 'alert'} size={30} />
                <div className="overlay-title">{overlayText}</div>
                {retryAt ? (
                  <div className="muted">{t.viewer.ended.retryIn(Math.max(0, Math.ceil((retryAt - now) / 1000)))}</div>
                ) : ending.recoverable && autoRetry ? (
                  <div className="muted">{t.viewer.ended.gaveUp}</div>
                ) : null}
                <div className="row gap-sm">
                  {(ending.recoverable || !autoRetry) && (
                    <Button variant="primary" icon="refresh" onClick={retryNow}>
                      {t.viewer.ended.retryNow}
                    </Button>
                  )}
                  <Button onClick={onLeave}>{t.viewer.back}</Button>
                </div>
              </div>
            ) : (
              <div className="stack" style={{ alignItems: 'center' }}>
                <Spinner size={28} />
                <div className="overlay-title">{overlayText}</div>
              </div>
            )}
          </div>
        )}

        {playing && audioAllowed && muted && (
          <button type="button" className="unmute-hint" onClick={toggleMute}>
            <Icon name="volume" size={16} /> {t.viewer.screen.clickToUnmute}
          </button>
        )}
        {banner && <div className="viewer-banner">{banner}</div>}
        {playing && <RoomNotices people={people} forwarding={forwarding} hint={hint} t={t} />}
        {keyboardLocked && barVisible && <div className="viewer-hint">{t.viewer.screen.keyboardLocked}</div>}
        {pendingCopy !== undefined && (
          <button type="button" className="unmute-hint copy-hint" onClick={() => copyLocally(pendingCopy)}>
            <Icon name="clipboard" size={16} /> {t.viewer.screen.copyHostText}
          </button>
        )}
        {dragOver && <div className="drop-hint">{t.viewer.screen.dropHint}</div>}
        {perf === 'compact' && playing && (
          <PerformanceSummary
            stats={stats}
            forwarding={forwarding}
            uptime={uptime}
            quality={qualityText}
            audio={audioAllowed ? audioTrack : undefined}
            muted={muted}
            volume={volume}
          />
        )}
        {perf === 'detailed' && playing && (
          <PerformanceOverlay
            stats={stats}
            history={perfHistory}
            forwarding={forwarding}
            budgetKbps={budgetKbps}
            uptime={uptime}
            encoder={encoderText}
            audioAllowed={audioAllowed}
            audioKbps={audioKbps}
            grants={grants}
            onClose={() => setPerf('off')}
          />
        )}
        {panel !== 'files' && transfers.length > 0 && (
          <div className="transfers-float">
            <TransferList transfers={transfers} files={viewer.current?.files} />
          </div>
        )}
      </div>

      <Dialog
        open={askControl}
        title={t.viewer.control.noPermission}
        onClose={() => setAskControl(false)}
        footer={
          <>
            <Button onClick={() => setAskControl(false)}>{t.common.cancel}</Button>
            <Button variant="primary" onClick={() => void askForControl()}>
              {t.viewer.control.ask}
            </Button>
          </>
        }
      >
        {null}
      </Dialog>

      <Dialog
        open={typing !== undefined}
        title={t.viewer.screen.typeTitle}
        onClose={() => setTyping(undefined)}
        footer={
          <>
            <Button onClick={() => setTyping(undefined)}>{t.common.cancel}</Button>
            <Button
              variant="primary"
              disabled={!typing}
              onClick={() => {
                const text = typing ?? '';
                setTyping(undefined);
                typeText(text);
              }}
            >
              {t.viewer.screen.send}
            </Button>
          </>
        }
      >
        <textarea className="input" rows={5} value={typing ?? ''} onChange={(e) => setTyping(e.target.value)} />
      </Dialog>

      {panel !== 'none' && (
        <aside className="viewer-panel">
          <header className="row">
            <h2 className="grow">
              {panel === 'files' ? t.viewer.screen.hostFiles : t.viewer.screen.people}
              {panel === 'people' && <span className="card-count">{people?.participants.length ?? 1}</span>}
            </h2>
            <button type="button" className="tool" onClick={() => setPanel('none')} aria-label={t.common.close}>
              <Icon name="close" size={16} />
            </button>
          </header>
          {panel === 'files' ? (
            <FilesPanel files={viewer.current?.files} transfers={transfers} toast={toast} />
          ) : (
            <div className="list">
              {forwarding && (
                <div className="list-item small muted">
                  {forwarding === 'e2ee' ? t.viewer.screen.forwardedE2ee : t.viewer.screen.forwardedCompatible}
                </div>
              )}
              {(people?.participants ?? []).map((p) => (
                <div key={p.id} className="list-item">
                  <span className="dot-color" style={{ background: colorOf(p, p.id) }} />
                  <div className="grow">
                    <div>
                      {p.nickname || t.common.guest}
                      {p.you && <span className="muted">{t.viewer.screen.me}</span>}
                    </div>
                    <div className="small muted">{p.deviceName}</div>
                  </div>
                  {p.id === people?.controlling && (
                    <span className="badge badge-amber">{t.viewer.screen.controlling}</span>
                  )}
                </div>
              ))}
              {!people && <div className="muted small">{t.viewer.screen.noParticipants}</div>}
              {people && others.length === 0 && (
                <div className="muted small" style={{ padding: 12 }}>
                  {t.viewer.screen.noOthers}
                </div>
              )}
            </div>
          )}
        </aside>
      )}
    </div>
  );
}
