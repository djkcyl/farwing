//! Chat (`docs/protocol/chat-design.md`): groups, channels, direct
//! messages, the chat connection and its pushes. Persistence is in
//! [`store`], permissions in [`perms`], the HTTP handlers in
//! `crate::http::chat`.

pub mod conn;
pub mod hub;
pub mod media;
pub mod perms;
pub mod push;
pub mod store;

use crate::db::{R, now_ms, now_s};
use farwing_protocol::chat::{
    Channel, ChatLimits, ChatUser, Dm, Group, GroupInvite, GroupPreview, GroupState, Id, Member,
    Mentions, Message, Overwrite, Reply, Role, Snapshot, Status, perm,
};
use rusqlite::Connection;
use std::collections::{HashMap, HashSet};
use store::{ChannelRow, GroupRow, MemberRow, MessageRow, RoleRow};

/// Days a dissolved group can be restored.
pub const RESTORE_DAYS: i64 = 30;

/// Chat state that lives in memory: the connections and the media key.
pub struct Chat {
    pub hub: std::sync::Arc<hub::ChatHub>,
    pub media: media::MediaKey,
    /// Recent sends by `(user, nonce)`: a repeated send makes no second
    /// message.
    nonces: parking_lot::Mutex<HashMap<(String, String), (std::time::Instant, i64)>>,
}

/// How long a send's nonce is remembered.
const NONCE_TTL: std::time::Duration = std::time::Duration::from_secs(60);

impl Chat {
    pub fn new(media_key: [u8; 32]) -> Self {
        Self {
            hub: Default::default(),
            media: media::MediaKey(media_key),
            nonces: Default::default(),
        }
    }

    /// The message an earlier send with this nonce made.
    pub fn sent(&self, user: &str, nonce: &str) -> Option<i64> {
        let mut nonces = self.nonces.lock();
        nonces.retain(|_, (t, _)| t.elapsed() < NONCE_TTL);
        nonces.get(&(user.to_owned(), nonce.to_owned())).map(|(_, id)| *id)
    }

    pub fn remember_sent(&self, user: &str, nonce: &str, message: i64) {
        if !nonce.is_empty() {
            self.nonces
                .lock()
                .insert((user.to_owned(), nonce.to_owned()), (std::time::Instant::now(), message));
        }
    }
}

// ====================================================================== IDs

/// The next Snowflake: milliseconds since [`Id::EPOCH_MS`] << 22 and a
/// counter within the millisecond.
pub fn next_id() -> i64 {
    static LAST: parking_lot::Mutex<(i64, i64)> = parking_lot::Mutex::new((0, 0));
    let mut last = LAST.lock();
    let mut ms = (now_ms() - Id::EPOCH_MS).max(0);
    if ms < last.0 {
        ms = last.0;
    }
    if ms == last.0 {
        last.1 += 1;
        if last.1 >= 1 << 22 {
            ms += 1;
            last.1 = 0;
        }
    } else {
        last.1 = 0;
    }
    last.0 = ms;
    (ms << 22) | last.1
}

/// The smallest ID made at or after Unix second `s`.
pub fn id_at(s: i64) -> i64 {
    ((s * 1000 - Id::EPOCH_MS).max(0)) << 22
}

// ============================================================ a group's view

/// A group with what permissions are computed from.
pub struct GroupCtx {
    pub group: GroupRow,
    /// `@everyone` first, then by position.
    pub roles: Vec<RoleRow>,
    pub channels: Vec<ChannelRow>,
    pub overwrites: HashMap<i64, Vec<Overwrite>>,
}

impl GroupCtx {
    /// A group that is not dissolved.
    pub fn load(c: &Connection, id: i64) -> R<Option<Self>> {
        let Some(group) = store::group(c, id)? else {
            return Ok(None);
        };
        if group.dissolved_at.is_some() {
            return Ok(None);
        }
        Self::with(c, group).map(Some)
    }

