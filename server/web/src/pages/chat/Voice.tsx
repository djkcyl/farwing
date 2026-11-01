// Voice on the chat page (`docs/protocol/chat-design.md` §7, §12.2): who is
// in each voice channel, this browser's voice bar and toggles, calls in
// direct messages, a participant's volume and moderation, the settings.
import { useEffect, useRef, useState } from 'react';
import { useT } from '../../i18n/react';
import { errorText } from '../../lib/api';
import { navigate } from '../../lib/router';
import { chat, useChat } from '../../lib/chat/store';
import {
  has,
  PERM,
  type Call,
  type Channel,
  type ChatUser,
  type Dm,
  type GroupState,
  type Id,
  type VoiceParticipant,
} from '../../lib/chat/types';
import { SOUND_GROUPS } from '../../lib/chat/sounds';
import { Mic, testMicrophone, useVoice, voice, type TrackRef, type VoiceSettings } from '../../lib/chat/voice';
import { Icon } from '../../ui/icons';
import { Button, Field, IconButton, Loading, Segmented, Select, Slider, Switch } from '../../ui/kit';
import { useOverlay } from '../../ui/overlay';
import { Avatar, displayName } from './parts';

/// The people in voice channel `channel`; those this browser hears light up.
export function VoicePeople({
  group,
  channel,
  onPerson,
}: {
  group: GroupState;
  channel: Id;
  onPerson(user: string, anchor: HTMLElement, voice: { channel: Id; participant: number }): void;
}) {
  const t = useT();
  const state = useChat();
  const v = useVoice();
  const people = state.snap?.voice.find((x) => x.channel_id === channel)?.participants ?? [];
  const members = state.members[group.group.id];
  useEffect(() => {
    if (people.length && !members) void chat.loadMembers(group.group.id);
  }, [people.length, members, group.group.id]);
  if (!people.length) return null;
  const mine = v.my && !v.my.call && v.my.channel === channel ? v.my : undefined;
  return (
    <div className="chat-voice-people">
      {people.map((p) => {
        const member = members?.find((m) => m.user.user_id === p.user_id);
        const user: ChatUser = member?.user ??
          (state.snap?.me.user_id === p.user_id ? state.snap.me : undefined) ?? {
            user_id: p.user_id,
            username: '',
            nickname: '',
            avatar: '',
            gone: false,
          };
        const talking = mine
          ? mine.participant === p.participant
            ? v.meSpeaking
            : v.speaking.includes(p.participant)
          : false;
        return (
          <button
            key={p.participant}
            type="button"
            className={`chat-voice-person ${talking ? 'talking' : ''}`}
            onClick={(e) => onPerson(p.user_id, e.currentTarget, { channel, participant: p.participant })}
          >
            <Avatar user={user} size={20} />
            <span className="ellipsis grow">{displayName(user, member?.nickname)}</span>
            {p.tracks.includes('screen') && <span className="chat-live">{t.chat.voice.live}</span>}
            {p.tracks.includes('cam_hi') && <Icon name="webcam" size={14} className="faint" />}
            {(p.self_muted || p.server_muted || p.cannot_speak) && (
              <Icon name="micOff" size={14} className={p.server_muted || p.cannot_speak ? 'red' : 'faint'} />
            )}
            {(p.self_deafened || p.server_deafened) && (
              <Icon name="headsetOff" size={14} className={p.server_deafened ? 'red' : 'faint'} />
            )}
          </button>
        );
      })}
    </div>
  );
}

