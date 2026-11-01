//! Chat persistence (`docs/protocol/chat-design.md` §2.3): synchronous
//! SQLite functions, run through [`crate::db::Db::call`].

use crate::db::{R, now_s};
use farwing_protocol::chat::{
    Attachment, ChannelKind, ChatUser, Id, Mentions, Overwrite, Prefs, ReadState, Status, perm,
};
use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter};
use std::collections::HashMap;

// ===================================================================== users

fn chat_user_from(r: &Row<'_>) -> R<ChatUser> {
    Ok(ChatUser {
        user_id: r.get(0)?,
        username: r.get(1)?,
        nickname: r.get(2)?,
        avatar: r.get(3)?,
        gone: r.get::<_, i64>(4)? != 0,
    })
}

/// Chat summaries of `ids`; missing accounts come back `gone`.
pub fn users(c: &Connection, ids: &[String]) -> R<HashMap<String, ChatUser>> {
    let mut out = HashMap::new();
    let mut st = c.prepare_cached(
        "SELECT id, username, nickname, avatar, disabled FROM users WHERE id = ?1",
    )?;
    for id in ids {
        if out.contains_key(id) {
            continue;
        }
        let user = st
            .query_row([id], chat_user_from)
            .optional()?
            .unwrap_or_else(|| ChatUser {
                user_id: id.clone(),
                gone: true,
                ..Default::default()
            });
        out.insert(id.clone(), user);
    }
    Ok(out)
}

pub fn user(c: &Connection, id: &str) -> R<ChatUser> {
    Ok(users(c, &[id.to_owned()])?.remove(id).unwrap_or_default())
}

pub fn status(c: &Connection, user: &str) -> R<Status> {
    let text: Option<String> = c
        .query_row("SELECT status FROM users WHERE id = ?1", [user], |r| r.get(0))
        .optional()?;
    Ok(text.as_deref().and_then(Status::parse).unwrap_or(Status::Online))
}

pub fn set_status(c: &Connection, user: &str, status: Status) -> R<()> {
    c.execute("UPDATE users SET status = ?2 WHERE id = ?1", params![user, status.as_str()])?;
    Ok(())
}

pub fn set_avatar(c: &Connection, user: &str, version: &str) -> R<()> {
    c.execute("UPDATE users SET avatar = ?2 WHERE id = ?1", params![user, version])?;
    Ok(())
}

/// Friends and everyone sharing a group with `user`: who sees its status.
pub fn audience(c: &Connection, user: &str) -> R<Vec<String>> {
    let mut st = c.prepare_cached(
        "SELECT user_b FROM friendships WHERE user_a = ?1
         UNION SELECT user_a FROM friendships WHERE user_b = ?1
         UNION SELECT m2.user_id FROM chat_members m1
             JOIN chat_members m2 ON m2.group_id = m1.group_id
             JOIN chat_groups g ON g.id = m1.group_id AND g.dissolved_at IS NULL
             WHERE m1.user_id = ?1 AND m2.user_id != ?1",
    )?;
    st.query_map([user], |r| r.get(0))?.collect()
}

// ==================================================================== groups

#[derive(Clone, Debug, Default)]
pub struct GroupRow {
    pub id: i64,
    pub name: String,
    pub icon: String,
    pub owner_id: String,
    pub public: bool,
    pub join_approval: bool,
    pub description: String,
    pub quota_bytes: Option<i64>,
    pub created_at: i64,
    pub frozen_at: Option<i64>,
    pub dissolved_at: Option<i64>,
}

const GROUP_COLS: &str = "id, name, icon, owner_id, public, join_approval, description, \
                          quota_bytes, created_at, frozen_at, dissolved_at";

fn group_from(r: &Row<'_>) -> R<GroupRow> {
    Ok(GroupRow {
        id: r.get(0)?,
        name: r.get(1)?,
        icon: r.get(2)?,
        owner_id: r.get(3)?,
        public: r.get::<_, i64>(4)? != 0,
        join_approval: r.get::<_, i64>(5)? != 0,
        description: r.get(6)?,
        quota_bytes: r.get(7)?,
        created_at: r.get(8)?,
        frozen_at: r.get(9)?,
        dissolved_at: r.get(10)?,
    })
}

pub fn group(c: &Connection, id: i64) -> R<Option<GroupRow>> {
    c.query_row(
        &format!("SELECT {GROUP_COLS} FROM chat_groups WHERE id = ?1"),
        [id],
        group_from,
    )
    .optional()
}

/// Every group (the console), newest first.
pub fn all_groups(c: &Connection) -> R<Vec<GroupRow>> {
    let mut st = c.prepare(&format!("SELECT {GROUP_COLS} FROM chat_groups ORDER BY id DESC"))?;
    st.query_map([], group_from)?.collect()
}

/// The groups `user` is in (not dissolved), oldest membership first.
pub fn user_groups(c: &Connection, user: &str) -> R<Vec<GroupRow>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {} FROM chat_groups g JOIN chat_members m ON m.group_id = g.id
         WHERE m.user_id = ?1 AND g.dissolved_at IS NULL ORDER BY m.joined_at, g.id",
        GROUP_COLS
            .split(", ")
            .map(|c| format!("g.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    st.query_map([user], group_from)?.collect()
}

/// Default names of what a new group starts with, in its creator's language.
pub struct Starter<'a> {
    pub admin_role: &'a str,
    pub category: &'a str,
    pub text: &'a str,
    pub voice: &'a str,
}

/// A new group with `@everyone`, an administrator role, a category with a
/// text channel and a voice channel; the owner is its first member.
pub fn create_group(
    c: &mut Connection,
    owner: &str,
    name: &str,
    description: &str,
    starter: &Starter<'_>,
    mut next_id: impl FnMut() -> i64,
) -> R<i64> {
    let tx = c.transaction()?;
    let now = now_s();
    let id = next_id();
    tx.execute(
        "INSERT INTO chat_groups (id, name, owner_id, description, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
        params![id, name, owner, description, now],
    )?;
    tx.execute(
        "INSERT INTO chat_roles (id, group_id, name, permissions, position) VALUES (?1, ?1, '@everyone', ?2, 0)",
        params![id, perm::EVERYONE],
    )?;
    tx.execute(
        "INSERT INTO chat_roles (id, group_id, name, color, permissions, position) VALUES (?1, ?2, ?3, ?4, ?5, 1)",
        params![next_id(), id, starter.admin_role, 0xE67E22, perm::ADMINISTRATOR],
    )?;
    let category = next_id();
    for (cid, kind, cname, parent, position) in [
        (category, ChannelKind::Category, starter.category, None, 0),
        (next_id(), ChannelKind::Text, starter.text, Some(category), 0),
        (next_id(), ChannelKind::Voice, starter.voice, Some(category), 1),
    ] {
        tx.execute(
            "INSERT INTO chat_channels (id, group_id, kind, name, parent_id, position, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![cid, id, kind.as_str(), cname, parent, position, now],
        )?;
    }
    tx.execute(
        "INSERT INTO chat_members (group_id, user_id, joined_at) VALUES (?1, ?2, ?3)",
        params![id, owner, now],
    )?;
    tx.commit()?;
    Ok(id)
}

