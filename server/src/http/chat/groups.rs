//! Groups, members, bans, roles and the group audit log
//! (`docs/protocol/chat-design.md` §5, §6.2).

use super::{audit, can_manage_role, forbidden, grantable, id, mine, mine_with, name, not_found, outranks, text};
use crate::chat::store::{self, RoleRow};
use crate::chat::{GroupCtx, next_id, push};
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::{Path, Query, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{
    AuditEntry, AuditList, Ban, BanCreate, BanList, GroupCreate, GroupState, GroupUpdate, Id, MemberPage,
    MemberUpdate, Role, RoleCreate, RoleOrder, RoleUpdate, UserRef, limits, perm,
};
use farwing_protocol::error::reason;
use serde::Deserialize;
use serde_json::json;

fn owner_error() -> ApiError {
    ApiError::reason(ErrorCode::Conflict, reason::CHAT_OWNER, "the owner cannot do this; transfer first")
}

fn rank_error() -> ApiError {
    ApiError::reason(ErrorCode::Forbidden, reason::CHAT_ROLE_RANK, "role at or above your highest")
}

/// The caller's view of `group` (after a change).
async fn state_of(app: &App, user: &str, group: i64) -> ApiResult<Json<GroupState>> {
    let u = user.to_owned();
    let state = app
        .db
        .call(move |c| {
            Ok(match mine(c, &u, group)? {
                Ok((ctx, me)) => Ok(ctx.state(c, &me)?),
                Err(e) => Err(e),
            })
        })
        .await??;
    Ok(Json(state))
}

/// `POST /chat/groups`
pub async fn create(
    State(app): State<App>,
    auth: AccountAuth,
    JsonBody(req): JsonBody<GroupCreate>,
) -> ApiResult<Json<GroupState>> {
    let uid = auth.user.id.clone();
    if !app.cfg().chat.create_groups {
        let u = uid.clone();
        let role = app.db.call(move |c| crate::webdb::role(c, &u)).await?;
        if role != crate::webdb::ROLE_ADMIN {
            return Err(ApiError::reason(
                ErrorCode::Forbidden,
                reason::CHAT_GROUPS_DISABLED,
                "only administrators may create groups",
            ));
        }
    }
    let group_name = name(&req.name, limits::MAX_GROUP_NAME)?;
    let description = text(&req.description, limits::MAX_DESCRIPTION)?;
    let pick = |given: &str, default: &str| -> ApiResult<String> {
        if given.trim().is_empty() {
            Ok(default.to_owned())
        } else {
            name(given, limits::MAX_CHANNEL_NAME)
        }
    };
    let admin_role = pick(&req.admin_role, "Admin")?;
    let category = pick(&req.category, "Text channels")?;
    let text_channel = pick(&req.text_channel, "general")?;
    let voice_channel = pick(&req.voice_channel, "Voice")?;
    app.limiter
        .take(&format!("chat-group:{uid}"), 10, std::time::Duration::from_secs(3600))
        .map_err(ApiError::rate_limited)?;
    let u = uid.clone();
    let group = app
        .db
        .call(move |c| {
            store::create_group(
                c,
                &u,
                &group_name,
                &description,
                &store::Starter {
                    admin_role: &admin_role,
                    category: &category,
                    text: &text_channel,
                    voice: &voice_channel,
                },
                next_id,
            )
        })
        .await?;
    tracing::info!(group, user = %uid, "chat group created");
    push::group_state(&app, group, Some(vec![uid.clone()])).await;
    state_of(&app, &uid, group).await
}

/// `GET /chat/groups/{id}`
pub async fn get(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<GroupState>> {
    state_of(&app, &auth.user.id, id(&gid)?).await
}

/// `PATCH /chat/groups/{id}` (`manage_group`)
pub async fn update(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<GroupUpdate>,
) -> ApiResult<Json<GroupState>> {
    let group = id(&gid)?;
    let new_name = req.name.as_deref().map(|n| name(n, limits::MAX_GROUP_NAME)).transpose()?;
    let description = req
        .description
        .as_deref()
        .map(|d| text(d, limits::MAX_DESCRIPTION))
        .transpose()?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let (ctx, _) = match mine_with(c, &u, group, perm::MANAGE_GROUP)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let mut g = ctx.group.clone();
            if let Some(n) = new_name {
                g.name = n;
            }
            if let Some(d) = description {
                g.description = d;
            }
            if let Some(p) = req.public {
                g.public = p;
            }
            if let Some(a) = req.join_approval {
                g.join_approval = a;
            }
            store::update_group(c, &g)?;
            audit(
                c,
                group,
                &u,
                "group_update",
                "",
                json!({ "name": g.name, "public": g.public, "join_approval": g.join_approval }),
            )?;
            Ok(Ok(()))
        })
        .await??;
    push::group_state(&app, group, None).await;
    state_of(&app, &auth.user.id, group).await
}

