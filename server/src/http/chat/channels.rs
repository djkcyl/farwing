//! Channels, categories, their order and role overwrites
//! (`docs/protocol/chat-design.md` §2.1, §3).

use super::{audit, forbidden, grantable, id, mine, mine_with, name, not_found, text};
use crate::chat::store::{self, ChannelRow};
use crate::chat::{next_id, push, wire_channel};
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::{Path, State};
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{Channel, ChannelCreate, ChannelKind, ChannelOrder, ChannelUpdate, Overwrite, limits, perm};
use serde_json::json;

fn check_voice(bitrate: Option<u32>, limit: Option<u32>, max_kbps: u32) -> ApiResult<()> {
    if bitrate.is_some_and(|b| !limits::VOICE_BITRATES.contains(&b) || b > max_kbps) {
        return Err(ApiError::bad_request("voice_bitrate is 32, 64, 96 or 128 within the server's limit"));
    }
    if limit.is_some_and(|l| l > limits::MAX_VOICE_LIMIT) {
        return Err(ApiError::bad_request("voice_limit is 0-99"));
    }
    Ok(())
}

/// `POST /chat/channels` (`manage_channels`, in the category too)
pub async fn create(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<ChannelCreate>,
) -> ApiResult<Json<Channel>> {
    if req.kind == ChannelKind::Dm {
        return Err(ApiError::bad_request("direct messages come from POST /chat/dm"));
    }
    let channel_name = name(&req.name, limits::MAX_CHANNEL_NAME)?;
    let topic = text(&req.topic, limits::MAX_TOPIC)?;
    let group = req.group_id.0;
    let voice_default = 64.min(app.cfg().chat.voice_max_kbps);
    let u = auth.user.id.clone();
    let channel = app
        .db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::MANAGE_CHANNELS)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if ctx.channels.len() >= limits::MAX_CHANNELS {
                return Ok(Err(ApiError::bad_request("too many channels")));
            }
            let parent = match req.parent_id {
                Some(p) if req.kind == ChannelKind::Category => {
                    let _ = p;
                    return Ok(Err(ApiError::bad_request("categories are not nested")));
                }
                Some(p) => match ctx.channel(p.0) {
                    Some(cat) if cat.kind == ChannelKind::Category => {
                        if ctx.channel_perms(&me, cat.id) & perm::MANAGE_CHANNELS == 0 {
                            return Ok(Err(forbidden()));
                        }
                        Some(cat.id)
                    }
                    _ => return Ok(Err(ApiError::bad_request("parent_id is not a category of the group"))),
                },
                None => None,
            };
            let position = ctx
                .channels
                .iter()
                .filter(|ch| ch.parent_id == parent)
                .map(|ch| ch.position + 1)
                .max()
                .unwrap_or(0);
            let row = ChannelRow {
                id: next_id(),
                group_id: Some(group),
                kind: req.kind,
                name: channel_name,
                topic,
                parent_id: parent,
                position,
                slow_mode_s: 0,
                voice_bitrate: voice_default,
                voice_limit: 0,
                voice_compat: false,
                created_at: crate::db::now_s(),
            };
            store::insert_channel(c, &row)?;
            audit(
                c,
                group,
                &u,
                "channel_create",
                &row.id.to_string(),
                json!({ "name": row.name, "kind": row.kind.as_str() }),
            )?;
            let ctx = crate::chat::GroupCtx::with(c, ctx.group)?;
            let p = ctx.channel_perms(&me, row.id);
            Ok(Ok(wire_channel(&row, Vec::new(), p, None)))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(channel))
}

/// The channel and the group it is in, for someone who may manage it.
fn managed(
    c: &rusqlite::Connection,
    user: &str,
    channel: i64,
) -> crate::db::R<ApiResult<(crate::chat::GroupCtx, store::MemberRow, ChannelRow)>> {
    let Some(ch) = store::channel(c, channel)? else {
        return Ok(Err(not_found()));
    };
    let Some(group) = ch.group_id else {
        return Ok(Err(not_found()));
    };
    let (ctx, me) = match mine(c, user, group)? {
        Ok(v) => v,
        Err(e) => return Ok(Err(e)),
    };
    let p = ctx.channel_perms(&me, channel);
    if p & perm::VIEW == 0 {
        return Ok(Err(not_found()));
    }
    if p & perm::MANAGE_CHANNELS == 0 {
        return Ok(Err(if ctx.group.frozen_at.is_some() { super::frozen() } else { forbidden() }));
    }
    Ok(Ok((ctx, me, ch)))
}