pub fn update_group(c: &Connection, g: &GroupRow) -> R<()> {
    c.execute(
        "UPDATE chat_groups SET name = ?2, description = ?3, public = ?4, join_approval = ?5,
         icon = ?6, owner_id = ?7, quota_bytes = ?8, frozen_at = ?9, dissolved_at = ?10 WHERE id = ?1",
        params![
            g.id,
            g.name,
            g.description,
            g.public,
            g.join_approval,
            g.icon,
            g.owner_id,
            g.quota_bytes,
            g.frozen_at,
            g.dissolved_at
        ],
    )?;
    Ok(())
}

/// Delete a group and everything in it; the attachment folder is removed
/// by the caller.
pub fn purge_group(c: &mut Connection, id: i64) -> R<()> {
    let tx = c.transaction()?;
    tx.execute(
        "DELETE FROM chat_messages_fts WHERE rowid IN
         (SELECT m.id FROM chat_messages m JOIN chat_channels ch ON ch.id = m.channel_id WHERE ch.group_id = ?1)",
        [id],
    )?;
    tx.execute("DELETE FROM chat_attachments WHERE group_id = ?1", [id])?;
    tx.execute("DELETE FROM chat_audit WHERE group_id = ?1", [id])?;
    tx.execute("DELETE FROM chat_groups WHERE id = ?1", [id])?;
    tx.commit()
}

/// Groups dissolved before `before` (Unix seconds).
pub fn dissolved_before(c: &Connection, before: i64) -> R<Vec<i64>> {
    let mut st = c.prepare("SELECT id FROM chat_groups WHERE dissolved_at IS NOT NULL AND dissolved_at < ?1")?;
    st.query_map([before], |r| r.get(0))?.collect()
}

/// Dissolve the groups `user` owns (the account is being deleted).
pub fn dissolve_owned(c: &Connection, user: &str) -> R<usize> {
    c.execute(
        "UPDATE chat_groups SET dissolved_at = ?2 WHERE owner_id = ?1 AND dissolved_at IS NULL",
        params![user, now_s()],
    )
}

pub fn member_count(c: &Connection, group: i64) -> R<u32> {
    c.query_row("SELECT COUNT(*) FROM chat_members WHERE group_id = ?1", [group], |r| r.get(0))
}

pub fn channel_count(c: &Connection, group: i64) -> R<u32> {
    c.query_row(
        "SELECT COUNT(*) FROM chat_channels WHERE group_id = ?1 AND kind != 'category'",
        [group],
        |r| r.get(0),
    )
}

/// Public groups whose name contains `q` (any when empty).
pub fn discover(c: &Connection, q: &str, limit: u32) -> R<Vec<GroupRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {GROUP_COLS} FROM chat_groups WHERE public = 1 AND dissolved_at IS NULL
         AND frozen_at IS NULL AND (?1 = '' OR instr(lower(name), lower(?1)) > 0)
         ORDER BY id DESC LIMIT ?2"
    ))?;
    st.query_map(params![q, limit], group_from)?.collect()
}

// ===================================================================== roles

#[derive(Clone, Debug, Default)]
pub struct RoleRow {
    pub id: i64,
    pub group_id: i64,
    pub name: String,
    pub color: u32,
    pub permissions: i64,
    pub position: i32,
    pub mentionable: bool,
}

fn role_from(r: &Row<'_>) -> R<RoleRow> {
    Ok(RoleRow {
        id: r.get(0)?,
        group_id: r.get(1)?,
        name: r.get(2)?,
        color: r.get(3)?,
        permissions: r.get(4)?,
        position: r.get(5)?,
        mentionable: r.get::<_, i64>(6)? != 0,
    })
}

/// The roles of a group, `@everyone` first, then by position.
pub fn roles(c: &Connection, group: i64) -> R<Vec<RoleRow>> {
    let mut st = c.prepare_cached(
        "SELECT id, group_id, name, color, permissions, position, mentionable FROM chat_roles
         WHERE group_id = ?1 ORDER BY position, id",
    )?;
    st.query_map([group], role_from)?.collect()
}

pub fn insert_role(c: &Connection, r: &RoleRow) -> R<()> {
    c.execute(
        "INSERT INTO chat_roles (id, group_id, name, color, permissions, position, mentionable)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![r.id, r.group_id, r.name, r.color, r.permissions, r.position, r.mentionable],
    )?;
    Ok(())
}

pub fn update_role(c: &Connection, r: &RoleRow) -> R<()> {
    c.execute(
        "UPDATE chat_roles SET name = ?2, color = ?3, permissions = ?4, position = ?5, mentionable = ?6
         WHERE id = ?1",
        params![r.id, r.name, r.color, r.permissions, r.position, r.mentionable],
    )?;
    Ok(())
}

pub fn delete_role(c: &mut Connection, group: i64, id: i64) -> R<()> {
    let tx = c.transaction()?;
    tx.execute(
        "DELETE FROM chat_overwrites WHERE role_id = ?1 AND channel_id IN
         (SELECT id FROM chat_channels WHERE group_id = ?2)",
        params![id, group],
    )?;
    tx.execute("DELETE FROM chat_roles WHERE id = ?1 AND group_id = ?2", params![id, group])?;
    tx.commit()
}

/// Positions 1.. in the order of `ids` (lowest first).
pub fn order_roles(c: &mut Connection, group: i64, ids: &[i64]) -> R<()> {
    let tx = c.transaction()?;
    for (i, id) in ids.iter().enumerate() {
        tx.execute(
            "UPDATE chat_roles SET position = ?3 WHERE id = ?1 AND group_id = ?2",
            params![id, group, i as i64 + 1],
        )?;
    }
    tx.commit()
}

// ================================================================== channels

#[derive(Clone, Debug, Default)]
pub struct ChannelRow {
    pub id: i64,
    pub group_id: Option<i64>,
    pub kind: ChannelKind,
    pub name: String,
    pub topic: String,
    pub parent_id: Option<i64>,
    pub position: i32,
    pub slow_mode_s: u32,
    pub voice_bitrate: u32,
    pub voice_limit: u32,
    pub voice_compat: bool,
    pub created_at: i64,
}

const CHANNEL_COLS: &str = "id, group_id, kind, name, topic, parent_id, position, slow_mode_s, \
                            voice_bitrate, voice_limit, created_at, voice_compat";

fn channel_from(r: &Row<'_>) -> R<ChannelRow> {
    let kind: String = r.get(2)?;
    Ok(ChannelRow {
        id: r.get(0)?,
        group_id: r.get(1)?,
        kind: ChannelKind::parse(&kind).unwrap_or_default(),
        name: r.get(3)?,
        topic: r.get(4)?,
        parent_id: r.get(5)?,
        position: r.get(6)?,
        slow_mode_s: r.get(7)?,
        voice_bitrate: r.get(8)?,
        voice_limit: r.get(9)?,
        created_at: r.get(10)?,
        voice_compat: r.get::<_, i64>(11)? != 0,
    })
}

