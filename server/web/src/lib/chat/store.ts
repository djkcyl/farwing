// The chat state of this browser (`docs/protocol/chat-design.md` §6, §12.2):
// one chat connection while signed in, the snapshot it starts from, the
// pushes that follow, loaded messages, typing, presence, unread counts,
// media tokens and notifications. Components read it with `useChat()`.
import { useSyncExternalStore } from 'react';
import { t } from '../../i18n';
import { call } from '../api';
import { sitePath } from '../base';
import { navigate } from '../router';
import { Realtime, reconnectable, type Envelope } from '../realtime';
import { parse, plain } from './markdown';
import {
  idAfter,
  idCmp,
  userName,
  type Channel,
  type ChatUser,
  type Dm,
  type GroupInvite,
  type GroupState,
  type Id,
  type MediaToken,
  type Member,
  type Message,
  type Prefs,
  type Presence,
  type ReadState,
  type Snapshot,
  type Status,
} from './types';

/// A message being sent (not yet answered by the server).
export interface Pending extends Message {
  sending: boolean;
  failed?: string;
}

export interface ChannelMessages {
  list: Pending[];
  /// Older messages exist on the server.
  older: boolean;
  /// Newer messages exist (the list was loaded around an older one).
  newer: boolean;
  loading: boolean;
}

export type Connection = 'connecting' | 'ready' | 'offline' | 'elsewhere';

export interface ChatState {
  connection: Connection;
  snap?: Snapshot;
  presences: Record<string, Status>;
  reads: Record<Id, ReadState>;
  messages: Record<Id, ChannelMessages>;
  /// channel → user → until (ms).
  typing: Record<Id, Record<string, number>>;
  members: Record<Id, Member[]>;
  token?: MediaToken;
  /// The channel being read now.
  focus?: Id;
}

const EMPTY: ChannelMessages = { list: [], older: true, newer: false, loading: false };
const IDLE_MS = 10 * 60 * 1000;
const TYPING_SHOWN_MS = 8000;
const TYPING_EVERY_MS = 4000;
const PAGE = 50;
/// Messages kept per channel; older ones are dropped while reading the newest.
const KEEP = 400;

type Listener = () => void;

class ChatStore {
  state: ChatState = { connection: 'connecting', presences: {}, reads: {}, messages: {}, typing: {}, members: {} };
  private listeners = new Set<Listener>();
  private rt?: Realtime;
  private running = false;
  private retry = 0;
  private retryTimer = 0;
  private idle = false;
  private lastInput = Date.now();
  private idleTimer = 0;
  private tokenTimer = 0;
  private typingSent: Record<Id, number> = {};
  private nonce = 0;

  subscribe = (l: Listener) => {
    this.listeners.add(l);
    return () => {
      this.listeners.delete(l);
    };
  };

  get = () => this.state;

  private set(patch: Partial<ChatState>) {
    this.state = { ...this.state, ...patch };
    this.listeners.forEach((l) => l());
  }

  private setSnap(f: (s: Snapshot) => Snapshot) {
    if (this.state.snap) this.set({ snap: f(this.state.snap) });
  }

  // ------------------------------------------------------------ lifecycle

  start() {
    if (this.running) return;
    this.running = true;
    window.addEventListener('keydown', this.input, true);
    window.addEventListener('pointerdown', this.input, true);
    window.addEventListener('pointermove', this.input, true);
    this.idleTimer = window.setInterval(() => this.checkIdle(), 30_000);
    void this.connect();
  }

  stop() {
    this.running = false;
    window.removeEventListener('keydown', this.input, true);
    window.removeEventListener('pointerdown', this.input, true);
    window.removeEventListener('pointermove', this.input, true);
    window.clearInterval(this.idleTimer);
    window.clearTimeout(this.retryTimer);
    window.clearTimeout(this.tokenTimer);
    this.rt?.bye();
    this.rt = undefined;
    this.state = { connection: 'connecting', presences: {}, reads: {}, messages: {}, typing: {}, members: {} };
    this.listeners.forEach((l) => l());
  }

