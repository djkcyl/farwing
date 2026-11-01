//! What the device center knows of chat (`docs/protocol/chat-design.md`
//! §6): the snapshot the chat connection started from, the pushes since,
//! loaded messages, typing, presence and unread counts. Pure data and the
//! reducer of pushes; the connection and requests are in `service.rs`.
use super::markdown::Names;
use farwing_protocol::chat::{
    Channel, ChatUser, Dm, GroupInvite, GroupState, Id, MediaToken, Member, Message, Notify, Prefs,
    Presence, ReadState, Snapshot, Status, types,
};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Typing notices are shown this long.
pub(crate) const TYPING_SHOWN: Duration = Duration::from_secs(8);
/// Messages kept per channel while reading the newest.
pub(crate) const KEEP: usize = 400;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum Connection {
    #[default]
    Connecting,
    Ready,
    Offline,
    /// Another device-center window of this device took the connection.
    Elsewhere,
}

/// A message, perhaps still being sent.
#[derive(Clone, Debug)]
pub(crate) struct Entry {
    pub message: Message,
    pub sending: bool,
    pub failed: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) struct ChannelMessages {
    pub list: Vec<Entry>,
    /// Older messages exist on the server.
    pub older: bool,
    /// Newer ones exist (loaded around an older message).
    pub newer: bool,
    pub loading: bool,
    /// Shown from the local cache; the server's answer replaces it.
    pub cached: bool,
}

impl Default for ChannelMessages {
    fn default() -> Self {
        Self {
            list: Vec::new(),
            older: true,
            newer: false,
            loading: false,
            cached: false,
        }
    }
}

/// The answer to a request the interface waits for (dialogs).
#[derive(Clone, Debug)]
pub(crate) enum Answer {
    Waiting,
    Done(serde_json::Value),
    Failed(String),
}

#[derive(Clone, Default)]
pub(crate) struct ChatState {
    pub connection: Connection,
    pub snap: Option<Snapshot>,
    pub presences: HashMap<String, Status>,
    pub reads: HashMap<Id, ReadState>,
    pub messages: HashMap<Id, ChannelMessages>,
    pub typing: HashMap<Id, HashMap<String, Instant>>,
    pub members: HashMap<Id, Vec<Member>>,
    pub focus: Option<Id>,
    pub token: Option<MediaToken>,
    pub answers: HashMap<String, Answer>,
    /// Failures to show once.
    pub errors: Vec<String>,
    /// The picker's index.
    pub emoji: std::sync::Arc<Vec<super::emoji::Entry>>,
    /// A message to bring into view (a jump to a reply, pin or result).
    pub jump: Option<(Id, Id, Instant)>,
    /// This device in a voice channel or call.
    pub my_voice: Option<MyVoice>,
}

/// This device's voice: where, and how it goes.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct MyVoice {
    /// The voice channel, or the direct message of a call.
    pub channel: Id,
    /// The call's ID (a call rather than a channel).
    pub call: Option<String>,
    pub participant: u32,
    pub connected: bool,
    pub muted: bool,
    pub deafened: bool,
    pub compat: bool,
}

/// A notification a push asks for.
pub(crate) struct Notice {
    pub title: String,
    pub body: String,
    pub channel: Id,
    /// A message (it has a sound).
    pub message: bool,
}

pub(crate) fn id_after(a: Option<Id>, b: Option<Id>) -> bool {
    match (a, b) {
        (Some(a), Some(b)) => a > b,
        (Some(_), None) => true,
        _ => false,
    }
}

impl ChatState {
    /// Do not disturb, or within its hours: nothing rings.
    pub(crate) fn do_not_disturb(&self) -> bool {
        self.snap
            .as_ref()
            .is_some_and(|s| s.status == Status::Dnd || quiet(&s.prefs, chrono::Local::now().time()))
    }

    pub(crate) fn me(&self) -> Option<&ChatUser> {
        self.snap.as_ref().map(|s| &s.me)
    }