/// Over the user bar while this browser is in a voice channel or a call.
export function VoiceBar() {
  const t = useT();
  const state = useChat();
  const v = useVoice();
  const my = v.my;
  if (!my) return null;
  const serverMuted =
    !my.call &&
    !!state.snap?.voice
      .find((c) => c.channel_id === my.channel)
      ?.participants.find((p) => p.participant === my.participant && (p.server_muted || p.cannot_speak));
  const where = chat.channel(my.channel);
  const place = my.call
    ? where?.dm
      ? displayName(where.dm.user)
      : ''
    : where
      ? where.group
        ? `${where.channel.name} / ${where.group.group.name}`
        : where.channel.name
      : '';
  const open = () =>
    navigate(where?.dm ? `/chat/dm/${my.channel}` : `/chat/${where?.group?.group.id ?? ''}/${my.channel}`);
  return (
    <div className="chat-voicebar">
      <button type="button" className="chat-voicebar-text grow" onClick={open}>
        <b className={my.connected ? 'green' : 'amber'}>
          {my.connected ? t.chat.voice.connected : t.chat.voice.connecting}
        </b>
        <span className="small muted ellipsis">
          {[place, my.compat && t.chat.voice.compat, serverMuted && t.chat.voice.serverMuted]
            .filter(Boolean)
            .join(' · ')}
        </span>
      </button>
      {!my.call && (
        <IconButton
          icon="webcam"
          label={v.camera ? t.chat.voice.cameraOff : t.chat.voice.cameraOn}
          className={v.camera ? 'on' : ''}
          onClick={() => void voice.setCamera(!v.camera)}
        />
      )}
      <IconButton
        icon="phoneOff"
        label={my.call ? t.chat.voice.hangUp : t.chat.voice.disconnect}
        onClick={() => void voice.stop(true)}
      />
    </div>
  );
}

/// Mute and deafen (they hold for the next channel too).
export function VoiceToggles() {
  const t = useT();
  const v = useVoice();
  return (
    <>
      <IconButton
        icon={v.muted ? 'micOff' : 'mic'}
        label={v.muted ? t.chat.voice.unmute : t.chat.voice.mute}
        variant={v.muted ? 'danger' : 'ghost'}
        onClick={() => voice.setSelf(!v.muted, v.deafened)}
      />
      <IconButton
        icon={v.deafened ? 'headsetOff' : 'headset'}
        label={v.deafened ? t.chat.voice.undeafen : t.chat.voice.deafen}
        variant={v.deafened ? 'danger' : 'ghost'}
        onClick={() => voice.setSelf(v.muted, !v.deafened)}
      />
    </>
  );
}

function callOf(channel: Id, calls: Call[] | undefined): Call | undefined {
  return calls?.find((c) => c.channel_id === channel);
}

/// The call button of a direct message (while it has no call).
export function CallButton({ dm }: { dm: Dm }) {
  const t = useT();
  const state = useChat();
  if (callOf(dm.channel.id, state.snap?.calls) || dm.user.gone) return null;
  return <IconButton icon="phone" label={t.chat.voice.call} onClick={() => void voice.startCall(dm.channel.id)} />;
}

function clock(ms: number): string {
  const s = Math.max(0, Math.floor(ms / 1000));
  return `${String(Math.floor(s / 60)).padStart(2, '0')}:${String(s % 60).padStart(2, '0')}`;
}

/// Under a direct message's header while it has a call.
export function CallBar({ dm }: { dm: Dm }) {
  const t = useT();
  const state = useChat();
  const v = useVoice();
  const [, tick] = useState(0);
  const c = callOf(dm.channel.id, state.snap?.calls);
  const here = !!c && v.my?.call === c.id;
  useEffect(() => {
    if (!here) return;
    const timer = window.setInterval(() => tick((n) => n + 1), 1000);
    return () => window.clearInterval(timer);
  }, [here]);
  if (!c) return null;
  const me = state.snap?.me.user_id;
  const calling = c.state === 'ringing' && c.caller.user_id === me;
  const ringing = c.state === 'ringing' && !calling;
  const [text, tone] = calling
    ? [t.chat.voice.calling, 'amber']
    : ringing
      ? [t.chat.voice.incoming(displayName(c.caller)), 'amber']
      : here
        ? [`${t.chat.voice.inCall} · ${clock(Date.now() - (c.answered_at ?? Date.now()))}`, 'green']
        : [t.chat.voice.elsewhere, 'muted'];
  return (
    <div className="chat-callbar">
      <Icon name="phone" size={16} className={tone} />
      <b className={tone}>{text}</b>
      <span className="grow" />
      {here || calling ? (
        <>
          {here && <VoiceToggles />}
          <Button size="sm" variant="danger" icon="phoneOff" onClick={() => void voice.stop(true)}>
            {t.chat.voice.hangUp}
          </Button>
        </>
      ) : ringing ? (
        <>
          <Button size="sm" onClick={() => voice.decline(c.id)}>
            {t.chat.voice.decline}
          </Button>
          <Button size="sm" variant="primary" onClick={() => void voice.answer(c.id, false)}>
            {t.chat.voice.answer}
          </Button>
        </>
      ) : (
        <Button size="sm" onClick={() => void voice.answer(c.id, true)}>
          {t.chat.voice.takeHere}
        </Button>
      )}
    </div>
  );
}