  /// Take the chat back from another tab of this browser.
  takeOver() {
    this.retry = 0;
    void this.connect();
  }

  private async connect() {
    if (!this.running) return;
    this.set({ connection: this.state.snap ? 'offline' : 'connecting' });
    try {
      const ticket = await call<{ ticket: string }>('POST', '/api/v2/chat/ticket', undefined, { quiet: true });
      const rt = await Realtime.connect(ticket.ticket, {
        message: (e) => this.push(e),
        closed: (code) => this.closed(rt, code),
      });
      this.rt = rt;
      const snap = await rt.request<Snapshot>('chat.hello', { idle: this.idle });
      this.retry = 0;
      const presences: Record<string, Status> = {};
      for (const p of snap.presences) presences[p.user_id] = p.status;
      presences[snap.me.user_id] = snap.status;
      const reads: Record<Id, ReadState> = {};
      for (const r of snap.read_states) reads[r.channel_id] = r;
      // Loaded channels may have missed messages: they load again.
      this.set({ connection: 'ready', snap, presences, reads, messages: {}, members: {} });
      void this.refreshToken();
    } catch {
      this.scheduleRetry();
    }
  }

  private closed(rt: Realtime, code: number) {
    if (rt !== this.rt) return;
    this.rt = undefined;
    if (code === 4001) {
      this.set({ connection: 'elsewhere' });
      return;
    }
    if (!reconnectable(code) && code !== 4000) {
      this.set({ connection: 'offline' });
      return;
    }
    this.scheduleRetry();
  }

  private scheduleRetry() {
    if (!this.running) return;
    this.set({ connection: 'offline' });
    const delay = Math.min(30_000, 1000 * 2 ** this.retry);
    this.retry++;
    window.clearTimeout(this.retryTimer);
    this.retryTimer = window.setTimeout(() => void this.connect(), delay);
  }

  private input = () => {
    this.lastInput = Date.now();
    if (this.idle) this.setIdle(false);
  };

  private checkIdle() {
    if (!this.idle && Date.now() - this.lastInput > IDLE_MS) this.setIdle(true);
    // Typing notices past their time.
    const now = Date.now();
    let changed = false;
    const typing: ChatState['typing'] = {};
    for (const [ch, users] of Object.entries(this.state.typing)) {
      const left = Object.fromEntries(Object.entries(users).filter(([, until]) => until > now));
      if (Object.keys(left).length !== Object.keys(users).length) changed = true;
      if (Object.keys(left).length) typing[ch] = left;
    }
    if (changed) this.set({ typing });
  }

  private setIdle(idle: boolean) {
    this.idle = idle;
    this.rt?.send('chat.idle', { idle });
  }

  private async refreshToken() {
    window.clearTimeout(this.tokenTimer);
    try {
      const token = await call<MediaToken>('POST', '/api/v2/chat/media-token', undefined, { quiet: true });
      this.set({ token });
      const left = token.expires_at * 1000 - Date.now();
      this.tokenTimer = window.setTimeout(() => void this.refreshToken(), Math.max(60_000, left - 3600_000));
    } catch {
      this.tokenTimer = window.setTimeout(() => void this.refreshToken(), 60_000);
    }
  }

  /// A `/media/...` URL with the media token.
  media(path: string): string | undefined {
    const token = this.state.token?.token;
    if (!token) return undefined;
    return sitePath(`${path}${path.includes('?') ? '&' : '?'}t=${encodeURIComponent(token)}`);
  }

  // ---------------------------------------------------------------- pushes