    /// The channel and the group or direct message it belongs to.
    pub(crate) fn channel(&self, id: Id) -> Option<(&Channel, Option<&GroupState>, Option<&Dm>)> {
        let s = self.snap.as_ref()?;
        for g in &s.groups {
            if let Some(c) = g.channels.iter().find(|c| c.id == id) {
                return Some((c, Some(g), None));
            }
        }
        s.dms.iter().find(|d| d.channel.id == id).map(|d| (&d.channel, None, Some(d)))
    }

    pub(crate) fn group(&self, id: Id) -> Option<&GroupState> {
        self.snap.as_ref()?.groups.iter().find(|g| g.group.id == id)
    }

    pub(crate) fn unread(&self, channel: &Channel) -> (bool, u32) {
        let read = self.reads.get(&channel.id);
        (
            id_after(channel.last_message_id, Some(read.map_or(Id(0), |r| r.last_read_id))),
            read.map_or(0, |r| r.mentions),
        )
    }

    /// The notification setting that applies: the channel's, the group's,
    /// the default.
    pub(crate) fn notify_level(&self, channel: Id, group: Option<Id>) -> Notify {
        let Some(prefs) = self.snap.as_ref().map(|s| &s.prefs) else {
            return Notify::Mentions;
        };
        if let Some(n) = prefs.notify.get(&format!("c:{channel}")) {
            return *n;
        }
        match group {
            Some(g) => prefs.notify.get(&format!("g:{g}")).copied().unwrap_or(Notify::Mentions),
            None => Notify::All,
        }
    }

    /// For the sidebar: unread mentions (and direct messages, invitations),
    /// and whether anything is unread.
    pub(crate) fn totals(&self) -> (u32, bool) {
        let Some(s) = &self.snap else { return (0, false) };
        let mut count = s.group_invites.len() as u32;
        let mut any = false;
        for g in &s.groups {
            for c in &g.channels {
                let (unread, mentions) = self.unread(c);
                count += mentions;
                any |= unread && self.notify_level(c.id, Some(g.group.id)) != Notify::None;
            }
        }
        for d in &s.dms {
            if self.unread(&d.channel).0 && self.notify_level(d.channel.id, None) != Notify::None {
                count += 1;
            }
        }
        (count, any)
    }

    pub(crate) fn names<'a>(&'a self, group: Option<&'a GroupState>) -> StateNames<'a> {
        StateNames { state: self, group }
    }

    fn set_last(&mut self, channel: Id, id: Id) {
        let Some(s) = &mut self.snap else { return };
        for g in &mut s.groups {
            for c in &mut g.channels {
                if c.id == channel && id_after(Some(id), c.last_message_id) {
                    c.last_message_id = Some(id);
                }
            }
        }
        for d in &mut s.dms {
            if d.channel.id == channel && id_after(Some(id), d.channel.last_message_id) {
                d.channel.last_message_id = Some(id);
            }
        }
    }

    fn mentions_me(&self, m: &Message, group: Option<&GroupState>) -> bool {
        let Some(me) = self.me() else { return false };
        if m.author.user_id == me.user_id {
            return false;
        }
        m.mentions.users.contains(&me.user_id)
            || m.mentions.everyone
            || m.mentions.here
            || group.is_some_and(|g| m.mentions.roles.iter().any(|r| g.me.roles.contains(r)))
    }