pub fn channel(c: &Connection, id: i64) -> R<Option<ChannelRow>> {
    c.query_row(
        &format!("SELECT {CHANNEL_COLS} FROM chat_channels WHERE id = ?1"),
        [id],
        channel_from,
    )
    .optional()
}

/// The channels of a group: categories and the channels outside any
/// first, by position.
pub fn channels(c: &Connection, group: i64) -> R<Vec<ChannelRow>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {CHANNEL_COLS} FROM chat_channels WHERE group_id = ?1 ORDER BY position, id"
    ))?;
    st.query_map([group], channel_from)?.collect()
}

pub fn insert_channel(c: &Connection, ch: &ChannelRow) -> R<()> {
    c.execute(
        "INSERT INTO chat_channels (id, group_id, kind, name, topic, parent_id, position, slow_mode_s,
         voice_bitrate, voice_limit, created_at, voice_compat)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            ch.id,
            ch.group_id,
            ch.kind.as_str(),
            ch.name,
            ch.topic,
            ch.parent_id,
            ch.position,
            ch.slow_mode_s,
            ch.voice_bitrate,
            ch.voice_limit,
            ch.created_at,
            ch.voice_compat
        ],
    )?;
    Ok(())
}

pub fn update_channel(c: &Connection, ch: &ChannelRow) -> R<()> {
    c.execute(
        "UPDATE chat_channels SET name = ?2, topic = ?3, parent_id = ?4, position = ?5, slow_mode_s = ?6,
         voice_bitrate = ?7, voice_limit = ?8, voice_compat = ?9 WHERE id = ?1",
        params![
            ch.id,
            ch.name,
            ch.topic,
            ch.parent_id,
            ch.position,
            ch.slow_mode_s,
            ch.voice_bitrate,
            ch.voice_limit,
            ch.voice_compat
        ],
    )?;
    Ok(())
}

/// Delete a channel with its messages; a category's channels move out of
/// it. Returns the attachment IDs whose files go.
pub fn delete_channel(c: &mut Connection, id: i64) -> R<Vec<i64>> {
    let tx = c.transaction()?;
    let files: Vec<i64> = {
        let mut st = tx.prepare("SELECT id FROM chat_attachments WHERE channel_id = ?1")?;
        st.query_map([id], |r| r.get(0))?.collect::<R<_>>()?
    };
    tx.execute(
        "DELETE FROM chat_messages_fts WHERE rowid IN (SELECT id FROM chat_messages WHERE channel_id = ?1)",
        [id],
    )?;
    tx.execute("DELETE FROM chat_attachments WHERE channel_id = ?1", [id])?;
    tx.execute("UPDATE chat_channels SET parent_id = NULL WHERE parent_id = ?1", [id])?;
    tx.execute("DELETE FROM chat_channels WHERE id = ?1", [id])?;
    tx.commit()?;
    Ok(files)
}

/// Positions 0.. in the order given, each with its category.
pub fn order_channels(c: &mut Connection, group: i64, places: &[(i64, Option<i64>)]) -> R<()> {
    let tx = c.transaction()?;
    for (i, (id, parent)) in places.iter().enumerate() {
        tx.execute(
            "UPDATE chat_channels SET position = ?3, parent_id = ?4 WHERE id = ?1 AND group_id = ?2",
            params![id, group, i as i64, parent],
        )?;
    }
    tx.commit()
}

/// The overwrites of a group's channels, by channel.
pub fn overwrites(c: &Connection, group: i64) -> R<HashMap<i64, Vec<Overwrite>>> {
    let mut st = c.prepare_cached(
        "SELECT o.channel_id, o.role_id, o.allow, o.deny FROM chat_overwrites o
         JOIN chat_channels ch ON ch.id = o.channel_id WHERE ch.group_id = ?1 ORDER BY o.role_id",
    )?;
    let mut out: HashMap<i64, Vec<Overwrite>> = HashMap::new();
    let rows = st.query_map([group], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            Overwrite {
                role_id: Id(r.get(1)?),
                allow: r.get(2)?,
                deny: r.get(3)?,
            },
        ))
    })?;
    for row in rows {
        let (channel, o) = row?;
        out.entry(channel).or_default().push(o);
    }
    Ok(out)
}

/// Set (or with nothing allowed or denied, remove) a role's overwrite.
pub fn put_overwrite(c: &Connection, channel: i64, role: i64, allow: i64, deny: i64) -> R<()> {
    if allow == 0 && deny == 0 {
        c.execute(
            "DELETE FROM chat_overwrites WHERE channel_id = ?1 AND role_id = ?2",
            params![channel, role],
        )?;
    } else {
        c.execute(
            "INSERT INTO chat_overwrites (channel_id, role_id, allow, deny) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT (channel_id, role_id) DO UPDATE SET allow = ?3, deny = ?4",
            params![channel, role, allow, deny],
        )?;
    }
    Ok(())
}

/// The newest message of each of `channels`.
pub fn last_message_ids(c: &Connection, channels: &[i64]) -> R<HashMap<i64, i64>> {
    let mut st = c.prepare_cached("SELECT MAX(id) FROM chat_messages WHERE channel_id = ?1")?;
    let mut out = HashMap::new();
    for ch in channels {
        if let Some(id) = st.query_row([ch], |r| r.get::<_, Option<i64>>(0))? {
            out.insert(*ch, id);
        }
    }
    Ok(out)
}

// =================================================================== members

#[derive(Clone, Debug, Default)]
pub struct MemberRow {
    pub group_id: i64,
    pub user_id: String,
    pub nickname: String,
    pub joined_at: i64,
    pub muted_until: Option<i64>,
    pub roles: Vec<i64>,
}

impl MemberRow {
    pub fn muted(&self, now: i64) -> bool {
        self.muted_until.is_some_and(|t| t > now)
    }
}

fn member_roles(c: &Connection, group: i64, user: &str) -> R<Vec<i64>> {
    let mut st = c.prepare_cached(
        "SELECT role_id FROM chat_member_roles WHERE group_id = ?1 AND user_id = ?2 ORDER BY role_id",
    )?;
    st.query_map(params![group, user], |r| r.get(0))?.collect()
}

pub fn member(c: &Connection, group: i64, user: &str) -> R<Option<MemberRow>> {
    let row = c
        .query_row(
            "SELECT group_id, user_id, nickname, joined_at, muted_until FROM chat_members
             WHERE group_id = ?1 AND user_id = ?2",
            params![group, user],
            |r| {
                Ok(MemberRow {
                    group_id: r.get(0)?,
                    user_id: r.get(1)?,
                    nickname: r.get(2)?,
                    joined_at: r.get(3)?,
                    muted_until: r.get(4)?,
                    roles: Vec::new(),
                })
            },
        )
        .optional()?;
    match row {
        Some(mut m) => {
            m.roles = member_roles(c, group, user)?;
            Ok(Some(m))
        }
        None => Ok(None),
    }
}

