// The browser viewer (`docs/web-console.md` §7.4, §7.7): the remote picture
// with scaling, full screen, picture-in-picture, display switching, sound,
// other participants' pointers, screenshot, statistics, participants,
// host events and automatic recovery; within the granted capabilities,
// keyboard and mouse control (special keys, pointer lock, touch), the text
// clipboard, files and the microphone.
import { useCallback, useEffect, useMemo, useRef, useState, type ReactNode } from 'react';
import { argbCss, type AnnotationState } from '../lib/annotation';
import { ApiError, call, errorText, hasToken } from '../lib/api';
import { DEFAULT_DIR, droppedEntries, type TransferInfo } from '../lib/files';
import { bitrate, capText, displayName } from '../lib/format';
import type { CursorShape, HostEvent, Participant, ParticipantPointer, Quality, Screens } from '../lib/fpp';
import { HostEventKind } from '../lib/fpp';
import { SystemCommand, Usage } from '../lib/input';
import { VideoCodec, type VideoCodecId } from '../lib/sdp';
import { PATHS, StatsSampler, type ViewStats } from '../lib/stats';
import {
  Viewer,
  type Admission,
  type ControlState,
  type Ending,
  type MicrophoneStatus,
  type ViewerState,
} from '../lib/viewer';
import { FilesPanel, TransferList } from './FilesPanel';
import { Icon, type IconName } from './icons';
import { Button, Spinner } from './kit';
import { Dialog, useOverlay } from './overlay';
import { AnnotationLayer, type AnnotationTool } from './AnnotationLayer';
import { useRemoteInput, type PictureMap } from './remoteInput';

type Scale = 'fit' | 'fill' | 'original';
type Panel = 'none' | 'stats' | 'people' | 'files';

/// Take the control token when a session starts ('0': the user turned it off).
const CONTROL_KEY = 'farwing.viewer.control';
/// Automatic clipboard synchronization ('0': off).
const CLIPBOARD_KEY = 'farwing.viewer.clipboard';
/// 'mouse': touch acts as mouse gestures even when the host takes touch.
const TOUCH_KEY = 'farwing.viewer.touch';
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

const QUALITIES: { id: number; label: string; hint?: string }[] = [
  { id: 5, label: '自动' },
  { id: 4, label: '原画', hint: '30M' },
  { id: 3, label: '超清', hint: '14M' },
  { id: 2, label: '高清', hint: '8M' },
];

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

const INK_COLORS: { argb: number; label: string }[] = [
  { argb: 0xffff4444, label: '红' },
  { argb: 0xffffc83d, label: '黄' },
  { argb: 0xff3ccf7a, label: '绿' },
  { argb: 0xff4b88eb, label: '蓝' },
  { argb: 0xffffffff, label: '白' },
  { argb: 0xff202020, label: '黑' },
];
const INK_WIDTHS: { width: number; label: string }[] = [
  { width: 2, label: '细' },
  { width: 4, label: '中' },
  { width: 8, label: '粗' },
];

const PALETTE = [0x4b88eb, 0x66cf9c, 0xeebe6f, 0xbe84f0, 0xf17d84, 0x5cc4d6];

function colorOf(p: Participant | undefined, id: string): string {
  const value = p?.color || PALETTE[Number(BigInt(id) % 6n)];
  return `#${value.toString(16).padStart(6, '0')}`;
}

function hostEventText(e: HostEvent): string {
  switch (e.kind) {
    case HostEventKind.UPDATING:
      return '对方正在更新 Farwing';
    case HostEventKind.RESTARTING:
      return '对方设备正在重启';
    case HostEventKind.SLEEPING:
      return '对方设备正在进入睡眠';
    case HostEventKind.SHUTTING_DOWN:
      return '对方设备正在关机';
    case HostEventKind.SESSION_SWITCH:
      return '对方切换了 Windows 用户';
    case HostEventKind.DISPLAY_CHANGING:
      return e.message || '对方正在修改显示设置';
    default:
      return e.message || '对方设备状态变化';
  }
}