    /// A new message (pushed, or the answer to this device's send).
    pub(crate) fn add_message(&mut self, m: Message, window_hidden: bool) -> Option<Notice> {
        let own = self.me().is_some_and(|me| me.user_id == m.author.user_id);
        if let Some(box_) = self.messages.get_mut(&m.channel_id)
            && !box_.newer
        {
            box_.list
                .retain(|e| e.message.id != m.id && !(own && !m.nonce.is_empty() && e.message.nonce == m.nonce));
            box_.list.push(Entry {
                message: m.clone(),
                sending: false,
                failed: None,
            });
            box_.list.sort_by_key(|e| (e.sending, e.message.id));
            if box_.list.len() > KEEP {
                let drop = box_.list.len() - KEEP;
                box_.list.drain(..drop);
                box_.older = true;
            }
        }
        self.set_last(m.channel_id, m.id);
        if let Some(t) = self.typing.get_mut(&m.channel_id) {
            t.remove(&m.author.user_id);
        }
        if own {
            self.reads.insert(
                m.channel_id,
                ReadState {
                    channel_id: m.channel_id,
                    last_read_id: m.id,
                    mentions: 0,
                },
            );
            return None;
        }
        let (group, dm) = match self.channel(m.channel_id) {
            Some((_, g, d)) => (g.cloned(), d.cloned()),
            None => return None,
        };
        let mention = self.mentions_me(&m, group.as_ref());
        let focused = self.focus == Some(m.channel_id) && !window_hidden;
        if mention && !focused {
            let r = self.reads.entry(m.channel_id).or_insert(ReadState {
                channel_id: m.channel_id,
                ..Default::default()
            });
            r.mentions += 1;
        }
        if focused {
            return None;
        }
        let snap = self.snap.as_ref()?;
        if snap.status == Status::Dnd || quiet(&snap.prefs, chrono::Local::now().time()) {
            return None;
        }
        let level = self.notify_level(m.channel_id, group.as_ref().map(|g| g.group.id));
        if !(level == Notify::All || (level == Notify::Mentions && mention)) {
            return None;
        }
        let names = self.names(group.as_ref());
        let title = match (&dm, &group) {
            (Some(d), _) => d.user.display_name().to_owned(),
            (None, Some(g)) => {
                let channel = g.channels.iter().find(|c| c.id == m.channel_id).map_or("", |c| c.name.as_str());
                let author = self
                    .members
                    .get(&g.group.id)
                    .and_then(|l| l.iter().find(|x| x.user.user_id == m.author.user_id))
                    .filter(|x| !x.nickname.is_empty())
                    .map_or_else(|| m.author.display_name().to_owned(), |x| x.nickname.clone());
                format!("{} · {} · {}", g.group.name, channel, author)
            }
            _ => return None,
        };
        let body = if snap.prefs.hide_content {
            crate::i18n::t().chat.new_message.to_owned()
        } else {
            preview(&m, &names)
        };
        Some(Notice {
            title,
            body,
            channel: m.channel_id,
            message: true,
        })
    }

    pub(crate) fn replace_message(&mut self, m: Message) {
        if let Some(b) = self.messages.get_mut(&m.channel_id) {
            for e in &mut b.list {
                if e.message.id == m.id {
                    e.message = m.clone();
                }
            }
        }
    }