  private push(e: Envelope) {
    const data = e.data as never;
    switch (e.type) {
      case 'chat.message':
        return this.onMessage(data as Message);
      case 'chat.message_updated':
        return this.replaceMessage(data as Message);
      case 'chat.message_deleted': {
        const d = data as { channel_id: Id; id: Id };
        const m = this.state.messages[d.channel_id]?.list.find((x) => x.id === d.id);
        if (m) this.replaceMessage({ ...m, deleted: true, content: '', attachments: [], pinned: false });
        return;
      }
      case 'chat.pins':
        window.dispatchEvent(new CustomEvent('farwing-chat-pins', { detail: (data as { channel_id: Id }).channel_id }));
        return;
      case 'chat.typing': {
        const d = data as { channel_id: Id; user_id: string };
        const users = { ...(this.state.typing[d.channel_id] ?? {}), [d.user_id]: Date.now() + TYPING_SHOWN_MS };
        this.set({ typing: { ...this.state.typing, [d.channel_id]: users } });
        window.setTimeout(() => this.checkIdle(), TYPING_SHOWN_MS + 100);
        return;
      }
      case 'chat.read': {
        const d = data as { channel_id: Id; last_read_id: Id; mentions: number; user_id?: string };
        if (d.user_id) {
          this.setSnap((s) => ({
            ...s,
            dms: s.dms.map((dm) => (dm.channel.id === d.channel_id ? { ...dm, peer_read_id: d.last_read_id } : dm)),
          }));
        } else {
          this.set({ reads: { ...this.state.reads, [d.channel_id]: d } });
        }
        return;
      }
      case 'chat.group': {
        const g = data as GroupState;
        this.setSnap((s) => {
          const at = s.groups.findIndex((x) => x.group.id === g.group.id);
          const groups = at >= 0 ? s.groups.map((x, i) => (i === at ? g : x)) : [...s.groups, g];
          return { ...s, groups };
        });
        return;
      }
      case 'chat.group_removed': {
        const d = data as { group_id: Id };
        this.setSnap((s) => ({ ...s, groups: s.groups.filter((g) => g.group.id !== d.group_id) }));
        return;
      }
      case 'chat.member': {
        const d = data as { group_id: Id; member: Member };
        const list = this.state.members[d.group_id];
        if (list) {
          const others = list.filter((m) => m.user.user_id !== d.member.user.user_id);
          this.set({ members: { ...this.state.members, [d.group_id]: [...others, d.member] } });
        }
        return;
      }
      case 'chat.member_removed': {
        const d = data as { group_id: Id; user_id: string };
        const list = this.state.members[d.group_id];
        if (list) {
          this.set({
            members: { ...this.state.members, [d.group_id]: list.filter((m) => m.user.user_id !== d.user_id) },
          });
        }
        return;
      }
      case 'chat.presence': {
        const p = data as Presence;
        this.set({ presences: { ...this.state.presences, [p.user_id]: p.status } });
        return;
      }
      case 'chat.user':
        return this.onUser(data as ChatUser);
      case 'chat.dm': {
        const dm = data as Dm;
        this.setSnap((s) => {
          const at = s.dms.findIndex((x) => x.channel.id === dm.channel.id);
          return { ...s, dms: at >= 0 ? s.dms.map((x, i) => (i === at ? dm : x)) : [...s.dms, dm] };
        });
        return;
      }
      case 'chat.invite': {
        const inv = data as GroupInvite;
        this.setSnap((s) => ({ ...s, group_invites: [...s.group_invites.filter((i) => i.id !== inv.id), inv] }));
        this.notify(t().chat.notify.invite, `${userName(inv.by)} · ${inv.group.name}`, '/chat/dm');
        return;
      }
      case 'chat.invite_removed': {
        const d = data as { id: Id };
        this.setSnap((s) => ({ ...s, group_invites: s.group_invites.filter((i) => i.id !== d.id) }));
        return;
      }
      case 'chat.request':
        window.dispatchEvent(new CustomEvent('farwing-chat-request', { detail: data }));
        return;
      case 'chat.prefs':
        this.setSnap((s) => ({ ...s, prefs: data as Prefs }));
        return;
    }
  }

  private onUser(u: ChatUser) {
    const fix = (x: ChatUser) => (x.user_id === u.user_id ? u : x);
    this.setSnap((s) => ({
      ...s,
      me: fix(s.me),
      dms: s.dms.map((d) => ({ ...d, user: fix(d.user) })),
      groups: s.groups.map((g) => ({ ...g, me: { ...g.me, user: fix(g.me.user) } })),
    }));
    const members: ChatState['members'] = {};
    for (const [g, list] of Object.entries(this.state.members))
      members[g] = list.map((m) => ({ ...m, user: fix(m.user) }));
    this.set({ members });
  }