    pub fn with(c: &Connection, group: GroupRow) -> R<Self> {
        Ok(Self {
            roles: store::roles(c, group.id)?,
            channels: store::channels(c, group.id)?,
            overwrites: store::overwrites(c, group.id)?,
            group,
        })
    }

    pub fn everyone(&self) -> i64 {
        self.roles
            .iter()
            .find(|r| r.id == self.group.id)
            .map_or(perm::EVERYONE, |r| r.permissions)
    }

    pub fn role(&self, id: i64) -> Option<&RoleRow> {
        self.roles.iter().find(|r| r.id == id)
    }

    pub fn channel(&self, id: i64) -> Option<&ChannelRow> {
        self.channels.iter().find(|c| c.id == id)
    }

    fn member_roles(&self, m: &MemberRow) -> Vec<(i64, i64)> {
        m.roles
            .iter()
            .filter_map(|id| self.role(*id))
            .filter(|r| r.id != self.group.id)
            .map(|r| (r.id, r.permissions))
            .collect()
    }

    pub fn is_owner(&self, user: &str) -> bool {
        self.group.owner_id == user
    }

    /// Group-wide permissions of `m`.
    pub fn base(&self, m: &MemberRow) -> i64 {
        let roles = self.member_roles(m);
        perms::base(
            self.everyone(),
            &perms::Who {
                owner: self.is_owner(&m.user_id),
                roles: &roles,
                muted: m.muted(now_s()),
            },
            self.group.frozen_at.is_some(),
        )
    }

    /// Permissions of `m` in channel `id` (0 when it does not exist).
    pub fn channel_perms(&self, m: &MemberRow, id: i64) -> i64 {
        let Some(ch) = self.channel(id) else {
            return 0;
        };
        let empty = Vec::new();
        let own = self.overwrites.get(&ch.id).unwrap_or(&empty);
        let parent = ch
            .parent_id
            .and_then(|p| self.overwrites.get(&p))
            .unwrap_or(&empty);
        let roles = self.member_roles(m);
        perms::channel(
            self.group.id,
            self.everyone(),
            &perms::Who {
                owner: self.is_owner(&m.user_id),
                roles: &roles,
                muted: m.muted(now_s()),
            },
            self.group.frozen_at.is_some(),
            &[parent.as_slice(), own.as_slice()],
        )
    }

    /// Highest role position of `m` (the owner above all).
    pub fn rank(&self, m: &MemberRow) -> i32 {
        perms::rank(
            self.is_owner(&m.user_id),
            m.roles.iter().filter_map(|id| self.role(*id)).map(|r| r.position),
        )
    }

    pub fn wire_group(&self, member_count: u32, permissions: i64) -> Group {
        let g = &self.group;
        Group {
            id: Id(g.id),
            name: g.name.clone(),
            icon: g.icon.clone(),
            owner_id: g.owner_id.clone(),
            public: g.public,
            join_approval: g.join_approval,
            description: g.description.clone(),
            created_at: g.created_at,
            frozen: g.frozen_at.is_some(),
            member_count,
            permissions,
        }
    }

    pub fn preview(&self, member_count: u32) -> GroupPreview {
        preview(&self.group, member_count)
    }

    pub fn wire_roles(&self) -> Vec<Role> {
        self.roles.iter().map(wire_role).collect()
    }