/// Members of a group by user ID after `after`, at most `limit`.
pub fn members(c: &Connection, group: i64, after: &str, limit: u32) -> R<Vec<MemberRow>> {
    let mut st = c.prepare_cached(
        "SELECT group_id, user_id, nickname, joined_at, muted_until FROM chat_members
         WHERE group_id = ?1 AND user_id > ?2 ORDER BY user_id LIMIT ?3",
    )?;
    let mut rows: Vec<MemberRow> = st
        .query_map(params![group, after, limit], |r| {
            Ok(MemberRow {
                group_id: r.get(0)?,
                user_id: r.get(1)?,
                nickname: r.get(2)?,
                joined_at: r.get(3)?,
                muted_until: r.get(4)?,
                roles: Vec::new(),
            })
        })?
        .collect::<R<_>>()?;
    let mut roles: HashMap<String, Vec<i64>> = HashMap::new();
    {
        let mut st = c.prepare_cached(
            "SELECT user_id, role_id FROM chat_member_roles WHERE group_id = ?1 ORDER BY role_id",
        )?;
        for row in st.query_map([group], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (u, role) = row?;
            roles.entry(u).or_default().push(role);
        }
    }
    for m in &mut rows {
        m.roles = roles.remove(&m.user_id).unwrap_or_default();
    }
    Ok(rows)
}

pub fn member_ids(c: &Connection, group: i64) -> R<Vec<String>> {
    let mut st = c.prepare_cached("SELECT user_id FROM chat_members WHERE group_id = ?1")?;
    st.query_map([group], |r| r.get(0))?.collect()
}

pub fn add_member(c: &mut Connection, group: i64, user: &str) -> R<bool> {
    let tx = c.transaction()?;
    let added = tx.execute(
        "INSERT OR IGNORE INTO chat_members (group_id, user_id, joined_at) VALUES (?1, ?2, ?3)",
        params![group, user, now_s()],
    )? > 0;
    tx.execute(
        "DELETE FROM chat_join_requests WHERE group_id = ?1 AND user_id = ?2",
        params![group, user],
    )?;
    tx.execute(
        "DELETE FROM chat_group_invites WHERE group_id = ?1 AND user_id = ?2",
        params![group, user],
    )?;
    tx.commit()?;
    Ok(added)
}

pub fn remove_member(c: &Connection, group: i64, user: &str) -> R<bool> {
    Ok(c.execute(
        "DELETE FROM chat_members WHERE group_id = ?1 AND user_id = ?2",
        params![group, user],
    )? > 0)
}

pub fn update_member(c: &mut Connection, m: &MemberRow) -> R<()> {
    let tx = c.transaction()?;
    tx.execute(
        "UPDATE chat_members SET nickname = ?3, muted_until = ?4 WHERE group_id = ?1 AND user_id = ?2",
        params![m.group_id, m.user_id, m.nickname, m.muted_until],
    )?;
    tx.execute(
        "DELETE FROM chat_member_roles WHERE group_id = ?1 AND user_id = ?2",
        params![m.group_id, m.user_id],
    )?;
    for role in &m.roles {
        tx.execute(
            "INSERT INTO chat_member_roles (group_id, user_id, role_id) VALUES (?1, ?2, ?3)",
            params![m.group_id, m.user_id, role],
        )?;
    }
    tx.commit()
}

// ======================================================== direct messages

pub fn dm_between(c: &Connection, a: &str, b: &str) -> R<Option<i64>> {
    let (a, b) = if a < b { (a, b) } else { (b, a) };
    c.query_row(
        "SELECT channel_id FROM chat_dm WHERE user_a = ?1 AND user_b = ?2",
        params![a, b],
        |r| r.get(0),
    )
    .optional()
}

pub fn create_dm(c: &mut Connection, id: i64, a: &str, b: &str) -> R<i64> {
    let (a, b) = if a < b { (a, b) } else { (b, a) };
    let tx = c.transaction()?;
    if let Some(existing) = tx
        .query_row(
            "SELECT channel_id FROM chat_dm WHERE user_a = ?1 AND user_b = ?2",
            params![a, b],
            |r| r.get(0),
        )
        .optional()?
    {
        return Ok(existing);
    }
    tx.execute(
        "INSERT INTO chat_channels (id, kind, created_at) VALUES (?1, 'dm', ?2)",
        params![id, now_s()],
    )?;
    tx.execute(
        "INSERT INTO chat_dm (channel_id, user_a, user_b) VALUES (?1, ?2, ?3)",
        params![id, a, b],
    )?;
    tx.commit()?;
    Ok(id)
}

/// The two sides of a direct-message channel.
pub fn dm_users(c: &Connection, channel: i64) -> R<Option<(String, String)>> {
    c.query_row(
        "SELECT user_a, user_b FROM chat_dm WHERE channel_id = ?1",
        [channel],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .optional()
}

/// `user`'s direct-message channels with the other side.
pub fn user_dms(c: &Connection, user: &str) -> R<Vec<(ChannelRow, String)>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {}, CASE WHEN d.user_a = ?1 THEN d.user_b ELSE d.user_a END
         FROM chat_dm d JOIN chat_channels ch ON ch.id = d.channel_id
         WHERE d.user_a = ?1 OR d.user_b = ?1",
        CHANNEL_COLS
            .split(", ")
            .map(|c| format!("ch.{}", c.trim()))
            .collect::<Vec<_>>()
            .join(", ")
    ))?;
    st.query_map([user], |r| Ok((channel_from(r)?, r.get(12)?)))?.collect()
}

// ================================================================== messages

#[derive(Clone, Debug, Default)]
pub struct MessageRow {
    pub id: i64,
    pub channel_id: i64,
    pub author_id: String,
    pub content: String,
    pub reply_to: Option<i64>,
    pub mentions: Mentions,
    pub attachments: Vec<Attachment>,
    pub pinned_at: Option<i64>,
    pub edited_at: Option<i64>,
    pub deleted_at: Option<i64>,
    pub call: Option<farwing_protocol::chat::CallRecord>,
}

const MESSAGE_COLS: &str =
    "id, channel_id, author_id, content, reply_to, mentions, attachments, pinned_at, edited_at, deleted_at, call";

fn message_from(r: &Row<'_>) -> R<MessageRow> {
    let mentions: String = r.get(5)?;
    let attachments: String = r.get(6)?;
    Ok(MessageRow {
        id: r.get(0)?,
        channel_id: r.get(1)?,
        author_id: r.get(2)?,
        content: r.get(3)?,
        reply_to: r.get(4)?,
        mentions: serde_json::from_str(&mentions).unwrap_or_default(),
        attachments: serde_json::from_str(&attachments).unwrap_or_default(),
        pinned_at: r.get(7)?,
        edited_at: r.get(8)?,
        deleted_at: r.get(9)?,
        call: r
            .get::<_, Option<String>>(10)?
            .and_then(|s| serde_json::from_str(&s).ok()),
    })
}