  /// The channel (and its group) by ID.
  channel(id: Id): { channel: Channel; group?: GroupState; dm?: Dm } | undefined {
    const s = this.state.snap;
    if (!s) return undefined;
    for (const g of s.groups) {
      const channel = g.channels.find((c) => c.id === id);
      if (channel) return { channel, group: g };
    }
    const dm = s.dms.find((d) => d.channel.id === id);
    return dm ? { channel: dm.channel, dm } : undefined;
  }

  private bumpLast(channel: Id, id: Id) {
    this.setSnap((s) => ({
      ...s,
      groups: s.groups.map((g) =>
        g.channels.some((c) => c.id === channel)
          ? {
              ...g,
              channels: g.channels.map((c) =>
                c.id === channel && idAfter(id, c.last_message_id) ? { ...c, last_message_id: id } : c,
              ),
            }
          : g,
      ),
      dms: s.dms.map((d) =>
        d.channel.id === channel && idAfter(id, d.channel.last_message_id)
          ? { ...d, channel: { ...d.channel, last_message_id: id } }
          : d,
      ),
    }));
  }

  private mentionsMe(m: Message, group?: GroupState): boolean {
    const me = this.state.snap?.me.user_id;
    if (!me || m.author.user_id === me) return false;
    return (
      m.mentions.users.includes(me) ||
      m.mentions.everyone ||
      m.mentions.here ||
      (!!group && m.mentions.roles.some((r) => group.me.roles.includes(r)))
    );
  }

  private onMessage(m: Message) {
    const me = this.state.snap?.me.user_id;
    const own = m.author.user_id === me;
    const box = this.state.messages[m.channel_id];
    if (box && !box.newer) {
      const list = box.list.filter((x) => x.id !== m.id && !(own && m.nonce && x.nonce === m.nonce));
      list.push({ ...m, sending: false });
      list.sort((a, b) => idCmp(a.id, b.id));
      this.setMessages(m.channel_id, { ...box, list: list.slice(-KEEP), older: box.older || list.length > KEEP });
    }
    this.bumpLast(m.channel_id, m.id);
    const where = this.channel(m.channel_id);
    if (own) {
      const read = { channel_id: m.channel_id, last_read_id: m.id, mentions: 0 };
      this.set({ reads: { ...this.state.reads, [m.channel_id]: read } });
      return;
    }
    const mention = this.mentionsMe(m, where?.group);
    if (mention && this.state.focus !== m.channel_id) {
      const r = this.state.reads[m.channel_id] ?? { channel_id: m.channel_id, last_read_id: '0', mentions: 0 };
      this.set({ reads: { ...this.state.reads, [m.channel_id]: { ...r, mentions: r.mentions + 1 } } });
    }
    // Typing stops when the message comes.
    const typing = this.state.typing[m.channel_id];
    if (typing?.[m.author.user_id]) {
      const rest = { ...typing };
      delete rest[m.author.user_id];
      this.set({ typing: { ...this.state.typing, [m.channel_id]: rest } });
    }
    if (where && (document.hidden || this.state.focus !== m.channel_id) && this.shouldNotify(m, where, mention)) {
      const title = where.dm
        ? userName(where.dm.user)
        : `${where.group?.group.name ?? ''} · ${where.channel.name} · ${userName(m.author)}`;
      const body = this.state.snap?.prefs.hide_content ? t().chat.notify.newMessage : this.preview(m, where.group);
      this.notify(
        title,
        body,
        where.dm ? `/chat/dm/${m.channel_id}` : `/chat/${where.group?.group.id}/${m.channel_id}`,
      );
    }
  }

  /// The setting that decides for a channel: its own, its group's, the default.
  notifyLevel(channel: Id, group?: Id): 'all' | 'mentions' | 'none' {
    const prefs = this.state.snap?.prefs;
    return prefs?.notify[`c:${channel}`] ?? (group ? (prefs?.notify[`g:${group}`] ?? 'mentions') : 'all');
  }