    /// Everything member `m` sees of the group.
    pub fn state(&self, c: &Connection, m: &MemberRow) -> R<GroupState> {
        let mut channels: Vec<(ChannelRow, i64)> = self
            .channels
            .iter()
            .map(|ch| (ch.clone(), self.channel_perms(m, ch.id)))
            .filter(|(_, p)| p & perm::VIEW != 0)
            .collect();
        channels.sort_by_key(|(ch, _)| (ch.position, ch.id));
        let ids: Vec<i64> = channels.iter().map(|(ch, _)| ch.id).collect();
        let last = store::last_message_ids(c, &ids)?;
        let user = store::user(c, &m.user_id)?;
        Ok(GroupState {
            group: self.wire_group(store::member_count(c, self.group.id)?, self.base(m)),
            channels: channels
                .into_iter()
                .map(|(ch, p)| {
                    let overwrites = self.overwrites.get(&ch.id).cloned().unwrap_or_default();
                    let last = last.get(&ch.id).copied();
                    wire_channel(&ch, overwrites, p, last)
                })
                .collect(),
            roles: self.wire_roles(),
            me: wire_member(m, user),
        })
    }
}

pub fn preview(g: &GroupRow, member_count: u32) -> GroupPreview {
    GroupPreview {
        id: Id(g.id),
        name: g.name.clone(),
        icon: g.icon.clone(),
        description: g.description.clone(),
        member_count,
        join_approval: g.join_approval,
    }
}

pub fn wire_role(r: &RoleRow) -> Role {
    Role {
        id: Id(r.id),
        name: r.name.clone(),
        color: r.color,
        permissions: r.permissions,
        position: r.position,
        mentionable: r.mentionable,
    }
}

pub fn wire_channel(ch: &ChannelRow, overwrites: Vec<Overwrite>, permissions: i64, last: Option<i64>) -> Channel {
    Channel {
        id: Id(ch.id),
        group_id: ch.group_id.map(Id),
        kind: ch.kind,
        name: ch.name.clone(),
        topic: ch.topic.clone(),
        parent_id: ch.parent_id.map(Id),
        position: ch.position,
        slow_mode_s: ch.slow_mode_s,
        voice_bitrate: ch.voice_bitrate,
        voice_limit: ch.voice_limit,
        overwrites,
        permissions,
        last_message_id: last.map(Id),
    }
}

pub fn wire_member(m: &MemberRow, user: ChatUser) -> Member {
    Member {
        user,
        nickname: m.nickname.clone(),
        roles: m.roles.iter().copied().map(Id).collect(),
        joined_at: m.joined_at,
        muted_until: m.muted_until.filter(|t| *t > now_s()),
    }
}

// ============================================================ channel access

/// What `user` may do in a channel, with what it was computed from.
pub struct Access {
    pub channel: ChannelRow,
    pub perms: i64,
    /// Group channels.
    pub ctx: Option<GroupCtx>,
    pub member: Option<MemberRow>,
    /// Direct messages: the other side and whether they are friends.
    pub peer: Option<(String, bool)>,
    /// The oldest message `user` may read (without `read_history`, from
    /// when it joined).
    pub floor: i64,
}

impl Access {
    pub fn can(&self, bits: i64) -> bool {
        self.perms & bits == bits
    }
    pub fn group_id(&self) -> Option<i64> {
        self.channel.group_id
    }
}

/// `user`'s access to channel `id`; none when it cannot see it.
pub fn access(c: &Connection, user: &str, id: i64) -> R<Option<Access>> {
    let Some(channel) = store::channel(c, id)? else {
        return Ok(None);
    };
    match channel.group_id {
        None => {
            let Some((a, b)) = store::dm_users(c, id)? else {
                return Ok(None);
            };
            let peer = if a == user {
                b
            } else if b == user {
                a
            } else {
                return Ok(None);
            };
            let friends = crate::db::are_friends(c, user, &peer)?;
            Ok(Some(Access {
                channel,
                perms: perms::dm(friends),
                ctx: None,
                member: None,
                peer: Some((peer, friends)),
                floor: 0,
            }))
        }
        Some(group) => {
            let Some(ctx) = GroupCtx::load(c, group)? else {
                return Ok(None);
            };
            let Some(member) = store::member(c, group, user)? else {
                return Ok(None);
            };
            let perms = ctx.channel_perms(&member, id);
            if perms & perm::VIEW == 0 {
                return Ok(None);
            }
            let floor = if perms & perm::READ_HISTORY == 0 {
                id_at(member.joined_at)
            } else {
                0
            };
            Ok(Some(Access {
                channel,
                perms,
                ctx: Some(ctx),
                member: Some(member),
                peer: None,
                floor,
            }))
        }
    }
}