pub fn message(c: &Connection, id: i64) -> R<Option<MessageRow>> {
    c.query_row(
        &format!("SELECT {MESSAGE_COLS} FROM chat_messages WHERE id = ?1"),
        [id],
        message_from,
    )
    .optional()
}

pub fn messages_by_id(c: &Connection, ids: &[i64]) -> R<HashMap<i64, MessageRow>> {
    let mut out = HashMap::new();
    let mut st = c.prepare_cached(&format!("SELECT {MESSAGE_COLS} FROM chat_messages WHERE id = ?1"))?;
    for id in ids {
        if let Some(m) = st.query_row([id], message_from).optional()? {
            out.insert(*id, m);
        }
    }
    Ok(out)
}

/// Insert a message (its attachments already bound by the caller's
/// transaction) and index it.
pub fn insert_message(c: &mut Connection, m: &MessageRow) -> R<()> {
    let tx = c.transaction()?;
    tx.execute(
        &format!("INSERT INTO chat_messages ({MESSAGE_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, NULL, NULL, NULL, ?8)"),
        params![
            m.id,
            m.channel_id,
            m.author_id,
            m.content,
            m.reply_to,
            serde_json::to_string(&m.mentions).unwrap_or_default(),
            serde_json::to_string(&m.attachments).unwrap_or_default(),
            m.call.map(|c| serde_json::to_string(&c).unwrap_or_default())
        ],
    )?;
    for a in &m.attachments {
        tx.execute(
            "UPDATE chat_attachments SET message_id = ?2 WHERE id = ?1",
            params![a.id.0, m.id],
        )?;
    }
    if !m.content.is_empty() {
        tx.execute(
            "INSERT INTO chat_messages_fts (rowid, content) VALUES (?1, ?2)",
            params![m.id, m.content],
        )?;
    }
    tx.commit()
}

pub fn edit_message(c: &mut Connection, id: i64, content: &str, mentions: &Mentions, at_ms: i64) -> R<()> {
    let tx = c.transaction()?;
    tx.execute(
        "UPDATE chat_messages SET content = ?2, mentions = ?3, edited_at = ?4 WHERE id = ?1",
        params![id, content, serde_json::to_string(mentions).unwrap_or_default(), at_ms],
    )?;
    tx.execute("DELETE FROM chat_messages_fts WHERE rowid = ?1", [id])?;
    if !content.is_empty() {
        tx.execute(
            "INSERT INTO chat_messages_fts (rowid, content) VALUES (?1, ?2)",
            params![id, content],
        )?;
    }
    tx.commit()
}

/// Withdraw a message: its content and attachments go (the attachment IDs
/// whose files are to be removed come back).
pub fn delete_message(c: &mut Connection, id: i64) -> R<Vec<i64>> {
    let tx = c.transaction()?;
    let files: Vec<i64> = {
        let mut st = tx.prepare("SELECT id FROM chat_attachments WHERE message_id = ?1")?;
        st.query_map([id], |r| r.get(0))?.collect::<R<_>>()?
    };
    tx.execute("DELETE FROM chat_attachments WHERE message_id = ?1", [id])?;
    tx.execute(
        "UPDATE chat_messages SET content = '', attachments = '[]', mentions = '{}', pinned_at = NULL,
         deleted_at = ?2 WHERE id = ?1",
        params![id, now_s()],
    )?;
    tx.execute("DELETE FROM chat_messages_fts WHERE rowid = ?1", [id])?;
    tx.commit()?;
    Ok(files)
}

pub fn set_pinned(c: &Connection, id: i64, pinned: bool) -> R<()> {
    c.execute(
        "UPDATE chat_messages SET pinned_at = ?2 WHERE id = ?1",
        params![id, pinned.then(now_s)],
    )?;
    Ok(())
}

pub fn pins(c: &Connection, channel: i64) -> R<Vec<MessageRow>> {
    let mut st = c.prepare_cached(&format!(
        "SELECT {MESSAGE_COLS} FROM chat_messages WHERE channel_id = ?1 AND pinned_at IS NOT NULL
         ORDER BY pinned_at DESC, id DESC"
    ))?;
    st.query_map([channel], message_from)?.collect()
}

pub fn pin_count(c: &Connection, channel: i64) -> R<u32> {
    c.query_row(
        "SELECT COUNT(*) FROM chat_messages WHERE channel_id = ?1 AND pinned_at IS NOT NULL",
        [channel],
        |r| r.get(0),
    )
}

/// Which page of a channel's history.
#[derive(Clone, Copy, Debug)]
pub enum Page {
    Latest,
    Before(i64),
    After(i64),
    Around(i64),
}

/// A page of a channel's messages at or after `floor`, oldest first.
pub fn page(c: &Connection, channel: i64, page: Page, limit: u32, floor: i64) -> R<Vec<MessageRow>> {
    let older = |before: i64, inclusive: bool, n: u32| -> R<Vec<MessageRow>> {
        let op = if inclusive { "<=" } else { "<" };
        let mut st = c.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS} FROM chat_messages WHERE channel_id = ?1 AND id {op} ?2 AND id >= ?3
             ORDER BY id DESC LIMIT ?4"
        ))?;
        let mut rows: Vec<MessageRow> = st
            .query_map(params![channel, before, floor, n], message_from)?
            .collect::<R<_>>()?;
        rows.reverse();
        Ok(rows)
    };
    let newer = |after: i64, n: u32| -> R<Vec<MessageRow>> {
        let mut st = c.prepare_cached(&format!(
            "SELECT {MESSAGE_COLS} FROM chat_messages WHERE channel_id = ?1 AND id > ?2 AND id >= ?3
             ORDER BY id LIMIT ?4"
        ))?;
        st.query_map(params![channel, after, floor, n], message_from)?.collect()
    };
    match page {
        Page::Latest => older(i64::MAX, true, limit),
        Page::Before(id) => older(id, false, limit),
        Page::After(id) => newer(id, limit),
        Page::Around(id) => {
            let mut rows = older(id, true, limit.div_ceil(2))?;
            rows.extend(newer(id, limit / 2)?);
            Ok(rows)
        }
    }
}

/// The newest message `user` sent in `channel` (slow mode).
pub fn last_sent(c: &Connection, channel: i64, user: &str) -> R<Option<i64>> {
    c.query_row(
        "SELECT MAX(id) FROM chat_messages WHERE channel_id = ?1 AND author_id = ?2",
        params![channel, user],
        |r| r.get(0),
    )
}

/// What to search for (§4.3).
pub struct Search<'a> {
    pub query: &'a str,
    pub author: Option<&'a str>,
    /// The channels to look in, each with the oldest message the caller
    /// may read there.
    pub channels: &'a [(i64, i64)],
    pub before: Option<i64>,
    pub after: Option<i64>,
    pub has_attachment: bool,
    pub limit: u32,
}