    /// Apply a push of the chat connection; a notification to show, if any.
    pub(crate) fn apply(&mut self, kind: &str, data: serde_json::Value, window_hidden: bool) -> Option<Notice> {
        fn body<T: serde::de::DeserializeOwned>(data: serde_json::Value) -> Option<T> {
            serde_json::from_value(data).ok()
        }
        match kind {
            types::MESSAGE => return self.add_message(body(data)?, window_hidden),
            types::VOICE_STATE => {
                let d: farwing_protocol::chat::VoiceChannelState = body(data)?;
                if let Some(snap) = &mut self.snap {
                    snap.voice.retain(|v| v.channel_id != d.channel_id);
                    if !d.participants.is_empty() {
                        snap.voice.push(d);
                    }
                }
            }
            types::CALL_RING | types::CALL_OUTGOING | types::CALL_ANSWERED => {
                let call: farwing_protocol::chat::Call = body(data)?;
                // Do not disturb (or its hours): it rings unanswered.
                let ringing = kind == types::CALL_RING && !self.do_not_disturb();
                let notice = ringing.then(|| Notice {
                    title: call.caller.display_name().to_owned(),
                    body: (crate::i18n::t().voice.incoming)(call.caller.display_name()),
                    channel: call.channel_id,
                    message: false,
                });
                if let Some(snap) = &mut self.snap {
                    snap.calls.retain(|c| c.id != call.id);
                    snap.calls.push(call);
                }
                return notice;
            }
            types::CALL_ENDED => {
                let d: farwing_protocol::chat::CallEnded = body(data)?;
                if let Some(snap) = &mut self.snap {
                    snap.calls.retain(|c| c.id != d.call_id);
                }
            }
            types::MESSAGE_UPDATED => self.replace_message(body(data)?),
            types::MESSAGE_DELETED => {
                let d: farwing_protocol::chat::MessageDeleted = body(data)?;
                if let Some(b) = self.messages.get_mut(&d.channel_id) {
                    for e in &mut b.list {
                        if e.message.id == d.id {
                            e.message.deleted = true;
                            e.message.content.clear();
                            e.message.attachments.clear();
                            e.message.pinned = false;
                        }
                    }
                }
            }
            types::TYPING => {
                let d: farwing_protocol::chat::Typing = body(data)?;
                self.typing.entry(d.channel_id).or_default().insert(d.user_id, Instant::now());
            }
            types::READ => {
                let d: farwing_protocol::chat::ReadPush = body(data)?;
                if d.user_id.is_empty() {
                    self.reads.insert(
                        d.channel_id,
                        ReadState {
                            channel_id: d.channel_id,
                            last_read_id: d.last_read_id,
                            mentions: d.mentions,
                        },
                    );
                } else if let Some(s) = &mut self.snap {
                    for dm in &mut s.dms {
                        if dm.channel.id == d.channel_id {
                            dm.peer_read_id = Some(d.last_read_id);
                        }
                    }
                }
            }
            types::GROUP => {
                let g: GroupState = body(data)?;
                let s = self.snap.as_mut()?;
                match s.groups.iter_mut().find(|x| x.group.id == g.group.id) {
                    Some(x) => *x = g,
                    None => s.groups.push(g),
                }
            }
            types::GROUP_REMOVED => {
                let d: farwing_protocol::chat::GroupRemoved = body(data)?;
                let s = self.snap.as_mut()?;
                s.groups.retain(|g| g.group.id != d.group_id);
                self.members.remove(&d.group_id);
            }
            types::MEMBER => {
                let d: farwing_protocol::chat::MemberPush = body(data)?;
                if let Some(list) = self.members.get_mut(&d.group_id) {
                    list.retain(|m| m.user.user_id != d.member.user.user_id);
                    list.push(d.member);
                }
            }
            types::MEMBER_REMOVED => {
                let d: farwing_protocol::chat::MemberRemoved = body(data)?;
                if let Some(list) = self.members.get_mut(&d.group_id) {
                    list.retain(|m| m.user.user_id != d.user_id);
                }
            }
            types::PRESENCE => {
                let p: Presence = body(data)?;
                self.presences.insert(p.user_id, p.status);
            }
            types::USER => {
                let u: ChatUser = body(data)?;
                let fix = |x: &mut ChatUser| {
                    if x.user_id == u.user_id {
                        *x = u.clone();
                    }
                };
                if let Some(s) = &mut self.snap {
                    fix(&mut s.me);
                    s.dms.iter_mut().for_each(|d| fix(&mut d.user));
                    s.groups.iter_mut().for_each(|g| fix(&mut g.me.user));
                }
                for list in self.members.values_mut() {
                    list.iter_mut().for_each(|m| fix(&mut m.user));
                }
            }
            types::DM => {
                let dm: Dm = body(data)?;
                let s = self.snap.as_mut()?;
                match s.dms.iter_mut().find(|x| x.channel.id == dm.channel.id) {
                    Some(x) => *x = dm,
                    None => s.dms.push(dm),
                }
            }
            types::INVITE => {
                let inv: GroupInvite = body(data)?;
                let s = self.snap.as_mut()?;
                s.group_invites.retain(|i| i.id != inv.id);
                let notice = Notice {
                    title: crate::i18n::t().chat.invite_title.to_owned(),
                    body: format!("{} · {}", inv.by.display_name(), inv.group.name),
                    channel: Id(0),
                    message: false,
                };
                s.group_invites.push(inv);
                return Some(notice);
            }
            types::INVITE_REMOVED => {
                let d: farwing_protocol::chat::InviteRemoved = body(data)?;
                self.snap.as_mut()?.group_invites.retain(|i| i.id != d.id);
            }
            types::REQUEST => {
                let d: farwing_protocol::chat::RequestPush = body(data)?;
                let text = &crate::i18n::t().chat;
                let msg = match &d.user {
                    Some(u) => (text.request_from)(&u.display_name(), &d.group.name),
                    None if d.approved => (text.join_approved)(&d.group.name),
                    None => (text.join_rejected)(&d.group.name),
                };
                self.errors.push(msg);
            }
            types::PREFS => {
                let p: Prefs = body(data)?;
                self.snap.as_mut()?.prefs = p;
            }
            _ => {}
        }
        None
    }