const STATE_TEXT: Record<ViewerState, string> = {
  connecting: '正在连接',
  negotiating: '正在建立连接',
  waiting_media: '等待画面',
  playing: '',
  reconnecting: '网络中断，正在恢复',
  ended: '已断开',
};

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
}: {
  icon: IconName;
  label: string;
  onClick(): void;
  active?: boolean;
  disabled?: boolean;
}) {
  return (
    <button
      type="button"
      className={`tool ${active ? 'active' : ''}`}
      title={label}
      aria-label={label}
      disabled={disabled}
      onClick={onClick}
    >
      <Icon name={icon} size={18} />
    </button>
  );
}

function Menu({ label, icon, text, children }: { label: string; icon: IconName; text?: string; children: ReactNode }) {
  const [open, setOpen] = useState(false);
  const ref = useRef<HTMLDivElement>(null);
  useEffect(() => {
    if (!open) return;
    const close = (e: MouseEvent) => {
      if (!ref.current?.contains(e.target as Node)) setOpen(false);
    };
    window.addEventListener('mousedown', close);
    return () => window.removeEventListener('mousedown', close);
  }, [open]);
  return (
    <div className="tool-menu" ref={ref}>
      <button type="button" className={`tool ${open ? 'active' : ''}`} title={label} onClick={() => setOpen(!open)}>
        <Icon name={icon} size={18} />
        {text && <span className="tool-text">{text}</span>}
      </button>
      {open && (
        <div className="menu" onClick={() => setOpen(false)}>
          {children}
        </div>
      )}
    </div>
  );
}

function MenuItem({
  selected,
  onClick,
  children,
  hint,
}: {
  selected?: boolean;
  onClick(): void;
  children: ReactNode;
  hint?: ReactNode;
}) {
  return (
    <button type="button" className={`menu-item ${selected ? 'selected' : ''}`} onClick={onClick}>
      <span className="menu-check">{selected && <Icon name="check" size={14} />}</span>
      <span className="grow">{children}</span>
      {hint && <span className="small faint">{hint}</span>}
    </button>
  );
}

export interface ViewerScreenProps {
  title: string;
  subtitle?: ReactNode;
  admit(): Promise<Admission>;
  onLeave(): void;
  /// Re-create lost sessions automatically (5 minutes).
  autoRetry?: boolean;
}

function sendCapture(v: Viewer, quality: number, fps: number, codec: number, cursor = true) {
  v.setCapture({
    fps: fps >= 60 ? 2 : 1,
    frameQuality: quality,
    // Off while controlling with an absolute mouse: the cursor is local.
    cursorCapture: cursor,
    maxCustomBitrate: 0,
    // 0 lets the host choose among the codecs both sides support.
    codecType: codec,
    maxScaleWidth: 0,
    maxScaleHeight: 0,
    fpsCount: fps,
    cinema: false,
  });
}