/// Messages scanned per search when the query is too short for the index.
const SCAN_LIMIT: u32 = 20_000;

/// Matching messages, newest first, and whether there are more.
pub fn search(c: &Connection, s: &Search<'_>) -> R<(Vec<MessageRow>, bool)> {
    if s.channels.is_empty() {
        return Ok((Vec::new(), false));
    }
    let q = s.query.trim();
    let mut sql = String::new();
    let mut args: Vec<rusqlite::types::Value> = Vec::new();
    let marks = vec!["?"; s.channels.len()].join(", ");
    let indexed = q.chars().count() >= 3;
    if indexed {
        sql.push_str(&format!(
            "SELECT {} FROM chat_messages m JOIN chat_messages_fts ON chat_messages_fts.rowid = m.id
             WHERE chat_messages_fts MATCH ? AND m.channel_id IN ({marks})",
            prefixed("m", MESSAGE_COLS)
        ));
        args.push(format!("\"{}\"", q.replace('"', "\"\"")).into());
    } else {
        // Too short for trigrams: the newest messages, scanned.
        sql.push_str(&format!(
            "SELECT {MESSAGE_COLS} FROM (SELECT * FROM chat_messages WHERE channel_id IN ({marks})
             AND deleted_at IS NULL"
        ));
    }
    for (ch, _) in s.channels {
        args.push((*ch).into());
    }
    let filter_prefix = if indexed { "m." } else { "" };
    let mut filters = String::new();
    if let Some(author) = s.author {
        filters.push_str(&format!(" AND {filter_prefix}author_id = ?"));
        args.push(author.to_owned().into());
    }
    if let Some(before) = s.before {
        filters.push_str(&format!(" AND {filter_prefix}id < ?"));
        args.push(before.into());
    }
    if let Some(after) = s.after {
        filters.push_str(&format!(" AND {filter_prefix}id > ?"));
        args.push(after.into());
    }
    if s.has_attachment {
        filters.push_str(&format!(" AND {filter_prefix}attachments != '[]'"));
    }
    if indexed {
        sql.push_str(&filters);
        sql.push_str(" AND m.deleted_at IS NULL ORDER BY m.id DESC LIMIT ?");
        args.push((i64::from(s.limit) * 2 + 1).into());
    } else {
        sql.push_str(&filters);
        sql.push_str(" ORDER BY id DESC LIMIT ?)");
        args.push(i64::from(SCAN_LIMIT).into());
        if !q.is_empty() {
            sql.push_str(" WHERE instr(lower(content), lower(?)) > 0");
            args.push(q.to_owned().into());
        }
        sql.push_str(" ORDER BY id DESC LIMIT ?");
        args.push((i64::from(s.limit) * 2 + 1).into());
    }
    let mut st = c.prepare(&sql)?;
    let floors: HashMap<i64, i64> = s.channels.iter().copied().collect();
    let mut rows: Vec<MessageRow> = st
        .query_map(params_from_iter(args), message_from)?
        .filter_map(|r| r.ok())
        .filter(|m| m.id >= floors.get(&m.channel_id).copied().unwrap_or(i64::MAX))
        .collect();
    let more = rows.len() > s.limit as usize;
    rows.truncate(s.limit as usize);
    Ok((rows, more))
}