/// `DELETE /chat/groups/{id}`: the owner dissolves it (restorable for 30 days).
pub async fn dissolve(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    let members = app
        .db
        .call(move |c| {
            let (ctx, _) = match mine(c, &u, group)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if !ctx.is_owner(&u) {
                return Ok(Err(forbidden()));
            }
            let mut g = ctx.group.clone();
            g.dissolved_at = Some(crate::db::now_s());
            store::update_group(c, &g)?;
            audit(c, group, &u, "group_dissolve", "", json!({}))?;
            Ok(Ok(store::member_ids(c, group)?))
        })
        .await??;
    push::group_removed(&app, group, &members, "dissolved");
    Ok(Json(Empty {}))
}

/// `POST /chat/groups/{id}/restore`: the owner, within 30 days.
pub async fn restore(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<GroupState>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let Some(mut g) = store::group(c, group)? else {
                return Ok(Err(not_found()));
            };
            let deadline = crate::db::now_s() - crate::chat::RESTORE_DAYS * 86_400;
            if g.owner_id != u || g.dissolved_at.is_none_or(|t| t < deadline) {
                return Ok(Err(not_found()));
            }
            g.dissolved_at = None;
            store::update_group(c, &g)?;
            audit(c, group, &u, "group_restore", "", json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    push::group_state(&app, group, None).await;
    state_of(&app, &auth.user.id, group).await
}

/// `POST /chat/groups/{id}/transfer {user}`: the owner hands the group on.
pub async fn transfer(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<UserRef>,
) -> ApiResult<Json<GroupState>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let (ctx, _) = match mine(c, &u, group)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if !ctx.is_owner(&u) {
                return Ok(Err(forbidden()));
            }
            if store::member(c, group, &req.user)?.is_none() {
                return Ok(Err(not_found()));
            }
            let mut g = ctx.group.clone();
            g.owner_id = req.user.clone();
            store::update_group(c, &g)?;
            audit(c, group, &u, "group_transfer", &req.user, json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    push::group_state(&app, group, None).await;
    state_of(&app, &auth.user.id, group).await
}

#[derive(Deserialize)]
pub struct MembersQuery {
    #[serde(default)]
    after: String,
    limit: Option<u32>,
}

/// `GET /chat/groups/{id}/members?after=&limit=` (≤ 1000 per page)
pub async fn members(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    Query(q): Query<MembersQuery>,
) -> ApiResult<Json<MemberPage>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    let limit = q.limit.unwrap_or(1000).clamp(1, 1000);
    let page = app
        .db
        .call(move |c| {
            if let Err(e) = mine(c, &u, group)? {
                return Ok(Err(e));
            }
            let rows = store::members(c, group, &q.after, limit)?;
            let ids: Vec<String> = rows.iter().map(|m| m.user_id.clone()).collect();
            let users = store::users(c, &ids)?;
            Ok(Ok(MemberPage {
                members: rows
                    .iter()
                    .map(|m| crate::chat::wire_member(m, users.get(&m.user_id).cloned().unwrap_or_default()))
                    .collect(),
            }))
        })
        .await??;
    Ok(Json(page))
}