  private shouldNotify(m: Message, where: { group?: GroupState; dm?: Dm }, mention: boolean): boolean {
    const s = this.state.snap;
    if (!s || this.state.presences[s.me.user_id] === 'dnd' || s.status === 'dnd') return false;
    if (quiet(s.prefs.quiet_start, s.prefs.quiet_end)) return false;
    const level = this.notifyLevel(m.channel_id, where.group?.group.id);
    return level === 'all' || (level === 'mentions' && mention);
  }

  /// The beginning of a message in plain words.
  preview(m: Message, group?: GroupState): string {
    const names = this.names(group);
    const text = parse(m.content)
      .map((b) => (b.t === 'pre' ? b.v : plain(b.c, names)))
      .join(' ');
    const files = m.attachments.length ? t().chat.attachmentCount(m.attachments.length) : '';
    return [text, files].filter(Boolean).join(' ').slice(0, 60);
  }

  names(group?: GroupState) {
    const members = group ? this.state.members[group.group.id] : undefined;
    return {
      user: (id: string) => {
        const m = members?.find((x) => x.user.user_id === id);
        if (m) return m.nickname || userName(m.user);
        if (this.state.snap?.me.user_id === id) return userName(this.state.snap.me);
        const dm = this.state.snap?.dms.find((d) => d.user.user_id === id);
        return dm ? userName(dm.user) : t().chat.unknownUser;
      },
      role: (id: string) => group?.roles.find((r) => r.id === id)?.name ?? t().chat.unknownRole,
      channel: (id: string) => {
        for (const g of this.state.snap?.groups ?? []) {
          const c = g.channels.find((x) => x.id === id);
          if (c) return c.name;
        }
        return t().chat.unknownChannel;
      },
    };
  }

  private notify(title: string, body: string, to: string) {
    if (typeof Notification === 'undefined' || Notification.permission !== 'granted') return;
    try {
      const n = new Notification(title, { body, tag: to });
      n.onclick = () => {
        window.focus();
        navigate(to);
        n.close();
      };
    } catch {
      // Some browsers only notify from a service worker.
    }
  }

  // -------------------------------------------------------------- messages

  private setMessages(channel: Id, box: ChannelMessages) {
    this.set({ messages: { ...this.state.messages, [channel]: box } });
  }

  messagesOf(channel: Id): ChannelMessages {
    return this.state.messages[channel] ?? EMPTY;
  }

  private replaceMessage(m: Message) {
    const box = this.state.messages[m.channel_id];
    if (!box) return;
    this.setMessages(m.channel_id, {
      ...box,
      list: box.list.map((x) => (x.id === m.id ? { ...m, sending: false } : x)),
    });
  }

  async loadLatest(channel: Id) {
    const box = this.messagesOf(channel);
    if (box.loading) return;
    this.setMessages(channel, { ...box, loading: true });
    try {
      const page = await call<{ messages: Message[] }>(
        'GET',
        `/api/v2/chat/channels/${channel}/messages?limit=${PAGE}`,
      );
      const pending = this.messagesOf(channel).list.filter((x) => x.sending || x.failed);
      this.setMessages(channel, {
        list: [...page.messages.map((m) => ({ ...m, sending: false })), ...pending],
        older: page.messages.length === PAGE,
        newer: false,
        loading: false,
      });
    } catch {
      this.setMessages(channel, { ...this.messagesOf(channel), loading: false });
    }
  }

  async loadOlder(channel: Id) {
    const box = this.messagesOf(channel);
    const first = box.list.find((m) => !m.sending);
    if (box.loading || !box.older || !first) return;
    this.setMessages(channel, { ...box, loading: true });
    try {
      const page = await call<{ messages: Message[] }>(
        'GET',
        `/api/v2/chat/channels/${channel}/messages?before=${first.id}&limit=${PAGE}`,
      );
      const now = this.messagesOf(channel);
      this.setMessages(channel, {
        ...now,
        list: [...page.messages.map((m) => ({ ...m, sending: false })), ...now.list],
        older: page.messages.length === PAGE,
        loading: false,
      });
    } catch {
      this.setMessages(channel, { ...this.messagesOf(channel), loading: false });
    }
  }