    /// Drop typing notices past their time.
    pub(crate) fn expire_typing(&mut self) {
        let now = Instant::now();
        for users in self.typing.values_mut() {
            users.retain(|_, at| now.duration_since(*at) < TYPING_SHOWN);
        }
        self.typing.retain(|_, u| !u.is_empty());
    }
}

/// Whether `now` is within the quiet hours.
pub(crate) fn quiet(prefs: &Prefs, now: chrono::NaiveTime) -> bool {
    let parse = |s: &str| chrono::NaiveTime::parse_from_str(s, "%H:%M").ok();
    let (Some(a), Some(b)) = (parse(&prefs.quiet_start), parse(&prefs.quiet_end)) else {
        return false;
    };
    if a <= b { now >= a && now < b } else { now >= a || now < b }
}

/// The beginning of a message in plain words.
pub(crate) fn preview(m: &Message, names: &dyn Names) -> String {
    if let Some(call) = m.call {
        let v = &crate::i18n::t().voice;
        return if call.missed { v.call_missed.to_owned() } else { (v.call_record)(call.duration_s.div_ceil(60).max(1)) };
    }
    let text = super::markdown::plain_message(&m.content, names);
    let files = if m.attachments.is_empty() {
        String::new()
    } else {
        (crate::i18n::t().chat.attachment_count)(m.attachments.len())
    };
    let joined = [text, files].into_iter().filter(|s| !s.is_empty()).collect::<Vec<_>>().join(" ");
    joined.chars().take(60).collect()
}

pub(crate) struct StateNames<'a> {
    state: &'a ChatState,
    group: Option<&'a GroupState>,
}

impl Names for StateNames<'_> {
    fn user(&self, id: &str) -> String {
        if let Some(g) = self.group
            && let Some(m) = self.state.members.get(&g.group.id).and_then(|l| l.iter().find(|m| m.user.user_id == id))
        {
            return if m.nickname.is_empty() { m.user.display_name().to_owned() } else { m.nickname.clone() };
        }
        if let Some(s) = &self.state.snap {
            if s.me.user_id == id {
                return s.me.display_name().to_owned();
            }
            if let Some(d) = s.dms.iter().find(|d| d.user.user_id == id) {
                return d.user.display_name().to_owned();
            }
        }
        crate::i18n::t().chat.unknown_user.to_owned()
    }
    fn role(&self, id: &str) -> String {
        self.group
            .and_then(|g| g.roles.iter().find(|r| r.id.to_string() == id))
            .map_or_else(|| crate::i18n::t().chat.unknown_role.to_owned(), |r| r.name.clone())
    }
    fn channel(&self, id: &str) -> String {
        if let Some(s) = &self.state.snap {
            for g in &s.groups {
                if let Some(c) = g.channels.iter().find(|c| c.id.to_string() == id) {
                    return c.name.clone();
                }
            }
        }
        crate::i18n::t().chat.unknown_channel.to_owned()
    }
}