/// `PATCH /chat/channels/{id}` (`manage_channels` there)
pub async fn update(
    State(app): State<App>,
    auth: AccountAuth,
    Path(cid): Path<String>,
    JsonBody(req): JsonBody<ChannelUpdate>,
) -> ApiResult<Json<Channel>> {
    let channel = id(&cid)?;
    let new_name = req.name.as_deref().map(|n| name(n, limits::MAX_CHANNEL_NAME)).transpose()?;
    let topic = req.topic.as_deref().map(|t| text(t, limits::MAX_TOPIC)).transpose()?;
    if req.slow_mode_s.is_some_and(|s| s > limits::MAX_SLOW_MODE_S) {
        return Err(ApiError::bad_request("slow_mode_s is 0-21600"));
    }
    check_voice(req.voice_bitrate, req.voice_limit, app.cfg().chat.voice_max_kbps)?;
    // Compatible mode is a choice only where the server allows it (§10.4).
    if req.voice_compat.is_some() && app.cfg().e2ee != farwing_protocol::api::E2ee::Allowed {
        return Err(ApiError::bad_request("the server decides the voice mode"));
    }
    let u = auth.user.id.clone();
    let (group, out) = app
        .db
        .call(move |c| {
            let (ctx, me, mut ch) = match managed(c, &u, channel)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if let Some(n) = new_name {
                ch.name = n;
            }
            if let Some(t) = topic {
                ch.topic = t;
            }
            if let Some(parent) = req.parent_id {
                match parent {
                    Some(p) => match ctx.channel(p.0) {
                        Some(cat) if cat.kind == ChannelKind::Category && ch.kind != ChannelKind::Category => {
                            ch.parent_id = Some(cat.id)
                        }
                        _ => return Ok(Err(ApiError::bad_request("parent_id is not a category of the group"))),
                    },
                    None => ch.parent_id = None,
                }
            }
            if let Some(s) = req.slow_mode_s {
                ch.slow_mode_s = s;
            }
            if let Some(b) = req.voice_bitrate {
                ch.voice_bitrate = b;
            }
            if let Some(l) = req.voice_limit {
                ch.voice_limit = l;
            }
            if let Some(compat) = req.voice_compat {
                ch.voice_compat = compat;
            }
            store::update_channel(c, &ch)?;
            let group = ctx.group.id;
            audit(c, group, &u, "channel_update", &ch.id.to_string(), json!({ "name": ch.name }))?;
            let ctx = crate::chat::GroupCtx::with(c, ctx.group)?;
            let p = ctx.channel_perms(&me, ch.id);
            let last = store::last_message_ids(c, &[ch.id])?.get(&ch.id).copied();
            let ow = ctx.overwrites.get(&ch.id).cloned().unwrap_or_default();
            Ok(Ok((group, wire_channel(&ch, ow, p, last))))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(out))
}

/// `DELETE /chat/channels/{id}` (`manage_channels` there)
pub async fn remove(State(app): State<App>, auth: AccountAuth, Path(cid): Path<String>) -> ApiResult<Json<Empty>> {
    let channel = id(&cid)?;
    let u = auth.user.id.clone();
    let data = app.cfg().data_dir.clone();
    let group = app
        .db
        .call(move |c| {
            let (ctx, _, ch) = match managed(c, &u, channel)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let group = ctx.group.id;
            for f in store::delete_channel(c, channel)? {
                crate::chat::media::remove(&data, Some(group), f);
            }
            audit(c, group, &u, "channel_delete", &channel.to_string(), json!({ "name": ch.name }))?;
            Ok(Ok(group))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

/// `PUT /chat/groups/{id}/channels/order {channels}`: every channel of the
/// group in its new order, each with its category.
pub async fn order(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<ChannelOrder>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let (ctx, _) = match mine_with(c, &u, group, perm::MANAGE_CHANNELS)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let mut given: Vec<i64> = req.channels.iter().map(|p| p.id.0).collect();
            let mut all: Vec<i64> = ctx.channels.iter().map(|ch| ch.id).collect();
            given.sort_unstable();
            all.sort_unstable();
            if given != all {
                return Ok(Err(ApiError::bad_request("list every channel of the group once")));
            }
            let mut places = Vec::new();
            for p in &req.channels {
                let ch = ctx.channel(p.id.0).expect("checked");
                let parent = match p.parent_id {
                    Some(pid) => match ctx.channel(pid.0) {
                        Some(cat) if cat.kind == ChannelKind::Category && ch.kind != ChannelKind::Category => {
                            Some(cat.id)
                        }
                        _ => return Ok(Err(ApiError::bad_request("parent_id is not a category of the group"))),
                    },
                    None => None,
                };
                places.push((ch.id, parent));
            }
            store::order_channels(c, group, &places)?;
            audit(c, group, &u, "channel_order", "", json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

/// `PUT /chat/channels/{id}/overwrites/{role} {allow, deny}`
/// (`manage_channels` there; only bits the caller holds)
pub async fn put_overwrite(
    State(app): State<App>,
    auth: AccountAuth,
    Path((cid, rid)): Path<(String, String)>,
    JsonBody(req): JsonBody<Overwrite>,
) -> ApiResult<Json<Empty>> {
    let channel = id(&cid)?;
    let role = id(&rid)?;
    if (req.allow | req.deny) & !perm::CHANNEL != 0 || req.allow & req.deny != 0 {
        return Err(ApiError::bad_request("allow and deny are distinct channel permissions"));
    }
    let u = auth.user.id.clone();
    let group = app
        .db
        .call(move |c| {
            let (ctx, me, _) = match managed(c, &u, channel)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if ctx.role(role).is_none() {
                return Ok(Err(not_found()));
            }
            let held = grantable(&ctx, &me) | ctx.channel_perms(&me, channel);
            let before = ctx
                .overwrites
                .get(&channel)
                .and_then(|l| l.iter().find(|o| o.role_id.0 == role))
                .copied()
                .unwrap_or_default();
            let changed = (before.allow ^ req.allow) | (before.deny ^ req.deny);
            if changed & !held != 0 {
                return Ok(Err(forbidden()));
            }
            store::put_overwrite(c, channel, role, req.allow, req.deny)?;
            let group = ctx.group.id;
            audit(
                c,
                group,
                &u,
                "channel_overwrite",
                &channel.to_string(),
                json!({ "role": role.to_string(), "allow": req.allow, "deny": req.deny }),
            )?;
            Ok(Ok(group))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}