/// `PATCH /chat/groups/{id}/members/{user}`: a member's group nickname
/// (its own always; others' with `manage_roles`), roles (`manage_roles`,
/// below the caller's rank) and mute (`mute`, members below the caller).
pub async fn update_member(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, target)): Path<(String, String)>,
    JsonBody(req): JsonBody<MemberUpdate>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let target = if target == "@me" { auth.user.id.clone() } else { target };
    let nickname = req
        .nickname
        .as_deref()
        .map(|n| text(n, limits::MAX_NICKNAME))
        .transpose()?;
    let u = auth.user.id.clone();
    let t = target.clone();
    app.db
        .call(move |c| {
            let (ctx, me) = match mine(c, &u, group)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let Some(mut m) = store::member(c, group, &t)? else {
                return Ok(Err(not_found()));
            };
            let base = ctx.base(&me);
            let own = t == u;
            if let Some(n) = nickname {
                if !own && (base & perm::MANAGE_ROLES == 0 || !outranks(&ctx, &me, &m)) {
                    return Ok(Err(forbidden()));
                }
                m.nickname = n;
            }
            if let Some(roles) = req.roles {
                if base & perm::MANAGE_ROLES == 0 {
                    return Ok(Err(forbidden()));
                }
                let mut wanted: Vec<i64> = Vec::new();
                for r in roles {
                    let Some(role) = ctx.role(r.0).filter(|role| role.id != group) else {
                        return Ok(Err(ApiError::bad_request("unknown role")));
                    };
                    if !wanted.contains(&role.id) {
                        wanted.push(role.id);
                    }
                }
                // Only roles below the caller's rank change hands.
                let before: Vec<i64> = m.roles.clone();
                for r in before.iter().chain(&wanted) {
                    let changed = before.contains(r) != wanted.contains(r);
                    let position = ctx.role(*r).map_or(0, |role| role.position);
                    if changed && !can_manage_role(&ctx, &me, position) {
                        return Ok(Err(rank_error()));
                    }
                }
                m.roles = wanted;
                audit(c, group, &u, "member_roles", &t, json!({ "roles": m.roles }))?;
            }
            if let Some(until) = req.muted_until {
                if base & perm::MUTE == 0 || !outranks(&ctx, &me, &m) {
                    return Ok(Err(forbidden()));
                }
                m.muted_until = until.filter(|t| *t > crate::db::now_s());
                audit(c, group, &u, "member_mute", &t, json!({ "until": m.muted_until }))?;
            }
            store::update_member(c, &m)?;
            Ok(Ok(()))
        })
        .await??;
    push::member(&app, group, &target).await;
    push::group_state(&app, group, Some(vec![target])).await;
    Ok(Json(Empty {}))
}

/// `DELETE /chat/groups/{id}/members/{user}`: kick (`kick`), or `@me` to leave.
pub async fn remove_member(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, target)): Path<(String, String)>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let leaving = target == "@me" || target == auth.user.id;
    let target = if leaving { auth.user.id.clone() } else { target };
    let u = auth.user.id.clone();
    let t = target.clone();
    app.db
        .call(move |c| {
            let (ctx, me) = match mine(c, &u, group)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if ctx.is_owner(&t) {
                return Ok(Err(owner_error()));
            }
            if !leaving {
                let Some(m) = store::member(c, group, &t)? else {
                    return Ok(Err(not_found()));
                };
                if ctx.base(&me) & perm::KICK == 0 || !outranks(&ctx, &me, &m) {
                    return Ok(Err(forbidden()));
                }
                audit(c, group, &u, "member_kick", &t, json!({}))?;
            }
            store::remove_member(c, group, &t)?;
            Ok(Ok(()))
        })
        .await??;
    push::group_removed(&app, group, std::slice::from_ref(&target), if leaving { "left" } else { "kicked" });
    push::member_removed(&app, group, &target).await;
    Ok(Json(Empty {}))
}

/// `GET /chat/groups/{id}/bans` (`ban`)
pub async fn bans(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<BanList>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    let list = app
        .db
        .call(move |c| {
            if let Err(e) = mine_with(c, &u, group, perm::BAN)? {
                return Ok(Err(e));
            }
            let rows = store::bans(c, group)?;
            let mut ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
            ids.extend(rows.iter().map(|r| r.1.clone()));
            let users = store::users(c, &ids)?;
            Ok(Ok(BanList {
                bans: rows
                    .into_iter()
                    .map(|(user, by, reason, at)| Ban {
                        user: users.get(&user).cloned().unwrap_or_default(),
                        by: users.get(&by).cloned().unwrap_or_default(),
                        reason,
                        at,
                    })
                    .collect(),
            }))
        })
        .await??;
    Ok(Json(list))
}

