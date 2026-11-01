// Voice on the chat page (`docs/protocol/chat-design.md` §7, §12.2): who is
// in each voice channel, this browser's voice bar and toggles, calls in
// direct messages, a participant's volume and moderation, the settings.
import { useEffect, useState } from 'react';
import { useT } from '../../i18n/react';
import { errorText } from '../../lib/api';
import { navigate } from '../../lib/router';
import { chat, useChat } from '../../lib/chat/store';
import { has, PERM, type Call, type ChatUser, type Dm, type GroupState, type Id } from '../../lib/chat/types';
import { useVoice, voice, type VoiceSettings } from '../../lib/chat/voice';
import { Icon } from '../../ui/icons';
import { Button, Field, IconButton, Segmented, Select, Slider, Switch } from '../../ui/kit';
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
            {(p.self_muted || p.server_muted) && (
              <Icon name="micOff" size={14} className={p.server_muted ? 'red' : 'faint'} />
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
      ?.participants.find((p) => p.participant === my.participant)?.server_muted;
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

/// A participant's volume and, for moderators, server mute and disconnect
/// (in their user card).
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
  const moderator = has(chat.channel(channel)?.channel.permissions ?? 0, PERM.VOICE_MODERATE);
  const act = async (f: () => Promise<unknown>) => {
    try {
      await f();
    } catch (e) {
      toast(errorText(e), 'red');
    }
  };
  const volume = voice.settings.volumes[p.user_id] ?? 100;
  return (
    <>
      {!me && (
        <Field label={`${t.chat.voice.userVolume} · ${volume}%`}>
          <Slider
            value={volume}
            min={0}
            max={200}
            step={5}
            label={t.chat.voice.userVolume}
            onChange={(n) => voice.setVolume(p.user_id, n)}
          />
        </Field>
      )}
      {moderator && (
        <div className="row gap-sm">
          <Button
            size="sm"
            icon={p.server_muted ? 'mic' : 'micOff'}
            onClick={() => void act(() => voice.moderate(channel, participant, !p.server_muted))}
          >
            {p.server_muted ? t.chat.voice.serverUnmute : t.chat.voice.serverMute}
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

/// The voice tab of the chat settings.
export function VoiceSettingsPanel() {
  const t = useT();
  useVoice();
  const s = voice.settings;
  const devices = useDevices();
  const [capturing, setCapturing] = useState(false);
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
      <Field label={`${t.chat.voice.volume} · ${s.volume}%`}>
        <Slider
          value={s.volume}
          min={0}
          max={200}
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
      {s.mode === 'ptt' && (
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
        <Switch label={t.chat.voice.sounds} checked={s.sounds} onChange={(sounds) => save({ sounds })} />
      </div>
    </div>
  );
}