/// A participant's volume and mute here and, for moderators, server mute,
/// deafen, a move and disconnect (in their user card).
export function VoiceCardPart({
  channel,
  participant,
  onClose,
}: {
  channel: Id;
  participant: number;
  onClose(): void;
}) {
  const t = useT();
  const state = useChat();
  useVoice();
  const { toast } = useOverlay();
  const p = state.snap?.voice
    .find((x) => x.channel_id === channel)
    ?.participants.find((x) => x.participant === participant);
  if (!p) return null;
  const me = p.user_id === state.snap?.me.user_id;
  const where = chat.channel(channel);
  const moderator = has(where?.channel.permissions ?? 0, PERM.VOICE_MODERATE);
  const others = where?.group?.channels.filter((c) => c.kind === 'voice' && c.id !== channel) ?? [];
  const act = async (f: () => Promise<unknown>) => {
    try {
      await f();
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };
  const volume = voice.settings.volumes[p.user_id] ?? 100;
  const mutedHere = voice.settings.muted.includes(p.user_id);
  return (
    <>
      {!me && (
        <>
          <Field label={`${t.chat.voice.userVolume} · ${volume}%`}>
            <Slider
              value={volume}
              min={0}
              max={100}
              step={5}
              label={t.chat.voice.userVolume}
              onChange={(n) => voice.setVolume(p.user_id, n)}
            />
          </Field>
          <Button size="sm" icon={mutedHere ? 'volume' : 'mute'} onClick={() => voice.muteUser(p.user_id, !mutedHere)}>
            {mutedHere ? t.chat.voice.unmuteUser : t.chat.voice.muteUser}
          </Button>
        </>
      )}
      {moderator && (
        <div className="row gap-sm wrap">
          <Button
            size="sm"
            icon={p.server_muted ? 'mic' : 'micOff'}
            onClick={() => void act(() => voice.moderate(channel, participant, { server_muted: !p.server_muted }))}
          >
            {p.server_muted ? t.chat.voice.serverUnmute : t.chat.voice.serverMute}
          </Button>
          <Button
            size="sm"
            icon={p.server_deafened ? 'headset' : 'headsetOff'}
            onClick={() =>
              void act(() => voice.moderate(channel, participant, { server_deafened: !p.server_deafened }))
            }
          >
            {p.server_deafened ? t.chat.voice.serverUndeafen : t.chat.voice.serverDeafen}
          </Button>
          {!me && (
            <Button
              size="sm"
              variant="danger"
              onClick={() => void act(async () => (await voice.disconnect(channel, participant), onClose()))}
            >
              {t.chat.voice.kick}
            </Button>
          )}
        </div>
      )}
      {moderator && others.length > 0 && (
        <Field label={t.chat.voice.moveTo}>
          <div className="row gap-sm wrap">
            {others.map((c) => (
              <Button
                key={c.id}
                size="sm"
                icon="volume"
                onClick={() =>
                  void act(async () => (await voice.moderate(channel, participant, { move_to: c.id }), onClose()))
                }
              >
                {c.name}
              </Button>
            ))}
          </div>
        </Field>
      )}
    </>
  );
}

function useDevices(): MediaDeviceInfo[] {
  const [list, setList] = useState<MediaDeviceInfo[]>([]);
  useEffect(() => {
    const load = () =>
      void navigator.mediaDevices
        ?.enumerateDevices()
        .then(setList)
        .catch(() => undefined);
    load();
    navigator.mediaDevices?.addEventListener('devicechange', load);
    return () => navigator.mediaDevices?.removeEventListener('devicechange', load);
  }, []);
  return list;
}

/// The microphone's level for the settings: the session's, or one opened
/// while the settings are shown.
function useInputLevel(settings: VoiceSettings, paused: boolean): number {
  const [level, setLevel] = useState(0);
  const inVoice = voice.inVoice();
  useEffect(() => {
    if (paused) return;
    let mic: Mic | undefined;
    let gone = false;
    if (!inVoice) {
      void Mic.open(() => voice.settings)
        .then((m) => (gone ? m.close() : (mic = m)))
        .catch(() => undefined);
    }
    const timer = window.setInterval(() => setLevel(voice.inputLevel() ?? mic?.measure()[0] ?? 0), 100);
    return () => {
      gone = true;
      window.clearInterval(timer);
      mic?.close();
    };
  }, [inVoice, paused, settings.input, settings.echo, settings.noise, settings.gain]);
  return level;
}

/// The voice tab of the chat settings.
export function VoiceSettingsPanel() {
  const t = useT();
  useVoice();
  const s = voice.settings;
  const devices = useDevices();
  const [capturing, setCapturing] = useState(false);
  const [test, setTest] = useState<{ recording?: number; playing?: boolean }>({});
  const testing = test.recording !== undefined || !!test.playing;
  const level = useInputLevel(s, testing);
  const save = (next: Partial<VoiceSettings>) => voice.saveSettings({ ...s, ...next });
  useEffect(() => {
    if (!capturing) return;
    const key = (e: KeyboardEvent) => {
      e.preventDefault();
      setCapturing(false);
      if (e.code !== 'Escape') voice.saveSettings({ ...voice.settings, key: e.code });
    };
    window.addEventListener('keydown', key, true);
    return () => window.removeEventListener('keydown', key, true);
  }, [capturing]);
  const options = (kind: MediaDeviceKind) => [
    { value: '', label: t.chat.voice.defaultDevice },
    ...devices
      .filter((d) => d.kind === kind && d.deviceId && d.deviceId !== 'default' && d.deviceId !== 'communications')
      .map((d, i) => ({ value: d.deviceId, label: d.label || `${t.chat.voice.input} ${i + 1}` })),
  ];
  const canOutput = 'setSinkId' in HTMLMediaElement.prototype;
  const db = level > 0 ? 20 * Math.log10(level) : -90;
  const fill = Math.min(1, Math.max(0, (db + 70) / 70));
  const open = s.mode === 'activity' && db > (s.threshold ?? -50);
  return (
    <div className="stack">
      <Field label={t.chat.voice.input}>
        <Select<string>
          value={s.input ?? ''}
          options={options('audioinput')}
          onChange={(v) => save({ input: v || undefined })}
        />
      </Field>
      {canOutput && (
        <Field label={t.chat.voice.output}>
          <Select<string>
            value={s.output ?? ''}
            options={options('audiooutput')}
            onChange={(v) => save({ output: v || undefined })}
          />
        </Field>
      )}
      <Field label={`${t.chat.voice.inputVolume} · ${s.inputVolume}%`}>
        <Slider
          value={s.inputVolume}
          min={0}
          max={200}
          step={5}
          label={t.chat.voice.inputVolume}
          onChange={(n) => save({ inputVolume: n })}
        />
      </Field>
      <div className="row gap">
        <div className="voice-meter grow">
          <span className={open ? 'on' : ''} style={{ width: `${fill * 100}%` }} />
        </div>
        <Button
          size="sm"
          disabled={testing || voice.inVoice()}
          onClick={() => void testMicrophone(voice.settings, setTest).catch(() => setTest({}))}
        >
          {test.recording !== undefined
            ? t.chat.voice.micRecording(test.recording)
            : test.playing
              ? t.chat.voice.micPlaying
              : t.chat.voice.micTest}
        </Button>
      </div>
      <Field label={`${t.chat.voice.volume} · ${s.volume}%`}>
        <Slider
          value={s.volume}
          min={0}
          max={100}
          step={5}
          label={t.chat.voice.volume}
          onChange={(n) => save({ volume: n })}
        />
      </Field>
      <Field label={t.chat.voice.mode}>
        <Segmented<'activity' | 'ptt'>
          value={s.mode}
          options={[
            { value: 'activity', label: t.chat.voice.activity },
            { value: 'ptt', label: t.chat.voice.pushToTalk },
          ]}
          onChange={(mode) => save({ mode })}
        />
      </Field>
      {s.mode === 'activity' ? (
        <div>
          <Switch
            label={t.chat.voice.autoThreshold}
            checked={s.threshold === undefined}
            onChange={(auto) => save({ threshold: auto ? undefined : -45 })}
          />
          {s.threshold !== undefined && (
            <Field label={`${t.chat.voice.threshold} · ${s.threshold} dB`}>
              <Slider
                value={s.threshold}
                min={-70}
                max={-20}
                step={1}
                label={t.chat.voice.threshold}
                onChange={(n) => save({ threshold: n })}
              />
            </Field>
          )}
        </div>
      ) : (
        <Field label={t.chat.voice.key}>
          <Button size="sm" onClick={() => setCapturing(true)}>
            {capturing ? t.chat.voice.pressKey : (s.key ?? t.chat.voice.noKey)}
          </Button>
        </Field>
      )}
      <div>
        <Switch label={t.chat.voice.echo} checked={s.echo} onChange={(echo) => save({ echo })} />
        <Switch label={t.chat.voice.noise} checked={s.noise} onChange={(noise) => save({ noise })} />
        <Switch label={t.chat.voice.gain} checked={s.gain} onChange={(gain) => save({ gain })} />
      </div>
      <Field label={`${t.chat.voice.soundVolume} · ${s.soundVolume}%`}>
        <Slider
          value={s.soundVolume}
          min={0}
          max={100}
          step={5}
          label={t.chat.voice.soundVolume}
          onChange={(n) => save({ soundVolume: n })}
        />
      </Field>
      <div>
        {SOUND_GROUPS.map((g) => (
          <Switch
            key={g}
            label={t.chat.voice.soundGroups[g]}
            checked={!s.soundsOff.includes(g)}
            onChange={(on) =>
              save({ soundsOff: on ? s.soundsOff.filter((x) => x !== g) : [...s.soundsOff.filter((x) => x !== g), g] })
            }
          />
        ))}
      </div>
    </div>
  );
}

/// One picture: a track in a muted video element.
function VideoTile({ track, mirror }: { track: MediaStreamTrack; mirror?: boolean }) {
  const ref = useRef<HTMLVideoElement>(null);
  useEffect(() => {
    const el = ref.current;
    if (!el) return;
    el.srcObject = new MediaStream([track]);
    void el.play().catch(() => undefined);
    return () => {
      el.srcObject = null;
    };
  }, [track]);
  return <video ref={ref} autoPlay muted playsInline className={mirror ? 'mirror' : ''} />;
}

interface Source {
  person: VoiceParticipant;
  kind: 'cam' | 'screen';
}

/// The pictures of the voice channel this browser is in, above its
/// messages: cameras and screens in a grid, one of them large when chosen.
/// It watches the small camera picture in the grid and the large one when
/// chosen; a screen only once opened (720p30 if asked for).
export function VoiceStage({ group, channel }: { group: GroupState; channel: Channel }) {
  const t = useT();
  const state = useChat();
  const v = useVoice();
  const [focus, setFocus] = useState<{ participant: number; kind: 'cam' | 'screen' }>();
  const [previous, setPrevious] = useState<{ participant: number; kind: 'cam' | 'screen' }>();
  const [low, setLow] = useState(false);
  const stage = useRef<HTMLDivElement>(null);
  const people = state.snap?.voice.find((x) => x.channel_id === channel.id)?.participants ?? [];
  const here = !!v.my && !v.my.call && v.my.channel === channel.id;
  const me = v.my?.participant;
  const sources: Source[] = people.flatMap((person) => [
    ...(person.tracks.includes('cam_hi') || (person.participant === me && v.camera)
      ? [{ person, kind: 'cam' as const }]
      : []),
    ...(person.tracks.includes('screen') ? [{ person, kind: 'screen' as const }] : []),
  ]);
  const focused = focus && sources.find((s) => s.person.participant === focus.participant && s.kind === focus.kind);
  const key = sources.map((s) => `${s.person.participant}${s.kind}`).join(',');
  // What this browser watches follows the layout; the earlier large picture
  // stays a moment so the new one starts without a gap.
  useEffect(() => {
    if (!here) return;
    const refs: TrackRef[] = [];
    for (const s of sources) {
      const p = s.person.participant;
      if (p === me) continue;
      const big = focused?.person.participant === p && focused.kind === s.kind;
      const was = previous?.participant === p && previous.kind === s.kind;
      if (s.kind === 'cam') {
        refs.push({ participant: p, kind: big ? 'cam_hi' : 'cam_lo' });
        if (was && !big) refs.push({ participant: p, kind: 'cam_hi' });
      } else if (big || was) {
        refs.push({ participant: p, kind: low && big ? 'screen_720' : 'screen' });
        if (s.person.screen?.audio !== false) refs.push({ participant: p, kind: 'screen_audio' });
      }
    }
    voice.watchTracks(refs);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [here, key, focused?.person.participant, focused?.kind, previous, low, me]);
  useEffect(() => {
    if (!previous) return;
    const timer = window.setTimeout(() => setPrevious(undefined), 2000);
    return () => window.clearTimeout(timer);
  }, [previous]);
  if (!here || !sources.length) return null;
  const choose = (next?: { participant: number; kind: 'cam' | 'screen' }) => {
    if (focus) setPrevious(focus);
    setFocus(next);
    setLow(false);
  };
  const nameOf = (person: VoiceParticipant) => {
    const member = state.members[group.group.id]?.find((m) => m.user.user_id === person.user_id);
    if (person.participant === me) return t.chat.voice.you;
    return member ? displayName(member.user, member.nickname) : '';
  };
  const trackOf = (s: Source, big: boolean) => {
    if (s.person.participant === me) return s.kind === 'cam' ? v.localCamera : undefined;
    const kinds =
      s.kind === 'cam'
        ? big
          ? ['cam_hi', 'cam_lo']
          : ['cam_lo', 'cam_hi']
        : low && big
          ? ['screen_720', 'screen']
          : ['screen', 'screen_720'];
    for (const kind of kinds) {
      const found = v.videos.find((x) => x.participant === s.person.participant && x.kind === kind);
      if (found) return found.track;
    }
    return undefined;
  };
  const tile = (s: Source) => {
    const track = s.kind === 'screen' && !(focused === s) ? undefined : trackOf(s, focused === s);
    return (
      <button
        key={`${s.person.participant}-${s.kind}`}
        type="button"
        className="voice-tile"
        onClick={() => choose({ participant: s.person.participant, kind: s.kind })}
      >
        {track ? (
          <VideoTile track={track} mirror={s.person.participant === me} />
        ) : (
          <span className="voice-tile-idle">
            <Icon name={s.kind === 'screen' ? 'monitor' : 'webcam'} size={22} />
            {s.kind === 'screen' && <span>{t.chat.voice.watch}</span>}
          </span>
        )}
        <span className="voice-tile-name">
          {s.kind === 'screen' && <span className="chat-live">{t.chat.voice.live}</span>}
          <span className="ellipsis">{nameOf(s.person)}</span>
        </span>
      </button>
    );
  };
  if (focused) {
    const track = trackOf(focused, true);
    const sharer = focused.kind === 'screen' && focused.person.participant !== me ? focused.person : undefined;
    const control = v.control?.sharer === sharer?.device_id ? v.control : undefined;
    return (
      <div className="voice-stage focused" ref={stage}>
        <div className="voice-big">
          {track ? <VideoTile track={track} mirror={focused.person.participant === me} /> : <Loading />}
        </div>
        <div className="voice-stage-bar">
          <IconButton icon="back" label={t.chat.voice.back} onClick={() => choose(undefined)} />
          <span className="ellipsis grow">
            {focused.kind === 'screen' && <span className="chat-live">{t.chat.voice.live}</span>}
            {nameOf(focused.person)}
          </span>
          {sharer && sharer.screen?.quality !== '720p30' && (
            <Segmented<'original' | 'p720'>
              value={low ? 'p720' : 'original'}
              options={[
                { value: 'original', label: t.chat.voice.original },
                { value: 'p720', label: t.chat.voice.p720 },
              ]}
              onChange={(q) => setLow(q === 'p720')}
            />
          )}
          {sharer?.screen?.whole &&
            (control?.allowed ? (
              <Button size="sm" onClick={() => voice.endControl()}>
                {t.chat.voice.giveBack}
              </Button>
            ) : (
              <Button
                size="sm"
                icon="mouse"
                disabled={!!control}
                onClick={() => void voice.requestControl(sharer.device_id)}
              >
                {control ? t.chat.voice.controlAsked : t.chat.voice.requestControl}
              </Button>
            ))}
          <IconButton
            icon="fullscreen"
            label={t.chat.voice.fullscreen}
            onClick={() => void stage.current?.requestFullscreen().catch(() => undefined)}
          />
        </div>
      </div>
    );
  }
  return (
    <div className="voice-stage" ref={stage}>
      {sources.map(tile)}
    </div>
  );
}