fn prefixed(table: &str, cols: &str) -> String {
    cols.split(", ")
        .map(|c| format!("{table}.{}", c.trim()))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Withdraw `user`'s messages in `channels` since `since_id` (a ban that
/// cleans up). Returns `(channel, message)` pairs and the attachment IDs.
pub fn delete_recent_by(
    c: &mut Connection,
    user: &str,
    channels: &[i64],
    since_id: i64,
) -> R<(Vec<(i64, i64)>, Vec<i64>)> {
    let mut gone = Vec::new();
    let mut files = Vec::new();
    for ch in channels {
        let ids: Vec<i64> = {
            let mut st = c.prepare_cached(
                "SELECT id FROM chat_messages WHERE channel_id = ?1 AND author_id = ?2 AND id >= ?3
                 AND deleted_at IS NULL",
            )?;
            st.query_map(params![ch, user, since_id], |r| r.get(0))?.collect::<R<_>>()?
        };
        for id in ids {
            files.extend(delete_message(c, id)?);
            gone.push((*ch, id));
        }
    }
    Ok((gone, files))
}

/// Delete messages older than `before_id` (retention).
pub fn prune_messages(c: &mut Connection, before_id: i64) -> R<(usize, Vec<i64>)> {
    let tx = c.transaction()?;
    let files: Vec<i64> = {
        let mut st = tx.prepare(
            "SELECT a.id FROM chat_attachments a JOIN chat_messages m ON m.id = a.message_id WHERE m.id < ?1",
        )?;
        st.query_map([before_id], |r| r.get(0))?.collect::<R<_>>()?
    };
    tx.execute(
        "DELETE FROM chat_attachments WHERE message_id IN (SELECT id FROM chat_messages WHERE id < ?1)",
        [before_id],
    )?;
    tx.execute("DELETE FROM chat_messages_fts WHERE rowid < ?1", [before_id])?;
    let n = tx.execute("DELETE FROM chat_messages WHERE id < ?1", [before_id])?;
    tx.commit()?;
    Ok((n, files))
}

// =============================================================== attachments

#[derive(Clone, Debug, Default)]
pub struct AttachmentRow {
    pub id: i64,
    pub owner_id: String,
    pub channel_id: i64,
    pub group_id: Option<i64>,
    pub name: String,
    pub mime: String,
    pub size: i64,
    pub width: u32,
    pub height: u32,
    pub thumb: bool,
    pub uploaded: bool,
    pub upload_hash: Option<String>,
    pub created_at: i64,
    pub message_id: Option<i64>,
}

impl AttachmentRow {
    pub fn wire(&self) -> Attachment {
        Attachment {
            id: Id(self.id),
            name: self.name.clone(),
            mime: self.mime.clone(),
            size: self.size.max(0) as u64,
            width: self.width,
            height: self.height,
            thumb: self.thumb,
        }
    }
}

const ATTACHMENT_COLS: &str = "id, owner_id, channel_id, group_id, name, mime, size, width, height, thumb, \
                               uploaded, upload_hash, created_at, message_id";

fn attachment_from(r: &Row<'_>) -> R<AttachmentRow> {
    Ok(AttachmentRow {
        id: r.get(0)?,
        owner_id: r.get(1)?,
        channel_id: r.get(2)?,
        group_id: r.get(3)?,
        name: r.get(4)?,
        mime: r.get(5)?,
        size: r.get(6)?,
        width: r.get(7)?,
        height: r.get(8)?,
        thumb: r.get::<_, i64>(9)? != 0,
        uploaded: r.get::<_, i64>(10)? != 0,
        upload_hash: r.get(11)?,
        created_at: r.get(12)?,
        message_id: r.get(13)?,
    })
}

pub fn attachment(c: &Connection, id: i64) -> R<Option<AttachmentRow>> {
    c.query_row(
        &format!("SELECT {ATTACHMENT_COLS} FROM chat_attachments WHERE id = ?1"),
        [id],
        attachment_from,
    )
    .optional()
}

pub fn insert_attachment(c: &Connection, a: &AttachmentRow) -> R<()> {
    c.execute(
        &format!(
            "INSERT INTO chat_attachments ({ATTACHMENT_COLS}) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0, 0, 0, 0, ?8, ?9, NULL)"
        ),
        params![
            a.id,
            a.owner_id,
            a.channel_id,
            a.group_id,
            a.name,
            a.mime,
            a.size,
            a.upload_hash,
            a.created_at
        ],
    )?;
    Ok(())
}

pub fn finish_upload(c: &Connection, id: i64, sha256: &str, width: u32, height: u32, thumb: bool) -> R<()> {
    c.execute(
        "UPDATE chat_attachments SET uploaded = 1, upload_hash = NULL, sha256 = ?2, width = ?3, height = ?4,
         thumb = ?5 WHERE id = ?1",
        params![id, sha256, width, height, thumb],
    )?;
    Ok(())
}

pub fn delete_attachment(c: &Connection, id: i64) -> R<()> {
    c.execute("DELETE FROM chat_attachments WHERE id = ?1", [id])?;
    Ok(())
}

/// Bytes stored for a group (none: direct messages, counted per owner).
pub fn group_usage(c: &Connection, group: i64) -> R<i64> {
    c.query_row(
        "SELECT IFNULL(SUM(size), 0) FROM chat_attachments WHERE group_id = ?1",
        [group],
        |r| r.get(0),
    )
}

pub fn dm_usage(c: &Connection, owner: &str) -> R<i64> {
    c.query_row(
        "SELECT IFNULL(SUM(size), 0) FROM chat_attachments WHERE owner_id = ?1 AND group_id IS NULL",
        [owner],
        |r| r.get(0),
    )
}

/// Attachments never sent created before `before`, with their folders.
pub fn stale_attachments(c: &Connection, before: i64) -> R<Vec<(i64, Option<i64>)>> {
    let mut st = c.prepare(
        "SELECT id, group_id FROM chat_attachments WHERE message_id IS NULL AND created_at < ?1",
    )?;
    st.query_map([before], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
}

// =============================================================== read states

pub fn read_states(c: &Connection, user: &str) -> R<Vec<ReadState>> {
    let mut st = c.prepare_cached(
        "SELECT channel_id, last_read_id, mentions FROM chat_read_states WHERE user_id = ?1",
    )?;
    st.query_map([user], |r| {
        Ok(ReadState {
            channel_id: Id(r.get(0)?),
            last_read_id: Id(r.get(1)?),
            mentions: r.get(2)?,
        })
    })?
    .collect()
}

pub fn read_state(c: &Connection, user: &str, channel: i64) -> R<ReadState> {
    Ok(c.query_row(
        "SELECT last_read_id, mentions FROM chat_read_states WHERE user_id = ?1 AND channel_id = ?2",
        params![user, channel],
        |r| {
            Ok(ReadState {
                channel_id: Id(channel),
                last_read_id: Id(r.get(0)?),
                mentions: r.get(1)?,
            })
        },
    )
    .optional()?
    .unwrap_or(ReadState {
        channel_id: Id(channel),
        ..Default::default()
    }))
}

/// Read up to `id`: never backwards; the mentions after it are recounted.
pub fn mark_read(c: &Connection, user: &str, channel: i64, id: i64) -> R<ReadState> {
    let mentions: u32 = c.query_row(
        "SELECT COUNT(*) FROM chat_messages WHERE channel_id = ?1 AND id > ?2 AND deleted_at IS NULL
         AND author_id != ?3 AND mentions != '{}' AND (
             EXISTS (SELECT 1 FROM json_each(chat_messages.mentions, '$.users') WHERE value = ?3)
             OR json_extract(chat_messages.mentions, '$.everyone') = 1
             OR json_extract(chat_messages.mentions, '$.here') = 1
             OR EXISTS (SELECT 1 FROM json_each(chat_messages.mentions, '$.roles') j
                 JOIN chat_member_roles mr ON mr.role_id = CAST(j.value AS INTEGER) AND mr.user_id = ?3))",
        params![channel, id, user],
        |r| r.get(0),
    )?;
    c.execute(
        "INSERT INTO chat_read_states (user_id, channel_id, last_read_id, mentions) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (user_id, channel_id) DO UPDATE SET
             mentions = CASE WHEN ?3 >= last_read_id THEN ?4 ELSE mentions END,
             last_read_id = MAX(last_read_id, ?3)",
        params![user, channel, id, mentions],
    )?;
    read_state(c, user, channel)
}

/// One more mention for each of `users` in `channel`.
pub fn add_mentions(c: &Connection, users: &[String], channel: i64) -> R<()> {
    let mut st = c.prepare_cached(
        "INSERT INTO chat_read_states (user_id, channel_id, last_read_id, mentions) VALUES (?1, ?2, 0, 1)
         ON CONFLICT (user_id, channel_id) DO UPDATE SET mentions = mentions + 1",
    )?;
    for u in users {
        st.execute(params![u, channel])?;
    }
    Ok(())
}

// ================================================================ moderation

pub fn ban(c: &Connection, group: i64, user: &str, by: &str, reason: &str) -> R<()> {
    c.execute(
        "INSERT INTO chat_bans (group_id, user_id, by_user, reason, at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (group_id, user_id) DO UPDATE SET by_user = ?3, reason = ?4, at = ?5",
        params![group, user, by, reason, now_s()],
    )?;
    Ok(())
}

pub fn unban(c: &Connection, group: i64, user: &str) -> R<bool> {
    Ok(c.execute(
        "DELETE FROM chat_bans WHERE group_id = ?1 AND user_id = ?2",
        params![group, user],
    )? > 0)
}

pub fn banned(c: &Connection, group: i64, user: &str) -> R<bool> {
    Ok(c
        .query_row(
            "SELECT 1 FROM chat_bans WHERE group_id = ?1 AND user_id = ?2",
            params![group, user],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// `(user, by, reason, at)`, newest first.
pub fn bans(c: &Connection, group: i64) -> R<Vec<(String, String, String, i64)>> {
    let mut st = c.prepare(
        "SELECT user_id, by_user, reason, at FROM chat_bans WHERE group_id = ?1 ORDER BY at DESC",
    )?;
    st.query_map([group], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect()
}

#[derive(Clone, Debug, Default)]
pub struct InviteRow {
    pub code: String,
    pub group_id: i64,
    pub by_user: String,
    pub max_uses: Option<u32>,
    pub uses: u32,
    pub expires_at: Option<i64>,
    pub revoked_at: Option<i64>,
    pub created_at: i64,
}

impl InviteRow {
    pub fn usable(&self, now: i64) -> bool {
        self.revoked_at.is_none()
            && self.expires_at.is_none_or(|t| t > now)
            && self.max_uses.is_none_or(|n| self.uses < n)
    }
}

fn invite_from(r: &Row<'_>) -> R<InviteRow> {
    Ok(InviteRow {
        code: r.get(0)?,
        group_id: r.get(1)?,
        by_user: r.get(2)?,
        max_uses: r.get(3)?,
        uses: r.get(4)?,
        expires_at: r.get(5)?,
        revoked_at: r.get(6)?,
        created_at: r.get(7)?,
    })
}

const INVITE_COLS: &str = "code, group_id, by_user, max_uses, uses, expires_at, revoked_at, created_at";

pub fn insert_invite(c: &Connection, i: &InviteRow) -> R<()> {
    c.execute(
        &format!("INSERT INTO chat_invites ({INVITE_COLS}) VALUES (?1, ?2, ?3, ?4, 0, ?5, NULL, ?6)"),
        params![i.code, i.group_id, i.by_user, i.max_uses, i.expires_at, i.created_at],
    )?;
    Ok(())
}

pub fn invite(c: &Connection, code: &str) -> R<Option<InviteRow>> {
    c.query_row(
        &format!("SELECT {INVITE_COLS} FROM chat_invites WHERE code = ?1"),
        [code],
        invite_from,
    )
    .optional()
}

pub fn invites(c: &Connection, group: i64) -> R<Vec<InviteRow>> {
    let mut st = c.prepare(&format!(
        "SELECT {INVITE_COLS} FROM chat_invites WHERE group_id = ?1 ORDER BY created_at DESC"
    ))?;
    st.query_map([group], invite_from)?.collect()
}

pub fn use_invite(c: &Connection, code: &str) -> R<()> {
    c.execute("UPDATE chat_invites SET uses = uses + 1 WHERE code = ?1", [code])?;
    Ok(())
}

pub fn revoke_invite(c: &Connection, code: &str) -> R<()> {
    c.execute(
        "UPDATE chat_invites SET revoked_at = ?2 WHERE code = ?1 AND revoked_at IS NULL",
        params![code, now_s()],
    )?;
    Ok(())
}

/// `(id, group, by, at)` of the invitations waiting for `user`.
pub fn group_invites_for(c: &Connection, user: &str) -> R<Vec<(i64, i64, String, i64)>> {
    let mut st = c.prepare_cached(
        "SELECT i.id, i.group_id, i.by_user, i.at FROM chat_group_invites i
         JOIN chat_groups g ON g.id = i.group_id AND g.dissolved_at IS NULL
         WHERE i.user_id = ?1 ORDER BY i.at",
    )?;
    st.query_map([user], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect()
}

/// `(group, user, by, at)`.
pub fn group_invite(c: &Connection, id: i64) -> R<Option<(i64, String, String, i64)>> {
    c.query_row(
        "SELECT group_id, user_id, by_user, at FROM chat_group_invites WHERE id = ?1",
        [id],
        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
    )
    .optional()
}

/// Ask `user` into `group` (again: the same invitation, now from `by`).
pub fn insert_group_invite(c: &Connection, id: i64, group: i64, user: &str, by: &str) -> R<i64> {
    c.execute(
        "INSERT INTO chat_group_invites (id, group_id, user_id, by_user, at) VALUES (?1, ?2, ?3, ?4, ?5)
         ON CONFLICT (group_id, user_id) DO UPDATE SET by_user = ?4, at = ?5",
        params![id, group, user, by, now_s()],
    )?;
    c.query_row(
        "SELECT id FROM chat_group_invites WHERE group_id = ?1 AND user_id = ?2",
        params![group, user],
        |r| r.get(0),
    )
}

pub fn delete_group_invite(c: &Connection, id: i64) -> R<()> {
    c.execute("DELETE FROM chat_group_invites WHERE id = ?1", [id])?;
    Ok(())
}

pub fn insert_join_request(c: &Connection, group: i64, user: &str, via: &str) -> R<()> {
    c.execute(
        "INSERT INTO chat_join_requests (group_id, user_id, via, at) VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT (group_id, user_id) DO UPDATE SET via = ?3, at = ?4",
        params![group, user, via, now_s()],
    )?;
    Ok(())
}

/// `(user, via, at)`, oldest first.
pub fn join_requests(c: &Connection, group: i64) -> R<Vec<(String, String, i64)>> {
    let mut st = c.prepare(
        "SELECT user_id, via, at FROM chat_join_requests WHERE group_id = ?1 ORDER BY at",
    )?;
    st.query_map([group], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect()
}

pub fn take_join_request(c: &Connection, group: i64, user: &str) -> R<bool> {
    Ok(c.execute(
        "DELETE FROM chat_join_requests WHERE group_id = ?1 AND user_id = ?2",
        params![group, user],
    )? > 0)
}

pub fn audit(c: &Connection, group: i64, actor: &str, action: &str, target: &str, details: &serde_json::Value) -> R<()> {
    c.execute(
        "INSERT INTO chat_audit (group_id, actor_id, action, target, details, at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![group, actor, action, target, details.to_string(), now_s()],
    )?;
    Ok(())
}

/// `(id, actor, action, target, details, at)`, newest first.
#[allow(clippy::type_complexity)]
pub fn audit_entries(
    c: &Connection,
    group: i64,
    before: Option<i64>,
    limit: u32,
) -> R<Vec<(i64, String, String, String, String, i64)>> {
    let mut st = c.prepare(
        "SELECT id, actor_id, action, target, details, at FROM chat_audit
         WHERE group_id = ?1 AND id < ?2 ORDER BY id DESC LIMIT ?3",
    )?;
    st.query_map(params![group, before.unwrap_or(i64::MAX), limit], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?))
    })?
    .collect()
}

pub fn prune_audit(c: &Connection, older_than_s: i64) -> R<usize> {
    c.execute("DELETE FROM chat_audit WHERE at < ?1", [now_s() - older_than_s])
}

// ===================================================================== prefs

pub fn prefs(c: &Connection, user: &str) -> R<Prefs> {
    let text: Option<String> = c
        .query_row("SELECT prefs FROM chat_prefs WHERE user_id = ?1", [user], |r| r.get(0))
        .optional()?;
    Ok(text
        .and_then(|t| serde_json::from_str(&t).ok())
        .unwrap_or_else(Prefs::new))
}

pub fn set_prefs(c: &Connection, user: &str, prefs: &Prefs) -> R<()> {
    c.execute(
        "INSERT INTO chat_prefs (user_id, prefs) VALUES (?1, ?2)
         ON CONFLICT (user_id) DO UPDATE SET prefs = ?2",
        params![user, serde_json::to_string(prefs).unwrap_or_default()],
    )?;
    Ok(())
}