// ================================================================ messages

/// Characters of a reply's preview.
const REPLY_PREVIEW: usize = 120;

/// Messages as sent to clients, with authors and the messages they reply to.
pub fn wire_messages(c: &Connection, rows: &[MessageRow]) -> R<Vec<Message>> {
    let reply_ids: Vec<i64> = rows.iter().filter_map(|m| m.reply_to).collect();
    let replies = store::messages_by_id(c, &reply_ids)?;
    let mut authors: Vec<String> = rows.iter().map(|m| m.author_id.clone()).collect();
    authors.extend(replies.values().map(|m| m.author_id.clone()));
    let users = store::users(c, &authors)?;
    let user = |id: &str| users.get(id).cloned().unwrap_or_default();
    Ok(rows
        .iter()
        .map(|m| Message {
            id: Id(m.id),
            channel_id: Id(m.channel_id),
            author: user(&m.author_id),
            content: m.content.clone(),
            reply: m.reply_to.map(|r| match replies.get(&r) {
                Some(orig) => Reply {
                    id: Id(r),
                    author: user(&orig.author_id),
                    content: orig.content.chars().take(REPLY_PREVIEW).collect(),
                    deleted: orig.deleted_at.is_some(),
                    attachments: orig.attachments.len() as u32,
                },
                None => Reply {
                    id: Id(r),
                    deleted: true,
                    ..Default::default()
                },
            }),
            mentions: m.mentions.clone(),
            attachments: m.attachments.clone(),
            pinned: m.pinned_at.is_some(),
            edited_at: m.edited_at,
            deleted: m.deleted_at.is_some(),
            created_at: Id(m.id).created_ms(),
            nonce: String::new(),
        })
        .collect())
}

pub fn wire_message(c: &Connection, row: &MessageRow) -> R<Message> {
    Ok(wire_messages(c, std::slice::from_ref(row))?.remove(0))
}

/// The mentions written in `content` (`<@user>`, `<@&role>`,
/// `<@everyone>`, `<@here>`), unchecked.
pub fn parse_mentions(content: &str) -> Mentions {
    let mut m = Mentions::default();
    let mut rest = content;
    while let Some(start) = rest.find("<@") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find('>') else { break };
        let inner = &rest[..end];
        if inner == "everyone" {
            m.everyone = true;
        } else if inner == "here" {
            m.here = true;
        } else if let Some(role) = inner.strip_prefix('&') {
            if let Ok(id) = role.parse::<i64>()
                && !m.roles.contains(&Id(id))
            {
                m.roles.push(Id(id));
            }
        } else if farwing_protocol::ids::is_uuid(inner) && !m.users.iter().any(|u| u == inner) {
            m.users.push(inner.to_owned());
        }
        rest = &rest[end..];
    }
    m
}

/// Keep only the mentions `access` allows: members (or the other side),
/// mentionable roles (any with `mention_all`), everyone and here with
/// `mention_all`.
pub fn allowed_mentions(c: &Connection, access: &Access, mut m: Mentions) -> R<Mentions> {
    let all = access.can(perm::MENTION_ALL);
    match (&access.ctx, &access.peer) {
        (Some(ctx), _) => {
            let mut users = Vec::new();
            for u in m.users {
                if store::member(c, ctx.group.id, &u)?.is_some() {
                    users.push(u);
                }
            }
            m.users = users;
            m.roles.retain(|r| {
                ctx.role(r.0)
                    .is_some_and(|role| role.id != ctx.group.id && (role.mentionable || all))
            });
        }
        (None, Some((peer, _))) => {
            m.users.retain(|u| u == peer);
            m.roles.clear();
        }
        _ => {}
    }
    if !all || access.ctx.is_none() {
        m.everyone = false;
        m.here = false;
    }
    Ok(m)
}

