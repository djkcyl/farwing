//! Messages, pins, read positions, search and direct-message channels
//! (`docs/protocol/chat-design.md` §4).

use super::{audit, forbidden, id, not_found};
use crate::chat::store::{self, MessageRow, Page};
use crate::chat::{Access, access, next_id, push};
use crate::db::{R, now_ms, now_s};
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::{Path, Query, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{
    ChannelKind, Dm, Id, Message, MessageCreate, MessageDeleted, MessageEdit, MessagePage, PinsChanged, ReadAll,
    ReadBody, ReadPush, SearchResult, UserRef, limits, perm, types,
};
use farwing_protocol::error::reason;
use farwing_protocol::realtime::Envelope;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;
use std::time::Duration;

fn muted(until: i64) -> ApiError {
    ApiError::reason(ErrorCode::Forbidden, reason::CHAT_MUTED, "muted in this group").with_detail("until", until)
}

fn not_friends() -> ApiError {
    ApiError::reason(ErrorCode::Forbidden, reason::CHAT_NOT_FRIENDS, "no longer friends")
}

fn content_invalid() -> ApiError {
    ApiError::reason(ErrorCode::BadRequest, reason::CHAT_CONTENT_INVALID, "empty or too long")
        .with_detail("max", limits::MAX_CONTENT_CHARS)
}

/// Why `bits` are missing in a channel the caller sees: muted, no longer
/// friends, frozen, or not allowed.
fn refused(a: &Access) -> ApiError {
    let now = now_s();
    if let Some(m) = &a.member
        && let Some(until) = m.muted_until.filter(|t| *t > now)
    {
        return muted(until);
    }
    if let Some((_, false)) = &a.peer {
        return not_friends();
    }
    if a.ctx.as_ref().is_some_and(|ctx| ctx.group.frozen_at.is_some()) {
        return super::frozen();
    }
    forbidden()
}

/// The caller's access to a channel with messages; `not_found` otherwise.
fn channel_access(c: &Connection, user: &str, channel: i64) -> R<ApiResult<Access>> {
    Ok(match access(c, user, channel)? {
        Some(a) if a.channel.kind.has_messages() => Ok(a),
        _ => Err(not_found()),
    })
}

fn check_content(content: &str, attachments: usize) -> ApiResult<()> {
    let chars = content.chars().count();
    if chars > limits::MAX_CONTENT_CHARS as usize || (content.trim().is_empty() && attachments == 0) {
        return Err(content_invalid());
    }
    if attachments > limits::MAX_ATTACHMENTS as usize {
        return Err(ApiError::bad_request("at most 10 attachments"));
    }
    Ok(())
}

/// `POST /chat/dm {user}`: the direct-message channel with a friend.
pub async fn open_dm(State(app): State<App>, auth: AccountAuth, JsonBody(req): JsonBody<UserRef>) -> ApiResult<Json<Dm>> {
    if !farwing_protocol::ids::is_uuid(&req.user) || req.user == auth.user.id {
        return Err(not_found());
    }
    let u = auth.user.id.clone();
    let other = req.user.clone();
    let (dm, created) = app
        .db
        .call(move |c| {
            if let Some(ch) = store::dm_between(c, &u, &other)? {
                return Ok(Ok((crate::chat::dm_for(c, &u, ch, &other)?, false)));
            }
            if !crate::db::are_friends(c, &u, &other)? {
                return Ok(Err(not_friends()));
            }
            let ch = store::create_dm(c, next_id(), &u, &other)?;
            Ok(Ok((crate::chat::dm_for(c, &u, ch, &other)?, true)))
        })
        .await??;
    let dm = dm.ok_or_else(not_found)?;
    if created {
        push::dm(&app, dm.channel.id.0).await;
    }
    Ok(Json(dm))
}

#[derive(Deserialize)]
pub struct HistoryQuery {
    before: Option<String>,
    after: Option<String>,
    around: Option<String>,
    limit: Option<u32>,
}

/// `GET /chat/channels/{id}/messages?before=&after=&around=&limit=`
pub async fn history(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    Query(q): Query<HistoryQuery>,
) -> ApiResult<Json<MessagePage>> {
    let channel = id(&cid)?;
    let limit = q.limit.unwrap_or(50).clamp(1, limits::MAX_PAGE);
    let parse = |v: &Option<String>| v.as_deref().map(id).transpose();
    let page = match (parse(&q.before)?, parse(&q.after)?, parse(&q.around)?) {
        (Some(b), None, None) => Page::Before(b),
        (None, Some(a), None) => Page::After(a),
        (None, None, Some(r)) => Page::Around(r),
        (None, None, None) => Page::Latest,
        _ => return Err(ApiError::bad_request("one of before, after, around")),
    };
    let u = auth.user.id.clone();
    let out = app
        .db
        .call(move |c| {
            let a = match channel_access(c, &u, channel)? {
                Ok(a) => a,
                Err(e) => return Ok(Err(e)),
            };
            let rows = store::page(c, channel, page, limit, a.floor)?;
            Ok(Ok(MessagePage {
                messages: crate::chat::wire_messages(c, &rows)?,
            }))
        })
        .await??;
    Ok(Json(out))
}

/// `POST /chat/channels/{id}/messages`
pub async fn send(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<MessageCreate>,
) -> ApiResult<Json<Message>> {
    let channel = id(&cid)?;
    let uid = auth.user.id.clone();
    check_content(&req.content, req.attachments.len())?;
    if req.nonce.len() > 64 {
        return Err(ApiError::bad_request("nonce is at most 64 characters"));
    }
    if !req.nonce.is_empty()
        && let Some(existing) = app.chat.sent(&uid, &req.nonce)
    {
        let u = uid.clone();
        let nonce = req.nonce.clone();
        let msg = app
            .db
            .call(move |c| {
                let Some(row) = store::message(c, existing)?.filter(|m| m.author_id == u) else {
                    return Ok(None);
                };
                let mut m = crate::chat::wire_message(c, &row)?;
                m.nonce = nonce;
                Ok(Some(m))
            })
            .await?;
        if let Some(m) = msg {
            return Ok(Json(m));
        }
    }
    let cfg = app.cfg().chat.clone();
    app.limiter
        .take(
            &format!("chat-send:{uid}"),
            cfg.rate_messages,
            Duration::from_secs(u64::from(cfg.rate_window_s)),
        )
        .map_err(ApiError::rate_limited)?;
    let online = app.chat.hub.online();
    let u = uid.clone();
    let nonce = req.nonce.clone();
    let message = app
        .db
        .call(move |c| {
            let a = match channel_access(c, &u, channel)? {
                Ok(a) => a,
                Err(e) => return Ok(Err(e)),
            };
            let needs = if req.attachments.is_empty() { perm::SEND } else { perm::SEND | perm::ATTACH };
            if !a.can(needs) {
                return Ok(Err(refused(&a)));
            }
            let slow = a.channel.slow_mode_s;
            if slow > 0
                && !a.can(perm::MANAGE_MESSAGES)
                && let Some(last) = store::last_sent(c, channel, &u)?
            {
                let wait = Id(last).created_ms() + i64::from(slow) * 1000 - now_ms();
                if wait > 0 {
                    return Ok(Err(ApiError::reason(ErrorCode::RateLimited, reason::CHAT_SLOW_MODE, "slow mode")
                        .with_detail("retry_after_ms", wait)));
                }
            }
            let mut reply_author = None;
            if let Some(r) = req.reply_to {
                match store::message(c, r.0)? {
                    Some(orig) if orig.channel_id == channel && orig.id >= a.floor => {
                        reply_author = Some(orig.author_id);
                    }
                    _ => return Ok(Err(ApiError::bad_request("reply_to is not a message of this channel"))),
                }
            }
            let mut attachments = Vec::new();
            for aid in &req.attachments {
                match store::attachment(c, aid.0)? {
                    Some(f) if f.owner_id == u && f.uploaded && f.message_id.is_none() && f.channel_id == channel => {
                        if attachments.iter().any(|x: &farwing_protocol::chat::Attachment| x.id == *aid) {
                            continue;
                        }
                        attachments.push(f.wire());
                    }
                    _ => return Ok(Err(ApiError::bad_request("an attachment is not one uploaded for this channel"))),
                }
            }
            let mut mentions = crate::chat::parse_mentions(&req.content);
            if req.mention_reply
                && let Some(author) = &reply_author
                && author != &u
                && !mentions.users.contains(author)
            {
                mentions.users.push(author.clone());
            }
            let mentions = crate::chat::allowed_mentions(c, &a, mentions)?;
            let row = MessageRow {
                id: next_id(),
                channel_id: channel,
                author_id: u.clone(),
                content: req.content.clone(),
                reply_to: req.reply_to.map(|r| r.0),
                mentions,
                attachments,
                ..Default::default()
            };
            store::insert_message(c, &row)?;
            let notified = crate::chat::mentioned_users(c, &a, &u, &row.mentions, &online)?;
            store::add_mentions(c, &notified, channel)?;
            store::mark_read(c, &u, channel, row.id)?;
            let mut m = crate::chat::wire_message(c, &row)?;
            m.nonce = nonce;
            Ok(Ok(m))
        })
        .await??;
    app.chat.remember_sent(&uid, &req.nonce, message.id.0);
    push::to_channel(&app, channel, &Envelope::new(types::MESSAGE, &message), None).await;
    Ok(Json(message))
}

/// `PATCH /chat/messages/{id}`: the author only.
pub async fn edit(
    State(app): State<App>,
    auth: AccountAuth,
    Path(mid): Path<String>,
    JsonBody(req): JsonBody<MessageEdit>,
) -> ApiResult<Json<Message>> {
    let message = id(&mid)?;
    let u = auth.user.id.clone();
    let out = app
        .db
        .call(move |c| {
            let Some(row) = store::message(c, message)?.filter(|m| m.deleted_at.is_none()) else {
                return Ok(Err(not_found()));
            };
            let a = match channel_access(c, &u, row.channel_id)? {
                Ok(a) if row.id >= a.floor => a,
                Ok(_) => return Ok(Err(not_found())),
                Err(e) => return Ok(Err(e)),
            };
            if row.author_id != u {
                return Ok(Err(forbidden()));
            }
            if !a.can(perm::SEND) {
                return Ok(Err(refused(&a)));
            }
            if let Err(e) = check_content(&req.content, row.attachments.len()) {
                return Ok(Err(e));
            }
            let mentions = crate::chat::allowed_mentions(c, &a, crate::chat::parse_mentions(&req.content))?;
            store::edit_message(c, message, &req.content, &mentions, now_ms())?;
            let row = store::message(c, message)?.expect("just edited");
            Ok(Ok(crate::chat::wire_message(c, &row)?))
        })
        .await??;
    push::to_channel(&app, out.channel_id.0, &Envelope::new(types::MESSAGE_UPDATED, &out), None).await;
    Ok(Json(out))
}

/// `DELETE /chat/messages/{id}`: the author, or `manage_messages`.
pub async fn remove(State(app): State<App>, auth: AccountAuth, Path(mid): Path<String>) -> ApiResult<Json<Empty>> {
    let message = id(&mid)?;
    let u = auth.user.id.clone();
    let data = app.cfg().data_dir.clone();
    let (channel, was_pinned) = app
        .db
        .call(move |c| {
            let Some(row) = store::message(c, message)?.filter(|m| m.deleted_at.is_none()) else {
                return Ok(Err(not_found()));
            };
            let a = match channel_access(c, &u, row.channel_id)? {
                Ok(a) if row.id >= a.floor => a,
                Ok(_) => return Ok(Err(not_found())),
                Err(e) => return Ok(Err(e)),
            };
            let own = row.author_id == u;
            if !own && !a.can(perm::MANAGE_MESSAGES) {
                return Ok(Err(forbidden()));
            }
            for f in store::delete_message(c, message)? {
                crate::chat::media::remove(&data, a.group_id(), f);
            }
            if !own && let Some(g) = a.group_id() {
                audit(c, g, &u, "message_delete", &row.author_id, json!({ "channel": row.channel_id.to_string() }))?;
            }
            Ok(Ok((row.channel_id, row.pinned_at.is_some())))
        })
        .await??;
    let env = Envelope::new(
        types::MESSAGE_DELETED,
        MessageDeleted {
            channel_id: Id(channel),
            id: Id(message),
        },
    );
    push::to_channel(&app, channel, &env, None).await;
    if was_pinned {
        let env = Envelope::new(types::PINS, PinsChanged { channel_id: Id(channel) });
        push::to_channel(&app, channel, &env, None).await;
    }
    Ok(Json(Empty {}))
}

async fn set_pin(app: &App, user: &str, message: i64, pinned: bool) -> ApiResult<()> {
    let u = user.to_owned();
    let (channel, out) = app
        .db
        .call(move |c| {
            let Some(row) = store::message(c, message)?.filter(|m| m.deleted_at.is_none()) else {
                return Ok(Err(not_found()));
            };
            let a = match channel_access(c, &u, row.channel_id)? {
                Ok(a) if row.id >= a.floor => a,
                Ok(_) => return Ok(Err(not_found())),
                Err(e) => return Ok(Err(e)),
            };
            let allowed = if a.channel.kind == ChannelKind::Dm { a.can(perm::VIEW) } else { a.can(perm::MANAGE_MESSAGES) };
            if !allowed {
                return Ok(Err(forbidden()));
            }
            if pinned == row.pinned_at.is_some() {
                return Ok(Ok((row.channel_id, None)));
            }
            if pinned && store::pin_count(c, row.channel_id)? >= limits::MAX_PINS {
                return Ok(Err(ApiError::reason(ErrorCode::Conflict, reason::CHAT_PIN_LIMIT, "too many pinned messages")
                    .with_detail("max", limits::MAX_PINS)));
            }
            store::set_pinned(c, message, pinned)?;
            if let Some(g) = a.group_id() {
                audit(
                    c,
                    g,
                    &u,
                    if pinned { "message_pin" } else { "message_unpin" },
                    &message.to_string(),
                    json!({ "channel": row.channel_id.to_string() }),
                )?;
            }
            let row = store::message(c, message)?.expect("present");
            Ok(Ok((row.channel_id, Some(crate::chat::wire_message(c, &row)?))))
        })
        .await??;
    if let Some(m) = out {
        push::to_channel(app, channel, &Envelope::new(types::MESSAGE_UPDATED, &m), None).await;
        push::to_channel(app, channel, &Envelope::new(types::PINS, PinsChanged { channel_id: Id(channel) }), None).await;
    }
    Ok(())
}

/// `PUT /chat/messages/{id}/pin` (`manage_messages`; both sides of a
/// direct message)
pub async fn pin(State(app): State<App>, auth: AccountAuth, Path(mid): Path<String>) -> ApiResult<Json<Empty>> {
    set_pin(&app, &auth.user.id, id(&mid)?, true).await?;
    Ok(Json(Empty {}))
}

pub async fn unpin(State(app): State<App>, auth: AccountAuth, Path(mid): Path<String>) -> ApiResult<Json<Empty>> {
    set_pin(&app, &auth.user.id, id(&mid)?, false).await?;
    Ok(Json(Empty {}))
}

/// `GET /chat/channels/{id}/pins`, newest pin first.
pub async fn pins(State(app): State<App>, auth: AccountAuth, Path(cid): Path<String>) -> ApiResult<Json<MessagePage>> {
    let channel = id(&cid)?;
    let u = auth.user.id.clone();
    let out = app
        .db
        .call(move |c| {
            let a = match channel_access(c, &u, channel)? {
                Ok(a) => a,
                Err(e) => return Ok(Err(e)),
            };
            let rows: Vec<MessageRow> = store::pins(c, channel)?.into_iter().filter(|m| m.id >= a.floor).collect();
            Ok(Ok(MessagePage {
                messages: crate::chat::wire_messages(c, &rows)?,
            }))
        })
        .await??;
    Ok(Json(out))
}

/// Tell the reader's other devices, and the other side of a direct
/// message when both want read receipts.
async fn announce_read(app: &App, user: &str, device: &str, state: farwing_protocol::chat::ReadState, peer: Option<String>) {
    app.chat.hub.send_user(
        user,
        Some(device),
        &Envelope::new(
            types::READ,
            ReadPush {
                channel_id: state.channel_id,
                last_read_id: state.last_read_id,
                mentions: state.mentions,
                user_id: String::new(),
            },
        ),
    );
    if let Some(peer) = peer {
        let (u, p) = (user.to_owned(), peer.clone());
        let receipts = app
            .db
            .call(move |c| Ok(store::prefs(c, &u)?.read_receipts && store::prefs(c, &p)?.read_receipts))
            .await
            .unwrap_or(false);
        if receipts {
            app.chat.hub.send_user(
                &peer,
                None,
                &Envelope::new(
                    types::READ,
                    ReadPush {
                        channel_id: state.channel_id,
                        last_read_id: state.last_read_id,
                        mentions: 0,
                        user_id: user.to_owned(),
                    },
                ),
            );
        }
    }
}

/// `POST /chat/channels/{id}/read {message_id}`
pub async fn read(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<ReadBody>,
) -> ApiResult<Json<farwing_protocol::chat::ReadState>> {
    let channel = id(&cid)?;
    let u = auth.user.id.clone();
    let (state, peer) = app
        .db
        .call(move |c| {
            let a = match channel_access(c, &u, channel)? {
                Ok(a) => a,
                Err(e) => return Ok(Err(e)),
            };
            let state = store::mark_read(c, &u, channel, req.message_id.0.max(0))?;
            Ok(Ok((state, a.peer.map(|(p, _)| p))))
        })
        .await??;
    announce_read(&app, &auth.user.id, &auth.device.id, state, peer).await;
    Ok(Json(state))
}

/// `POST /chat/read-all {group?}`: everything (in one group) read.
pub async fn read_all(State(app): State<App>, auth: AccountAuth, JsonBody(req): JsonBody<ReadAll>) -> ApiResult<Json<Empty>> {
    let u = auth.user.id.clone();
    let states = app
        .db
        .call(move |c| {
            let mut channels: Vec<(i64, Option<String>)> = Vec::new();
            let groups = match req.group {
                Some(g) => vec![g.0],
                None => store::user_groups(c, &u)?.into_iter().map(|g| g.id).collect(),
            };
            for g in groups {
                if let Ok((ctx, me)) = super::mine(c, &u, g)? {
                    for ch in &ctx.channels {
                        if ch.kind.has_messages() && ctx.channel_perms(&me, ch.id) & perm::VIEW != 0 {
                            channels.push((ch.id, None));
                        }
                    }
                }
            }
            if req.group.is_none() {
                for (ch, other) in store::user_dms(c, &u)? {
                    channels.push((ch.id, Some(other)));
                }
            }
            let ids: Vec<i64> = channels.iter().map(|(ch, _)| *ch).collect();
            let last = store::last_message_ids(c, &ids)?;
            let mut out = Vec::new();
            for (ch, peer) in channels {
                if let Some(id) = last.get(&ch) {
                    let before = store::read_state(c, &u, ch)?;
                    if before.last_read_id.0 < *id || before.mentions > 0 {
                        out.push((store::mark_read(c, &u, ch, *id)?, peer));
                    }
                }
            }
            Ok(out)
        })
        .await?;
    for (state, peer) in states {
        announce_read(&app, &auth.user.id, &auth.device.id, state, peer).await;
    }
    Ok(Json(Empty {}))
}

#[derive(Deserialize)]
pub struct SearchQuery {
    #[serde(default)]
    q: String,
    author: Option<String>,
    channel: Option<String>,
    group: Option<String>,
    before: Option<String>,
    after: Option<String>,
    /// `attachment`
    has: Option<String>,
    limit: Option<u32>,
}

/// `GET /chat/search?q=&author=&channel=&group=&before=&after=&has=`
pub async fn search(
    State(app): State<App>,
    auth: AccountAuth,
    Query(q): Query<SearchQuery>,
) -> ApiResult<Json<SearchResult>> {
    let query = q.q.trim().chars().take(200).collect::<String>();
    let author = q.author.filter(|a| farwing_protocol::ids::is_uuid(a));
    let has_attachment = q.has.as_deref() == Some("attachment");
    if query.is_empty() && author.is_none() && !has_attachment {
        return Err(ApiError::bad_request("search for words, an author or attachments"));
    }
    let channel = q.channel.as_deref().map(id).transpose()?;
    let group = q.group.as_deref().map(id).transpose()?;
    let before = q.before.as_deref().map(id).transpose()?;
    let after = q.after.as_deref().map(id).transpose()?;
    let limit = q.limit.unwrap_or(25).clamp(1, 50);
    app.limiter
        .take(&format!("chat-search:{}", auth.user.id), 60, Duration::from_secs(60))
        .map_err(ApiError::rate_limited)?;
    let u = auth.user.id.clone();
    let out = app
        .db
        .call(move |c| {
            let mut channels: Vec<(i64, i64)> = Vec::new();
            if let Some(ch) = channel {
                match channel_access(c, &u, ch)? {
                    Ok(a) => channels.push((ch, a.floor)),
                    Err(e) => return Ok(Err(e)),
                }
            } else {
                let groups = match group {
                    Some(g) => vec![g],
                    None => store::user_groups(c, &u)?.into_iter().map(|g| g.id).collect(),
                };
                for g in groups {
                    match super::mine(c, &u, g)? {
                        Ok((ctx, me)) => {
                            for ch in &ctx.channels {
                                let p = ctx.channel_perms(&me, ch.id);
                                if ch.kind.has_messages() && p & perm::VIEW != 0 {
                                    let floor = if p & perm::READ_HISTORY == 0 { crate::chat::id_at(me.joined_at) } else { 0 };
                                    channels.push((ch.id, floor));
                                }
                            }
                        }
                        Err(e) if group.is_some() => return Ok(Err(e)),
                        Err(_) => {}
                    }
                }
                if group.is_none() {
                    for (ch, _) in store::user_dms(c, &u)? {
                        channels.push((ch.id, 0));
                    }
                }
            }
            let (rows, more) = store::search(
                c,
                &store::Search {
                    query: &query,
                    author: author.as_deref(),
                    channels: &channels,
                    before,
                    after,
                    has_attachment,
                    limit,
                },
            )?;
            Ok(Ok(SearchResult {
                messages: crate::chat::wire_messages(c, &rows)?,
                more,
            }))
        })
        .await??;
    Ok(Json(out))
}