  async loadNewer(channel: Id) {
    const box = this.messagesOf(channel);
    const last = [...box.list].reverse().find((m) => !m.sending);
    if (box.loading || !box.newer || !last) return;
    this.setMessages(channel, { ...box, loading: true });
    try {
      const page = await call<{ messages: Message[] }>(
        'GET',
        `/api/v2/chat/channels/${channel}/messages?after=${last.id}&limit=${PAGE}`,
      );
      const now = this.messagesOf(channel);
      this.setMessages(channel, {
        ...now,
        list: [...now.list, ...page.messages.map((m) => ({ ...m, sending: false }))],
        newer: page.messages.length === PAGE,
        loading: false,
      });
    } catch {
      this.setMessages(channel, { ...this.messagesOf(channel), loading: false });
    }
  }

  /// Load the messages around `id` (a jump to a reply, a pin, a result).
  async loadAround(channel: Id, id: Id) {
    const box = this.messagesOf(channel);
    if (box.list.some((m) => m.id === id)) return;
    this.setMessages(channel, { ...box, loading: true });
    try {
      const page = await call<{ messages: Message[] }>(
        'GET',
        `/api/v2/chat/channels/${channel}/messages?around=${id}&limit=${PAGE}`,
      );
      const last = this.channel(channel)?.channel.last_message_id;
      const newest = page.messages[page.messages.length - 1]?.id;
      this.setMessages(channel, {
        list: page.messages.map((m) => ({ ...m, sending: false })),
        older: true,
        newer: !!last && !!newest && idCmp(newest, last) < 0,
        loading: false,
      });
    } catch {
      this.setMessages(channel, { ...this.messagesOf(channel), loading: false });
    }
  }

  /// Send a message: shown at once, confirmed by the server's answer.
  async send(channel: Id, content: string, opts: { reply?: Message; mentionReply?: boolean; attachments?: Id[] } = {}) {
    const me = this.state.snap?.me;
    if (!me) return;
    const nonce = `w${Date.now().toString(36)}${(this.nonce++).toString(36)}`;
    const local: Pending = {
      id: `9${'9'.repeat(18)}${this.nonce}`,
      channel_id: channel,
      author: me,
      content,
      reply: opts.reply
        ? {
            id: opts.reply.id,
            author: opts.reply.author,
            content: opts.reply.content.slice(0, 120),
            deleted: false,
            attachments: 0,
          }
        : null,
      mentions: { users: [], roles: [], everyone: false, here: false },
      attachments: [],
      pinned: false,
      edited_at: null,
      deleted: false,
      created_at: Date.now(),
      nonce,
      sending: true,
    };
    const box = this.messagesOf(channel);
    this.setMessages(channel, { ...box, list: [...box.list, local] });
    const body = {
      content,
      reply_to: opts.reply?.id ?? null,
      mention_reply: opts.mentionReply ?? true,
      attachments: opts.attachments ?? [],
      nonce,
    };
    try {
      const sent = await call<Message>('POST', `/api/v2/chat/channels/${channel}/messages`, body);
      this.onMessage({ ...sent, nonce });
      const now = this.messagesOf(channel);
      this.setMessages(channel, { ...now, list: now.list.filter((x) => x.nonce !== nonce || x.id === sent.id) });
    } catch (e) {
      const now = this.messagesOf(channel);
      const failed = (e as Error)?.message ?? '';
      this.setMessages(channel, {
        ...now,
        list: now.list.map((x) => (x.nonce === nonce && x.sending ? { ...x, sending: false, failed } : x)),
      });
      throw e;
    }
  }

  /// Send a failed message again.
  async resend(channel: Id, m: Pending) {
    const now = this.messagesOf(channel);
    this.setMessages(channel, { ...now, list: now.list.filter((x) => x !== m) });
    await this.send(channel, m.content);
  }

  discard(channel: Id, m: Pending) {
    const now = this.messagesOf(channel);
    this.setMessages(channel, { ...now, list: now.list.filter((x) => x !== m) });
  }