/// `PUT /chat/groups/{id}/bans/{user}` (`ban`): out of the group, and kept out.
pub async fn ban(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, target)): Path<(String, String)>,
    JsonBody(req): JsonBody<BanCreate>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    if !farwing_protocol::ids::is_uuid(&target) {
        return Err(not_found());
    }
    let reason_text = text(&req.reason, 512)?;
    let hours = i64::from(req.delete_hours.min(168));
    let u = auth.user.id.clone();
    let t = target.clone();
    let data = app.cfg().data_dir.clone();
    let (was_member, withdrawn) = app
        .db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::BAN)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if ctx.is_owner(&t) || t == u {
                return Ok(Err(owner_error()));
            }
            let member = store::member(c, group, &t)?;
            if let Some(m) = &member
                && !outranks(&ctx, &me, m)
            {
                return Ok(Err(forbidden()));
            }
            store::ban(c, group, &t, &u, &reason_text)?;
            store::remove_member(c, group, &t)?;
            c.execute(
                "DELETE FROM chat_join_requests WHERE group_id = ?1 AND user_id = ?2",
                rusqlite::params![group, &t],
            )?;
            let withdrawn = if hours > 0 {
                let channels: Vec<i64> = ctx.channels.iter().map(|ch| ch.id).collect();
                let since = crate::chat::id_at(crate::db::now_s() - hours * 3600);
                let (gone, files) = store::delete_recent_by(c, &t, &channels, since)?;
                for f in files {
                    crate::chat::media::remove(&data, Some(group), f);
                }
                gone
            } else {
                Vec::new()
            };
            audit(c, group, &u, "member_ban", &t, json!({ "reason": reason_text, "delete_hours": hours }))?;
            Ok(Ok((member.is_some(), withdrawn)))
        })
        .await??;
    if was_member {
        push::group_removed(&app, group, std::slice::from_ref(&target), "banned");
        push::member_removed(&app, group, &target).await;
    }
    for (channel, message) in withdrawn {
        let env = farwing_protocol::realtime::Envelope::new(
            farwing_protocol::chat::types::MESSAGE_DELETED,
            farwing_protocol::chat::MessageDeleted {
                channel_id: Id(channel),
                id: Id(message),
            },
        );
        push::to_channel(&app, channel, &env, None).await;
    }
    Ok(Json(Empty {}))
}

/// `DELETE /chat/groups/{id}/bans/{user}` (`ban`)
pub async fn unban(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, target)): Path<(String, String)>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            if let Err(e) = mine_with(c, &u, group, perm::BAN)? {
                return Ok(Err(e));
            }
            if !store::unban(c, group, &target)? {
                return Ok(Err(not_found()));
            }
            audit(c, group, &u, "member_unban", &target, json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    Ok(Json(Empty {}))
}

fn check_role_fields(name_: Option<&str>, color: Option<u32>) -> ApiResult<Option<String>> {
    if color.is_some_and(|c| c > 0xFF_FFFF) {
        return Err(ApiError::bad_request("color is 0xRRGGBB"));
    }
    name_.map(|n| name(n, limits::MAX_ROLE_NAME)).transpose()
}

/// `POST /chat/groups/{id}/roles` (`manage_roles`): a new role at the
/// bottom, just above `@everyone`.
pub async fn create_role(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<RoleCreate>,
) -> ApiResult<Json<Role>> {
    let group = id(&gid)?;
    let role_name = check_role_fields(Some(&req.name), Some(req.color))?.unwrap_or_default();
    let u = auth.user.id.clone();
    let role = app
        .db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::MANAGE_ROLES)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if ctx.roles.len() >= limits::MAX_ROLES {
                return Ok(Err(ApiError::bad_request("too many roles")));
            }
            if req.permissions & !grantable(&ctx, &me) & perm::ALL != 0 {
                return Ok(Err(forbidden()));
            }
            // New roles go to the bottom (position 1), the rest move up.
            let mut order: Vec<i64> = ctx.roles.iter().filter(|r| r.id != group).map(|r| r.id).collect();
            let role = RoleRow {
                id: next_id(),
                group_id: group,
                name: role_name,
                color: req.color,
                permissions: req.permissions & perm::ALL,
                position: 1,
                mentionable: req.mentionable,
            };
            store::insert_role(c, &role)?;
            order.insert(0, role.id);
            store::order_roles(c, group, &order)?;
            audit(c, group, &u, "role_create", &role.id.to_string(), json!({ "name": role.name }))?;
            Ok(Ok(crate::chat::wire_role(&role)))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(role))
}