export function ViewerScreen({ title, subtitle, admit, onLeave, autoRetry = true }: ViewerScreenProps) {
  const { toast } = useOverlay();
  const root = useRef<HTMLDivElement>(null);
  const stage = useRef<HTMLDivElement>(null);
  const video = useRef<HTMLVideoElement>(null);
  const viewer = useRef<Viewer | undefined>(undefined);
  const firstFailure = useRef(0);
  const retries = useRef(0);
  const sampler = useRef(new StatsSampler());
  const [generation, setGeneration] = useState(0);
  const [state, setState] = useState<ViewerState>('connecting');
  const [detail, setDetail] = useState<string>();
  const [ending, setEnding] = useState<Ending>();
  const [retryAt, setRetryAt] = useState<number>();
  const [now, setNow] = useState(() => Date.now());
  const [screens, setScreens] = useState<Screens>();
  const [quality, setQuality] = useState<Quality>();
  const [grants, setGrants] = useState<string[]>([]);
  const [people, setPeople] = useState<{ participants: Participant[]; controlling: string }>();
  const [pointers, setPointers] = useState<ParticipantPointer[]>([]);
  const [banner, setBanner] = useState<string>();
  const [audioKbps, setAudioKbps] = useState<number>();
  const [stats, setStats] = useState<ViewStats>();
  const [scale, setScale] = useState<Scale>('fit');
  const [panel, setPanel] = useState<Panel>('none');
  const [muted, setMuted] = useState(true);
  const [volume, setVolume] = useState(1);
  const [fullscreen, setFullscreen] = useState(false);
  const [barVisible, setBarVisible] = useState(true);
  const [videoSize, setVideoSize] = useState<[number, number]>([0, 0]);
  const [frameQuality, setFrameQuality] = useState(5);
  const [fps, setFps] = useState(60);
  const [sharePointer, setSharePointer] = useState(false);
  const [codec, setCodec] = useState(storedCodec);
  const [codecs, setCodecs] = useState<{ common: VideoCodecId[]; hostPreferred?: VideoCodecId }>();
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
  const [inkTool, setInkTool] = useState<AnnotationTool>('pen');
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

  // One session per generation; a timer keeps React's double effects in
  // development from opening two.
  useEffect(() => {
    let current: Viewer | undefined;
    const timer = window.setTimeout(() => {
      const v = new Viewer(
        { admit, codec: () => capture.current.codec },
        {
          state: (s, d) => {
            setState(s);
            setDetail(d);
            if (s === 'playing') {
              firstFailure.current = 0;
              retries.current = 0;
            }
          },
          ended: (e) => onEnded(e),
          stream: (stream) => {
            if (video.current) video.current.srcObject = stream;
          },
          screens: setScreens,
          quality: setQuality,
          permissions: (g) => setGrants([...g]),
          participants: setPeople,
          pointers: setPointers,
          hostEvent: (e) => setBanner(hostEventText(e)),
          audio: (a) => {
            if (a.kind === 'rejected') toast(a.message ?? '对方没有接受音质设置', 'red');
            if (a.kbps) setAudioKbps(a.kbps);
          },
          codecs: (common, hostPreferred) => {
            setCodecs({ common, hostPreferred });
            const wanted = capture.current.codec;
            if (wanted && common.includes(wanted as VideoCodecId)) {
              sendCapture(v, capture.current.quality, capture.current.fps, wanted);
            }
          },
          captureResult: (errors) => {
            if (errors.some((c) => c === -1 || c === -2)) toast('对方没有接受画面设置', 'red');
          },
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
            setTransfers((list) => [...list.filter((t) => t.id !== info.id || t.direction !== info.direction), info]);
            if (info.state !== 'running') {
              window.setTimeout(
                () => setTransfers((list) => list.filter((t) => t.id !== info.id || t.direction !== info.direction)),
                5000,
              );
            }
          },
        },
      );
      current = v;
      viewer.current = v;
      // For testing from the developer tools of the dev server only.
      if (import.meta.env.DEV) (window as unknown as { farwingViewer?: Viewer }).farwingViewer = v;
      v.start().catch((error: unknown) => {
        if (v.state === 'ended') return;
        const code = error instanceof ApiError ? error.code : ((error as { code?: string })?.code ?? 'error');
        onEnded({
          reason: code,
          message: errorText(error),
          recoverable: RETRY_CODES.has(code) || error instanceof TypeError,
        });
      });
    }, 0);
    return () => {
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
    const t = Date.now();
    if (!firstFailure.current) firstFailure.current = t;
    if (t - firstFailure.current > RETRY_WINDOW_MS) return;
    const delay = RETRY_DELAYS[Math.min(retries.current, RETRY_DELAYS.length - 1)] * 1000;
    retries.current += 1;
    setRetryAt(t + delay);
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
      if (report) setStats(sampler.current.sample(report));
    }, 1000);
    return () => window.clearInterval(timer);
  }, []);

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
      toast('截图已保存');
    }, 'image/png');
  }, [title, toast]);

  const pip = useCallback(() => {
    const v = video.current;
    if (!v) return;
    if (document.pictureInPictureElement) void document.exitPictureInPicture();
    else void v.requestPictureInPicture?.().catch(() => toast('浏览器不支持画中画', 'red'));
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
    if (c) localStorage.setItem(CODEC_KEY, String(c));
    else localStorage.removeItem(CODEC_KEY);
    if (viewer.current) sendCapture(viewer.current, q, f, c, capture.current.cursor);
  };

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
    sendCapture(v, c.quality, c.fps, c.codec, c.cursor);
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
      if (!text) toast('本机剪贴板里没有文字', 'red');
      else {
        viewer.current?.clipboard.offer(text);
        toast('已发送到对方剪贴板');
      }
    } catch {
      toast('浏览器不允许读取剪贴板', 'red');
    }
  };

  const pullClipboard = async () => {
    try {
      const text = await viewer.current?.clipboard.pull();
      if (!text) toast('对方剪贴板里没有文字', 'red');
      else {
        copyLocally(text);
        toast('已复制到本机剪贴板');
      }
    } catch (error) {
      toast(errorText(error), 'red');
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
    if (!(await a.setEnabled(!a.active)) && !ink.error) toast('对方没有开启批注', 'red');
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
      if (files.length || folders.length) toast('已上传到对方的“下载”文件夹');
    } catch (error) {
      toast(errorText(error), 'red');
    }
  };
  const usedCodec = CODECS.find((c) => c.mime === stats?.codec?.toUpperCase());
  // A stored choice this host cannot encode counts as automatic.
  const chosenCodec = codecs?.common.includes(codec as VideoCodecId) ? codec : 0;

  const current = screens?.screens.find((s) => s.id === screens.current);
  // The screen in video_0: what input positions refer to.
  const shown = screens?.screens.find((s) => s.track === 0) ?? current;
  pictureMap.current = shown
    ? {
        screenId: shown.id,
        width: shown.current.width,
        height: shown.current.height,
        box: () => (video.current?.videoWidth ? contentBox(video.current, scale) : undefined),
      }
    : undefined;
  const byId = useMemo(() => new Map((people?.participants ?? []).map((p) => [p.id, p])), [people]);
  const visiblePointers = pointers.filter(
    (p) => p.visible && p.idleMs < 3000 && (!current || p.screenId === current.id),
  );
  const box = video.current && videoSize[0] ? contentBox(video.current, scale) : undefined;
  const playing = state === 'playing';
  const overlayText = ending ? ending.message : detail || STATE_TEXT[state];
  const others = (people?.participants ?? []).filter((p) => !p.you);
  const controller = people?.participants.find((p) => p.id === people.controlling);

  // Our indicator pointer, when shared: normalized to the picture.
  const onPointer = (e: React.PointerEvent<HTMLDivElement>, visible: boolean) => {
    showBar();
    if (control.held || !sharePointer || !video.current || !current || !box) return;
    const t = performance.now();
    if (visible && t - pointerSent.current < 30) return;
    pointerSent.current = t;
    const rect = video.current.getBoundingClientRect();
    const x = (e.clientX - rect.left - box.x) / box.w;
    const y = (e.clientY - rect.top - box.y) / box.h;
    const inside = x >= 0 && x <= 1 && y >= 0 && y <= 1;
    viewer.current?.sendPointer(
      current.id,
      Math.min(1, Math.max(0, x)),
      Math.min(1, Math.max(0, y)),
      visible && inside,
    );
  };

  return (
    <div ref={root} className={`viewer ${fullscreen && !barVisible ? 'bar-hidden' : ''}`} onMouseMove={showBar}>
      <header className="viewer-bar">
        <button type="button" className="tool" title="返回" onClick={onLeave}>
          <Icon name="back" size={18} />
        </button>
        <div className="viewer-title">
          <div className="ellipsis">{title}</div>
          <div className="small muted ellipsis">
            {playing && stats
              ? `${stats.width}×${stats.height} · ${Math.round(stats.fps)} fps · ${Math.round(stats.rttMs)} ms · ${PATHS[stats.path]}`
              : subtitle}
          </div>
        </div>
        <div className="viewer-tools">
          {screens && screens.screens.length > 1 && (
            <Menu label="显示器" icon="monitor" text={current?.name || `显示器 ${screens.current + 1}`}>
              {screens.screens.map((s, i) => (
                <MenuItem
                  key={s.id}
                  selected={s.id === screens.current}
                  hint={`${s.current.width}×${s.current.height}`}
                  onClick={() => viewer.current?.switchScreen(s.id)}
                >
                  {s.name || `显示器 ${i + 1}`}
                  {s.primary ? '（主屏）' : ''}
                </MenuItem>
              ))}
            </Menu>
          )}
          {controlAllowed && (
            <ToolButton
              icon="mouse"
              label={
                control.held
                  ? '停止键鼠控制'
                  : control.pending
                    ? '正在获取键鼠控制权'
                    : controller && !controller.you
                      ? `${controller.nickname || '其他参与者'} 正在控制`
                      : '开启键鼠控制'
              }
              active={control.held}
              disabled={!playing || control.pending}
              onClick={() => void toggleControl()}
            />
          )}
          {control.held && (
            <Menu label="特殊键与鼠标" icon="keyboard">
              <div className="menu-label">鼠标</div>
              <MenuItem selected={!relative} onClick={() => setRelative(false)}>
                绝对定位
              </MenuItem>
              <MenuItem
                selected={relative}
                onClick={() => {
                  setRelative(true);
                  void stage.current?.requestPointerLock?.();
                }}
              >
                相对移动
              </MenuItem>
              {!!hostFeatures.touch && (
                <>
                  <div className="menu-label">触屏</div>
                  <MenuItem
                    selected={!touchMouse}
                    onClick={() => {
                      localStorage.removeItem(TOUCH_KEY);
                      setTouchMouse(false);
                    }}
                  >
                    原生触摸
                  </MenuItem>
                  <MenuItem
                    selected={touchMouse}
                    onClick={() => {
                      localStorage.setItem(TOUCH_KEY, 'mouse');
                      setTouchMouse(true);
                    }}
                  >
                    模拟鼠标
                  </MenuItem>
                </>
              )}
              <div className="menu-label">组合键</div>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.SAS))}>Ctrl+Alt+Del</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.WIN]))}>Win</MenuItem>
              <MenuItem hint="运行" onClick={() => special((v) => v.input.chord([Usage.WIN, Usage.R]))}>
                Win+R
              </MenuItem>
              <MenuItem hint="资源管理器" onClick={() => special((v) => v.input.chord([Usage.WIN, Usage.E]))}>
                Win+E
              </MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.ALT, Usage.TAB]))}>Alt+Tab</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.ALT, Usage.F4]))}>Alt+F4</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.ESC]))}>Esc</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.chord([Usage.PRINT_SCREEN]))}>PrintScreen</MenuItem>
              <div className="menu-label">系统</div>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.LOCK))}>锁定</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.TASK_MANAGER))}>
                任务管理器
              </MenuItem>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.SHOW_DESKTOP))}>显示桌面</MenuItem>
              <MenuItem onClick={() => special((v) => v.input.command(SystemCommand.TASK_VIEW))}>任务视图</MenuItem>
              {clipboardAllowed && (
                <>
                  <div className="menu-label">文字</div>
                  <MenuItem onClick={() => setTyping('')}>输入文字…</MenuItem>
                </>
              )}
            </Menu>
          )}
          {inkAllowed && ink.available && (
            <Menu label="批注" icon="edit">
              <MenuItem selected={ink.enabled} onClick={() => void toggleInk()}>
                {ink.enabled ? '关闭批注' : '开启批注'}
              </MenuItem>
              {ink.enabled && shown && (
                <>
                  <div className="menu-label">工具</div>
                  <MenuItem selected={inkTool === 'pen'} onClick={() => setInkTool('pen')}>
                    画笔
                  </MenuItem>
                  <MenuItem selected={inkTool === 'laser'} onClick={() => setInkTool('laser')}>
                    激光笔
                  </MenuItem>
                  <div className="menu-label">颜色与粗细</div>
                  <div className="ink-swatches" onClick={(e) => e.stopPropagation()}>
                    {INK_COLORS.map((c) => (
                      <button
                        key={c.argb}
                        type="button"
                        className={`ink-swatch ${inkColor === c.argb ? 'selected' : ''}`}
                        style={{ background: argbCss(c.argb) }}
                        title={c.label}
                        aria-label={c.label}
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
                        {w.label}
                      </button>
                    ))}
                  </div>
                  <div className="menu-label">编辑</div>
                  <MenuItem
                    onClick={() => inkAction(() => viewer.current?.annotation.undo())}
                    hint={inkRevision >= 0 && viewer.current?.annotation.canUndo ? undefined : '没有笔迹'}
                  >
                    撤销上一笔
                  </MenuItem>
                  <MenuItem onClick={() => inkAction(() => viewer.current?.annotation.clear())}>清空笔迹</MenuItem>
                  <MenuItem
                    selected={!!viewer.current?.annotation.hasBoard(shown.id)}
                    onClick={() =>
                      inkAction(() => {
                        const a = viewer.current?.annotation;
                        a?.setBoard(shown.id, !a.hasBoard(shown.id));
                      })
                    }
                  >
                    白板
                  </MenuItem>
                </>
              )}
            </Menu>
          )}
          {clipboardAllowed && (
            <Menu label="剪贴板" icon="clipboard">
              <MenuItem
                selected={clipboardAuto}
                onClick={() => {
                  localStorage.setItem(CLIPBOARD_KEY, clipboardAuto ? '0' : '1');
                  setClipboardAuto(!clipboardAuto);
                }}
              >
                自动同步文字
              </MenuItem>
              <MenuItem onClick={() => void sendClipboard()}>发送本机剪贴板到对方</MenuItem>
              <MenuItem onClick={() => void pullClipboard()}>获取对方剪贴板</MenuItem>
            </Menu>
          )}
          {filesAllowed && (
            <ToolButton
              icon="folder"
              label="文件"
              active={panel === 'files'}
              disabled={!playing}
              onClick={() => setPanel(panel === 'files' ? 'none' : 'files')}
            />
          )}
          {micAllowed && (
            <ToolButton
              icon={mic.state === 'off' ? 'micOff' : 'mic'}
              label={
                mic.state === 'off'
                  ? '开启麦克风'
                  : mic.state === 'starting'
                    ? '正在开启麦克风'
                    : mic.state === 'waiting'
                      ? '麦克风已开启，等待对方使用'
                      : '关闭麦克风'
              }
              active={mic.state === 'on'}
              disabled={!playing}
              onClick={toggleMic}
            />
          )}
          <Menu label="缩放" icon="fit">
            <MenuItem selected={scale === 'fit'} onClick={() => setScale('fit')}>
              适应窗口
            </MenuItem>
            <MenuItem selected={scale === 'fill'} onClick={() => setScale('fill')}>
              填满窗口
            </MenuItem>
            <MenuItem selected={scale === 'original'} onClick={() => setScale('original')}>
              原始大小（1:1）
            </MenuItem>
          </Menu>
          <Menu label="画质" icon="settings">
            <div className="menu-label">画质</div>
            {QUALITIES.map((q) => (
              <MenuItem
                key={q.id}
                selected={frameQuality === q.id}
                hint={q.hint}
                onClick={() => applyCapture(q.id, fps)}
              >
                {q.label}
              </MenuItem>
            ))}
            <div className="menu-label">帧率</div>
            {[30, 60].map((f) => (
              <MenuItem key={f} selected={fps === f} onClick={() => applyCapture(frameQuality, f)}>
                {f} FPS
              </MenuItem>
            ))}
            {codecs && codecs.common.length > 1 && (
              <>
                <div className="menu-label">编码格式</div>
                {codecs.hostPreferred ? (
                  <div className="menu-note small faint">
                    对方指定 {CODECS.find((c) => c.id === codecs.hostPreferred)?.label}
                  </div>
                ) : (
                  <>
                    <MenuItem
                      selected={!chosenCodec}
                      hint={!chosenCodec && usedCodec ? usedCodec.label : undefined}
                      onClick={() => applyCapture(frameQuality, fps, 0)}
                    >
                      自动
                    </MenuItem>
                    {CODECS.filter((c) => codecs.common.includes(c.id)).map((c) => (
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
              </>
            )}
            {audioAllowed && (
              <>
                <div className="menu-label">音质</div>
                {[64, 128, 192, 256].map((k) => (
                  <MenuItem key={k} selected={audioKbps === k} onClick={() => viewer.current?.setAudioKbps(k)}>
                    {k} kbps
                  </MenuItem>
                ))}
              </>
            )}
            <div className="menu-label">其他</div>
            <MenuItem selected={sharePointer} onClick={() => setSharePointer(!sharePointer)}>
              向其他参与者显示我的指针
            </MenuItem>
          </Menu>
          <div className="volume">
            <ToolButton
              icon={muted || !audioAllowed ? 'mute' : 'volume'}
              label={audioAllowed ? (muted ? '开启声音（M）' : '静音（M）') : '没有声音权限'}
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
                aria-label="音量"
              />
            )}
          </div>
          <ToolButton icon="camera" label="截图（S）" onClick={screenshot} disabled={!playing} />
          <ToolButton icon="pip" label="画中画" onClick={pip} disabled={!playing} />
          <ToolButton
            icon="chart"
            label="连接统计"
            active={panel === 'stats'}
            onClick={() => setPanel(panel === 'stats' ? 'none' : 'stats')}
          />
          <ToolButton
            icon="users"
            label="参与者"
            active={panel === 'people'}
            onClick={() => setPanel(panel === 'people' ? 'none' : 'people')}
          />
          <ToolButton
            icon={fullscreen ? 'exitFullscreen' : 'fullscreen'}
            label={fullscreen ? '退出全屏（F）' : '全屏（F）'}
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
            style={
              scale === 'original' && videoSize[0]
                ? { width: videoSize[0] / devicePixelRatio, height: videoSize[1] / devicePixelRatio }
                : undefined
            }
          />
          {box && playing && annotating && shown && viewer.current && (
            <AnnotationLayer
              annotation={viewer.current.annotation}
              box={box}
              screenId={shown.id}
              screenWidth={shown.current.width}
              tool={inkTool}
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
                      {who ? displayName({ nickname: who.nickname }) : '参与者'}
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
                  <div className="muted">{Math.max(0, Math.ceil((retryAt - now) / 1000))} 秒后自动重连</div>
                ) : ending.recoverable && autoRetry ? (
                  <div className="muted">已尝试 5 分钟，没有恢复</div>
                ) : null}
                <div className="row gap-sm">
                  {(ending.recoverable || !autoRetry) && (
                    <Button variant="primary" icon="refresh" onClick={retryNow}>
                      立即重试
                    </Button>
                  )}
                  <Button onClick={onLeave}>返回</Button>
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
            <Icon name="volume" size={16} /> 点击开启声音
          </button>
        )}
        {banner && <div className="viewer-banner">{banner}</div>}
        {keyboardLocked && barVisible && <div className="viewer-hint">键盘已交给对方</div>}
        {pendingCopy !== undefined && (
          <button type="button" className="unmute-hint copy-hint" onClick={() => copyLocally(pendingCopy)}>
            <Icon name="clipboard" size={16} /> 复制对方的文字到本机
          </button>
        )}
        {dragOver && <div className="drop-hint">上传到对方的“下载”文件夹</div>}
        {panel !== 'files' && transfers.length > 0 && (
          <div className="transfers-float">
            <TransferList transfers={transfers} files={viewer.current?.files} />
          </div>
        )}
      </div>

      <Dialog
        open={typing !== undefined}
        title="输入文字"
        onClose={() => setTyping(undefined)}
        footer={
          <>
            <Button onClick={() => setTyping(undefined)}>取消</Button>
            <Button
              variant="primary"
              disabled={!typing}
              onClick={() => {
                const text = typing ?? '';
                setTyping(undefined);
                typeText(text);
              }}
            >
              发送
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
              {panel === 'stats'
                ? '连接统计'
                : panel === 'files'
                  ? '对方的文件'
                  : `参与者（${people?.participants.length ?? 1}）`}
            </h2>
            <button type="button" className="tool" onClick={() => setPanel('none')} aria-label="关闭">
              <Icon name="close" size={16} />
            </button>
          </header>
          {panel === 'files' ? (
            <FilesPanel files={viewer.current?.files} transfers={transfers} toast={toast} />
          ) : panel === 'stats' ? (
            <dl className="stats">
              <dt>分辨率</dt>
              <dd>{stats?.width ? `${stats.width} × ${stats.height}` : '—'}</dd>
              <dt>帧率</dt>
              <dd>{stats ? `${Math.round(stats.fps)} fps` : '—'}</dd>
              <dt>码率</dt>
              <dd>{stats ? bitrate(stats.bitrate) : '—'}</dd>
              <dt>丢包</dt>
              <dd>{stats ? `${(stats.loss * 100).toFixed(1)}%` : '—'}</dd>
              <dt>往返时间</dt>
              <dd>{stats ? `${Math.round(stats.rttMs)} ms` : '—'}</dd>
              <dt>抖动</dt>
              <dd>{stats ? `${Math.round(stats.jitterMs)} ms` : '—'}</dd>
              <dt>解码耗时</dt>
              <dd>{stats?.decodeMs ? `${stats.decodeMs.toFixed(1)} ms` : '—'}</dd>
              <dt>丢弃帧</dt>
              <dd>{stats?.framesDropped ?? '—'}</dd>
              <dt>编码格式</dt>
              <dd>{stats?.codec || '—'}</dd>
              <dt>解码</dt>
              <dd>
                {stats?.decoder || '—'}
                {stats?.hardware !== undefined && (stats.hardware ? '（硬件）' : '（软件）')}
              </dd>
              <dt>对方编码</dt>
              <dd>{quality ? `${quality.encoder || '—'} · ${quality.capture || '—'}` : '—'}</dd>
              <dt>路径</dt>
              <dd>
                {stats ? PATHS[stats.path] : '—'}
                {stats?.protocol && ` · ${stats.protocol.toUpperCase()}`}
              </dd>
              <dt>对方地址</dt>
              <dd className="mono small">{stats?.remote || '—'}</dd>
              <dt>声音</dt>
              <dd>
                {!audioAllowed ? '没有权限' : stats ? bitrate(stats.audioBitrate) : '—'}
                {audioAllowed && audioKbps ? ` · 设定 ${audioKbps} kbps` : ''}
              </dd>
              <dt>我的权限</dt>
              <dd>{grants.length ? grants.map(capText).join('、') : '—'}</dd>
            </dl>
          ) : (
            <div className="list">
              {(people?.participants ?? []).map((p) => (
                <div key={p.id} className="list-item">
                  <span className="dot-color" style={{ background: colorOf(p, p.id) }} />
                  <div className="grow">
                    <div>
                      {p.nickname || '访客'}
                      {p.you && <span className="muted">（我）</span>}
                    </div>
                    <div className="small muted">{p.deviceName}</div>
                  </div>
                  {p.id === people?.controlling && <span className="badge badge-amber">控制中</span>}
                </div>
              ))}
              {!people && <div className="muted small">暂无参与者</div>}
              {people && others.length === 0 && (
                <div className="muted small" style={{ padding: 12 }}>
                  只有你在观看
                </div>
              )}
            </div>
          )}
        </aside>
      )}
    </div>
  );
}