/// Who `m` mentions in a channel (the author aside) and can see it;
/// `online` is who counts for `@here`.
pub fn mentioned_users(
    c: &Connection,
    access: &Access,
    author: &str,
    m: &Mentions,
    online: &HashSet<String>,
) -> R<Vec<String>> {
    let mut out: HashSet<String> = m.users.iter().cloned().collect();
    if let Some(ctx) = &access.ctx {
        let members = store::members(c, ctx.group.id, "", u32::MAX)?;
        for member in &members {
            let hit = m.everyone
                || (m.here && online.contains(&member.user_id))
                || member.roles.iter().any(|r| m.roles.contains(&Id(*r)));
            if hit {
                out.insert(member.user_id.clone());
            }
        }
        out.retain(|u| {
            members
                .iter()
                .find(|mm| &mm.user_id == u)
                .is_some_and(|mm| ctx.channel_perms(mm, access.channel.id) & perm::VIEW != 0)
        });
    }
    out.remove(author);
    Ok(out.into_iter().collect())
}

// ================================================== direct messages, snapshot

/// `me`'s view of direct-message channel `channel` with `other`.
pub fn dm_for(c: &Connection, me: &str, channel: i64, other: &str) -> R<Option<Dm>> {
    let Some(ch) = store::channel(c, channel)? else {
        return Ok(None);
    };
    let friends = crate::db::are_friends(c, me, other)?;
    let last = store::last_message_ids(c, &[channel])?.get(&channel).copied();
    let receipts = store::prefs(c, me)?.read_receipts && store::prefs(c, other)?.read_receipts;
    let peer_read = if receipts {
        let id = store::read_state(c, other, channel)?.last_read_id;
        (id.0 > 0).then_some(id)
    } else {
        None
    };
    Ok(Some(Dm {
        channel: wire_channel(&ch, Vec::new(), perms::dm(friends), last),
        user: store::user(c, other)?,
        friends,
        peer_read_id: peer_read,
    }))
}

/// Everything `user` sees at once (`chat.hello`), but who is online: the
/// presences come with the user IDs whose status to fill in.
pub fn snapshot(c: &Connection, user: &str, limits: ChatLimits) -> R<(Snapshot, Vec<String>)> {
    let mut groups = Vec::new();
    for g in store::user_groups(c, user)? {
        let ctx = GroupCtx::with(c, g)?;
        if let Some(m) = store::member(c, ctx.group.id, user)? {
            groups.push(ctx.state(c, &m)?);
        }
    }
    let mut dms = Vec::new();
    for (ch, other) in store::user_dms(c, user)? {
        if let Some(dm) = dm_for(c, user, ch.id, &other)? {
            dms.push(dm);
        }
    }
    let mut group_invites = Vec::new();
    for (id, group, by, at) in store::group_invites_for(c, user)? {
        if let Some(g) = store::group(c, group)? {
            group_invites.push(GroupInvite {
                id: Id(id),
                group: preview(&g, store::member_count(c, group)?),
                by: store::user(c, &by)?,
                at,
            });
        }
    }
    Ok((
        Snapshot {
            me: store::user(c, user)?,
            status: store::status(c, user)?,
            groups,
            dms,
            read_states: store::read_states(c, user)?,
            presences: Vec::new(),
            prefs: store::prefs(c, user)?,
            group_invites,
            limits,
        },
        store::audience(c, user)?,
    ))
}