/// Channels in display order: outside any category first, then each
/// category followed by its channels.
pub(crate) fn ordered(channels: &[Channel]) -> Vec<&Channel> {
    let key = |c: &&Channel| (c.position, c.id);
    let mut top: Vec<&Channel> = channels
        .iter()
        .filter(|c| c.parent_id.is_none() && c.kind != farwing_protocol::chat::ChannelKind::Category)
        .collect();
    top.sort_by_key(key);
    let mut cats: Vec<&Channel> = channels
        .iter()
        .filter(|c| c.kind == farwing_protocol::chat::ChannelKind::Category)
        .collect();
    cats.sort_by_key(key);
    let mut out = top;
    for cat in cats {
        out.push(cat);
        let mut children: Vec<&Channel> = channels.iter().filter(|c| c.parent_id == Some(cat.id)).collect();
        children.sort_by_key(key);
        out.extend(children);
    }
    for c in channels {
        if !out.iter().any(|x| x.id == c.id) {
            out.push(c);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use farwing_protocol::chat::{ChannelKind, Group};

    fn state() -> ChatState {
        let me = ChatUser {
            user_id: "a".into(),
            username: "alice".into(),
            ..Default::default()
        };
        let channel = Channel {
            id: Id(10),
            group_id: Some(Id(1)),
            kind: ChannelKind::Text,
            name: "general".into(),
            ..Default::default()
        };
        ChatState {
            snap: Some(Snapshot {
                me: me.clone(),
                groups: vec![GroupState {
                    group: Group {
                        id: Id(1),
                        name: "G".into(),
                        ..Default::default()
                    },
                    channels: vec![channel],
                    roles: Vec::new(),
                    me: Member {
                        user: me,
                        ..Default::default()
                    },
                }],
                prefs: Prefs::new(),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    fn message(id: i64, author: &str, content: &str) -> Message {
        Message {
            id: Id(id),
            channel_id: Id(10),
            author: ChatUser {
                user_id: author.into(),
                username: author.into(),
                ..Default::default()
            },
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn messages_move_unread_and_mentions() {
        let mut s = state();
        s.messages.insert(Id(10), ChannelMessages::default());
        let n = s.add_message(message(100, "b", "hi"), false);
        // Groups notify on mentions only by default.
        assert!(n.is_none());
        let c = s.channel(Id(10)).unwrap().0.clone();
        assert_eq!(s.unread(&c), (true, 0));
        let mut m = message(101, "b", "<@a> look");
        m.mentions.users.push("a".into());
        let n = s.add_message(m, false).expect("a mention notifies");
        assert_eq!(n.title, "G · general · b");
        assert!(n.body.contains("look"));
        let c = s.channel(Id(10)).unwrap().0.clone();
        assert_eq!(s.unread(&c), (true, 1));
        // My own message reads up to it.
        s.add_message(message(102, "a", "ok"), false);
        let c = s.channel(Id(10)).unwrap().0.clone();
        assert_eq!(s.unread(&c), (false, 0));
        assert_eq!(s.messages[&Id(10)].list.len(), 3);
        // Withdrawn by a push.
        s.apply(
            types::MESSAGE_DELETED,
            serde_json::json!({"channel_id": "10", "id": "101"}),
            false,
        );
        assert!(s.messages[&Id(10)].list[1].message.deleted);
    }

    #[test]
    fn quiet_hours_cross_midnight() {
        let mut p = Prefs::new();
        p.quiet_start = "23:00".into();
        p.quiet_end = "08:00".into();
        let at = |h, m| chrono::NaiveTime::from_hms_opt(h, m, 0).unwrap();
        assert!(quiet(&p, at(23, 30)));
        assert!(quiet(&p, at(7, 59)));
        assert!(!quiet(&p, at(12, 0)));
        p.quiet_start.clear();
        assert!(!quiet(&p, at(23, 30)));
    }
}