  /// Tell the others this user types (at most every 4 seconds).
  typing(channel: Id) {
    const now = Date.now();
    if ((this.typingSent[channel] ?? 0) + TYPING_EVERY_MS > now) return;
    this.typingSent[channel] = now;
    this.rt?.send('chat.typing', { channel_id: channel });
  }

  setFocus(channel: Id | undefined) {
    if (this.state.focus !== channel) this.set({ focus: channel });
  }

  /// Read up to the newest message of `channel` (when that moves on).
  async markRead(channel: Id) {
    const last = this.channel(channel)?.channel.last_message_id;
    const read = this.state.reads[channel];
    if (!last || (read && !idAfter(last, read.last_read_id) && read.mentions === 0)) return;
    this.set({ reads: { ...this.state.reads, [channel]: { channel_id: channel, last_read_id: last, mentions: 0 } } });
    try {
      await call('POST', `/api/v2/chat/channels/${channel}/read`, { message_id: last }, { quiet: true });
    } catch {
      // Read again next time.
    }
  }

  unread(channel: Channel): { unread: boolean; mentions: number } {
    const r = this.state.reads[channel.id];
    return {
      unread: idAfter(channel.last_message_id, r?.last_read_id ?? '0'),
      mentions: r?.mentions ?? 0,
    };
  }

  // --------------------------------------------------------------- members

  async loadMembers(group: Id) {
    const all: Member[] = [];
    let after = '';
    try {
      for (;;) {
        const page = await call<{ members: Member[] }>(
          'GET',
          `/api/v2/chat/groups/${group}/members?limit=1000${after ? `&after=${after}` : ''}`,
        );
        all.push(...page.members);
        if (page.members.length < 1000) break;
        after = page.members[page.members.length - 1].user.user_id;
      }
      this.set({ members: { ...this.state.members, [group]: all } });
    } catch {
      // Shown as not loaded.
    }
  }

  // ------------------------------------------------------------- the rest

  /// A group's state as answered by a call (before its push arrives).
  upsertGroup(g: GroupState) {
    this.push({ v: 3, type: 'chat.group', data: g });
  }

  upsertDm(dm: Dm) {
    this.push({ v: 3, type: 'chat.dm', data: dm });
  }

  removeGroup(id: Id) {
    this.push({ v: 3, type: 'chat.group_removed', data: { group_id: id } });
  }

  async setStatus(status: Status) {
    await call('PUT', '/api/v2/me/status', { status });
    this.setSnap((s) => ({ ...s, status }));
  }

  async savePrefs(prefs: Prefs) {
    await call('PUT', '/api/v2/chat/prefs', prefs);
    this.setSnap((s) => ({ ...s, prefs }));
  }

  /// Totals for the sidebar: unread mentions and whether anything is unread.
  totals(): { mentions: number; unread: boolean } {
    const s = this.state.snap;
    if (!s) return { mentions: 0, unread: false };
    // Mentions in groups, each unread direct message, each invitation.
    let mentions = s.group_invites.length;
    let unread = false;
    for (const c of s.groups.flatMap((g) => g.channels)) {
      const u = this.unread(c);
      mentions += u.mentions;
      if (u.unread && this.notifyLevel(c.id, c.group_id ?? undefined) !== 'none') unread = true;
    }
    for (const d of s.dms) {
      if (this.unread(d.channel).unread && this.notifyLevel(d.channel.id) !== 'none') mentions += 1;
    }
    return { mentions, unread };
  }
}

/// Whether now (local time) is within the quiet hours `start`–`end`.
export function quiet(start: string, end: string, now = new Date()): boolean {
  if (!start || !end) return false;
  const minutes = (hm: string) => {
    const [h, m] = hm.split(':').map(Number);
    return h * 60 + m;
  };
  const a = minutes(start);
  const b = minutes(end);
  const n = now.getHours() * 60 + now.getMinutes();
  return a <= b ? n >= a && n < b : n >= a || n < b;
}

export const chat = new ChatStore();

export function useChat(): ChatState {
  return useSyncExternalStore(chat.subscribe, chat.get);
}