/// Remove attachments never sent (an hour old); with `hourly`, also purge
/// groups dissolved more than 30 days ago, messages past the retention and
/// old group audit entries.
pub async fn upkeep(app: &crate::http::App, hourly: bool) {
    let data = app.cfg().data_dir.clone();
    let stale = app
        .db
        .call(|c| {
            let rows = store::stale_attachments(c, now_s() - 3600)?;
            for (id, _) in &rows {
                store::delete_attachment(c, *id)?;
            }
            Ok(rows)
        })
        .await;
    match stale {
        Ok(rows) => {
            for (id, group) in &rows {
                media::remove(&data, *group, *id);
            }
            if !rows.is_empty() {
                tracing::info!(removed = rows.len(), "unsent chat attachments removed");
            }
        }
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "removing unsent attachments failed"),
    }
    if !hourly {
        return;
    }
    let cfg = app.cfg();
    let retention = cfg.chat.retention_days;
    let audit_keep = i64::from(cfg.web.audit_days.max(1)) * 86_400;
    let result = app
        .db
        .call(move |c| {
            let mut purged = Vec::new();
            for g in store::dissolved_before(c, now_s() - RESTORE_DAYS * 86_400)? {
                store::purge_group(c, g)?;
                purged.push(g);
            }
            let mut files = Vec::new();
            let mut pruned = 0;
            if let Some(days) = retention {
                let (n, f) = store::prune_messages(c, id_at(now_s() - i64::from(days) * 86_400))?;
                pruned = n;
                files = f;
            }
            store::prune_audit(c, audit_keep)?;
            Ok((purged, pruned, files))
        })
        .await;
    match result {
        Ok((purged, pruned, files)) => {
            for g in &purged {
                let _ = std::fs::remove_dir_all(media::folder(&data, Some(*g)));
                let _ = std::fs::remove_file(media::icon(&data, *g));
            }
            // Files of pruned messages: their folders are not known here, so
            // every folder is tried.
            if !files.is_empty()
                && let Ok(dirs) = std::fs::read_dir(data.join("attachments"))
            {
                let dirs: Vec<_> = dirs.flatten().map(|d| d.path()).collect();
                for id in &files {
                    for d in &dirs {
                        let _ = std::fs::remove_file(d.join(id.to_string()));
                        let _ = std::fs::remove_file(d.join(format!("{id}.thumb.webp")));
                    }
                }
            }
            if !purged.is_empty() || pruned > 0 {
                tracing::info!(groups = purged.len(), messages = pruned, "chat data past its time removed");
            }
        }
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "chat upkeep failed"),
    }
}

/// What the others see of `user`'s status.
pub fn effective_status(chosen: Status, activity: Option<bool>) -> Status {
    match (chosen, activity) {
        (_, None) | (Status::Invisible, _) | (Status::Offline, _) => Status::Offline,
        (Status::Dnd, _) => Status::Dnd,
        (_, Some(true)) => Status::Idle,
        _ => Status::Online,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snowflakes_increase() {
        let a = next_id();
        let b = next_id();
        assert!(b > a);
        assert!((Id(b).created_ms() - now_ms()).abs() < 5_000);
        assert!(id_at(now_s() + 2) > b);
    }

    #[test]
    fn mentions_are_parsed() {
        let u = "0f8fad5b-d9cb-469f-a165-70867728950e";
        let m = parse_mentions(&format!("hi <@{u}> and <@&42> <@everyone> <@here> <@{u}> <@bad> <@&x>"));
        assert_eq!(m.users, vec![u.to_owned()]);
        assert_eq!(m.roles, vec![Id(42)]);
        assert!(m.everyone && m.here);
        assert!(parse_mentions("a <@ b").is_empty());
    }

    #[test]
    fn statuses_seen_by_others() {
        assert_eq!(effective_status(Status::Online, None), Status::Offline);
        assert_eq!(effective_status(Status::Online, Some(false)), Status::Online);
        assert_eq!(effective_status(Status::Online, Some(true)), Status::Idle);
        assert_eq!(effective_status(Status::Dnd, Some(true)), Status::Dnd);
        assert_eq!(effective_status(Status::Invisible, Some(false)), Status::Offline);
    }
}