/// `PATCH /chat/groups/{id}/roles/{role}` (`manage_roles`; `@everyone`'s
/// permissions too)
pub async fn update_role(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, rid)): Path<(String, String)>,
    JsonBody(req): JsonBody<RoleUpdate>,
) -> ApiResult<Json<Role>> {
    let group = id(&gid)?;
    let role_id = id(&rid)?;
    let new_name = check_role_fields(req.name.as_deref(), req.color)?;
    let u = auth.user.id.clone();
    let role = app
        .db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::MANAGE_ROLES)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let Some(mut role) = ctx.role(role_id).cloned() else {
                return Ok(Err(not_found()));
            };
            let everyone = role.id == group;
            if !everyone && !can_manage_role(&ctx, &me, role.position) {
                return Ok(Err(rank_error()));
            }
            if let Some(n) = new_name
                && !everyone
            {
                role.name = n;
            }
            if let Some(color) = req.color {
                role.color = color;
            }
            if let Some(m) = req.mentionable {
                role.mentionable = m;
            }
            if let Some(p) = req.permissions {
                let changed = (p ^ role.permissions) & perm::ALL;
                if changed & !grantable(&ctx, &me) != 0 {
                    return Ok(Err(forbidden()));
                }
                role.permissions = p & perm::ALL;
            }
            store::update_role(c, &role)?;
            audit(
                c,
                group,
                &u,
                "role_update",
                &role.id.to_string(),
                json!({ "name": role.name, "permissions": role.permissions }),
            )?;
            Ok(Ok(crate::chat::wire_role(&role)))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(role))
}

/// `DELETE /chat/groups/{id}/roles/{role}` (`manage_roles`)
pub async fn delete_role(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, rid)): Path<(String, String)>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let role_id = id(&rid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::MANAGE_ROLES)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let Some(role) = ctx.role(role_id).filter(|r| r.id != group).cloned() else {
                return Ok(Err(not_found()));
            };
            if !can_manage_role(&ctx, &me, role.position) {
                return Ok(Err(rank_error()));
            }
            store::delete_role(c, group, role_id)?;
            audit(c, group, &u, "role_delete", &role_id.to_string(), json!({ "name": role.name }))?;
            Ok(Ok(()))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

/// `PUT /chat/groups/{id}/roles/order {roles}`: every role but `@everyone`,
/// lowest first; roles at or above the caller's rank keep their places.
pub async fn order_roles(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<RoleOrder>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::MANAGE_ROLES)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let current: Vec<&RoleRow> = ctx.roles.iter().filter(|r| r.id != group).collect();
            let wanted: Vec<i64> = req.roles.iter().map(|r| r.0).collect();
            let mut sorted_a: Vec<i64> = current.iter().map(|r| r.id).collect();
            let mut sorted_b = wanted.clone();
            sorted_a.sort_unstable();
            sorted_b.sort_unstable();
            if sorted_a != sorted_b {
                return Ok(Err(ApiError::bad_request("list every role but @everyone once")));
            }
            for (i, rid) in wanted.iter().enumerate() {
                let role = current.iter().find(|r| r.id == *rid).expect("checked");
                let moved = role.position != i as i32 + 1;
                if moved && (!can_manage_role(&ctx, &me, role.position) || !can_manage_role(&ctx, &me, i as i32 + 1)) {
                    return Ok(Err(rank_error()));
                }
            }
            store::order_roles(c, group, &wanted)?;
            audit(c, group, &u, "role_order", "", json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    push::group_state(&app, group, None).await;
    Ok(Json(Empty {}))
}

#[derive(Deserialize)]
pub struct AuditQuery {
    before: Option<i64>,
}

/// A page of a group's audit log, newest first.
pub async fn audit_log_of(app: &App, group: i64, before: Option<i64>) -> ApiResult<AuditList> {
    let list = app
        .db
        .call(move |c| {
            let rows = store::audit_entries(c, group, before, 100)?;
            let ids: Vec<String> = rows.iter().map(|r| r.1.clone()).collect();
            let users = store::users(c, &ids)?;
            Ok(AuditList {
                entries: rows
                    .into_iter()
                    .map(|(id, actor, action, target, details, at)| AuditEntry {
                        id,
                        actor: users.get(&actor).cloned().unwrap_or_default(),
                        action,
                        target,
                        details: serde_json::from_str(&details).unwrap_or_default(),
                        at,
                    })
                    .collect(),
            })
        })
        .await?;
    Ok(list)
}

/// `GET /chat/groups/{id}/audit?before=` (`manage_group`)
pub async fn audit_log(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    Query(q): Query<AuditQuery>,
) -> ApiResult<Json<AuditList>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    app.db
        .call(move |c| Ok(mine_with(c, &u, group, perm::MANAGE_GROUP)?.map(|_| ())))
        .await??;
    Ok(Json(audit_log_of(&app, group, q.before).await?))
}

/// The group context of a group that exists (dissolved too), for the console.
pub fn group_ctx_any(c: &rusqlite::Connection, group: i64) -> crate::db::R<Option<GroupCtx>> {
    match store::group(c, group)? {
        Some(g) => GroupCtx::with(c, g).map(Some),
        None => Ok(None),
    }
}
