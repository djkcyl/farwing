//! Chat HTTP API (`docs/protocol/chat-design.md` §6.2), under
//! `/api/v2/chat`, and the media files under `/media`.

pub mod admin;
pub mod channels;
pub mod files;
pub mod groups;
pub mod invites;
pub mod messages;
pub mod misc;

use super::App;
use crate::chat::store::{self, MemberRow};
use crate::chat::GroupCtx;
use crate::db::R;
use crate::error::{ApiError, ApiResult};
use axum::Router;
use axum::routing::{get, patch, post, put};
use farwing_protocol::ErrorCode;
use farwing_protocol::chat::perm;
use farwing_protocol::error::reason;
use rusqlite::Connection;

pub fn routes(r: Router<App>) -> Router<App> {
    r.route("/api/v2/chat/ticket", post(misc::ticket))
        .route("/api/v2/chat/state", get(misc::state))
        .route("/api/v2/chat/prefs", get(misc::prefs).put(misc::put_prefs))
        .route("/api/v2/chat/media-token", post(misc::media_token))
        .route("/api/v2/me/status", put(misc::put_status))
        .route("/api/v2/me/avatar", put(files::put_avatar).delete(files::delete_avatar))
        .route("/api/v2/chat/groups", post(groups::create))
        .route(
            "/api/v2/chat/groups/{id}",
            get(groups::get).patch(groups::update).delete(groups::dissolve),
        )
        .route("/api/v2/chat/groups/{id}/restore", post(groups::restore))
        .route("/api/v2/chat/groups/{id}/transfer", post(groups::transfer))
        .route(
            "/api/v2/chat/groups/{id}/icon",
            put(files::put_icon).delete(files::delete_icon),
        )
        .route("/api/v2/chat/groups/{id}/members", get(groups::members))
        .route(
            "/api/v2/chat/groups/{id}/members/{user}",
            patch(groups::update_member).delete(groups::remove_member),
        )
        .route("/api/v2/chat/groups/{id}/bans", get(groups::bans))
        .route(
            "/api/v2/chat/groups/{id}/bans/{user}",
            put(groups::ban).delete(groups::unban),
        )
        .route("/api/v2/chat/groups/{id}/roles", post(groups::create_role))
        .route("/api/v2/chat/groups/{id}/roles/order", put(groups::order_roles))
        .route(
            "/api/v2/chat/groups/{id}/roles/{role}",
            patch(groups::update_role).delete(groups::delete_role),
        )
        .route("/api/v2/chat/groups/{id}/audit", get(groups::audit_log))
        .route("/api/v2/chat/groups/{id}/channels/order", put(channels::order))
        .route("/api/v2/chat/channels", post(channels::create))
        .route(
            "/api/v2/chat/channels/{id}",
            patch(channels::update).delete(channels::remove),
        )
        .route(
            "/api/v2/chat/channels/{id}/overwrites/{role}",
            put(channels::put_overwrite),
        )
        .route(
            "/api/v2/chat/groups/{id}/invites",
            get(invites::list).post(invites::create),
        )
        .route(
            "/api/v2/chat/invites/{code}",
            get(invites::preview).delete(invites::revoke),
        )
        .route("/api/v2/chat/invites/{code}/join", post(invites::join_by_invite))
        .route("/api/v2/chat/groups/{id}/add", post(invites::add_friend))
        .route("/api/v2/chat/group-invites/{id}/{answer}", post(invites::answer_group_invite))
        .route("/api/v2/chat/discover", get(invites::discover))
        .route("/api/v2/chat/groups/{id}/join", post(invites::join_public))
        .route("/api/v2/chat/groups/{id}/requests", get(invites::requests))
        .route(
            "/api/v2/chat/groups/{id}/requests/{user}/{answer}",
            post(invites::answer_request),
        )
        .route("/api/v2/chat/dm", post(messages::open_dm))
        .route(
            "/api/v2/chat/channels/{id}/messages",
            get(messages::history).post(messages::send),
        )
        .route("/api/v2/chat/channels/{id}/pins", get(messages::pins))
        .route("/api/v2/chat/channels/{id}/read", post(messages::read))
        .route("/api/v2/chat/read-all", post(messages::read_all))
        .route(
            "/api/v2/chat/messages/{id}",
            patch(messages::edit).delete(messages::remove),
        )
        .route(
            "/api/v2/chat/messages/{id}/pin",
            put(messages::pin).delete(messages::unpin),
        )
        .route("/api/v2/chat/search", get(messages::search))
        .route("/api/v2/chat/attachments", post(files::create_attachment))
        .route("/media/upload/{id}", put(files::upload))
        .route("/media/attachments/{id}/{name}", get(files::download))
        .route("/media/thumbs/{id}", get(files::thumb))
        .route("/media/avatars/{user}", get(files::avatar))
        .route("/media/icons/{group}", get(files::icon))
        .route("/emoji/{file}", get(files::emoji))
        .route("/api/v2/admin/chat/groups", get(admin::groups))
        .route(
            "/api/v2/admin/chat/groups/{id}",
            patch(admin::update).delete(admin::dissolve),
        )
        .route("/api/v2/admin/chat/groups/{id}/restore", post(admin::restore))
        .route("/api/v2/admin/chat/groups/{id}/audit", get(admin::audit))
}

/// A chat ID from a path; `not_found` when it is not one.
pub(crate) fn id(text: &str) -> ApiResult<i64> {
    text.parse::<i64>()
        .ok()
        .filter(|n| *n > 0)
        .ok_or_else(ApiError::not_found)
}

pub(crate) fn forbidden() -> ApiError {
    ApiError::reason(ErrorCode::Forbidden, reason::CHAT_FORBIDDEN, "not allowed in this group or channel")
}

pub(crate) fn not_found() -> ApiError {
    ApiError::not_found()
}

/// A name of 1–`max` characters without control characters, trimmed.
pub(crate) fn name(text: &str, max: usize) -> ApiResult<String> {
    let text = text.trim();
    if text.is_empty() || text.chars().count() > max || text.chars().any(char::is_control) {
        return Err(ApiError::bad_request(format!("a name of 1-{max} characters")));
    }
    Ok(text.to_owned())
}

/// Optional text of at most `max` characters, trimmed.
pub(crate) fn text(text: &str, max: usize) -> ApiResult<String> {
    let text = text.trim();
    if text.chars().count() > max {
        return Err(ApiError::bad_request(format!("at most {max} characters")));
    }
    Ok(text.to_owned())
}

/// The group and the caller's membership; `not_found` when it is gone or
/// the caller is not in it.
pub(crate) fn mine(c: &Connection, user: &str, group: i64) -> R<ApiResult<(GroupCtx, MemberRow)>> {
    let Some(ctx) = GroupCtx::load(c, group)? else {
        return Ok(Err(not_found()));
    };
    let Some(me) = store::member(c, group, user)? else {
        return Ok(Err(not_found()));
    };
    Ok(Ok((ctx, me)))
}

/// The group and the caller's membership, which must hold `bits` group-wide.
pub(crate) fn mine_with(c: &Connection, user: &str, group: i64, bits: i64) -> R<ApiResult<(GroupCtx, MemberRow)>> {
    Ok(mine(c, user, group)?.and_then(|(ctx, me)| {
        if ctx.base(&me) & bits == bits {
            Ok((ctx, me))
        } else if ctx.group.frozen_at.is_some() {
            Err(frozen())
        } else {
            Err(forbidden())
        }
    }))
}

pub(crate) fn frozen() -> ApiError {
    ApiError::reason(ErrorCode::Forbidden, reason::CHAT_FROZEN, "the group is frozen")
}

/// Whether the caller (`me`) outranks member `target` (who is not the owner).
pub(crate) fn outranks(ctx: &GroupCtx, me: &MemberRow, target: &MemberRow) -> bool {
    !ctx.is_owner(&target.user_id) && ctx.rank(me) > ctx.rank(target)
}

/// What a role assignment or edit may touch: roles below the caller's
/// highest (owners and administrators of the top rank aside).
pub(crate) fn can_manage_role(ctx: &GroupCtx, me: &MemberRow, position: i32) -> bool {
    ctx.is_owner(&me.user_id) || position < ctx.rank(me)
}

/// The bits an editor may grant: an owner any, others only what they hold.
pub(crate) fn grantable(ctx: &GroupCtx, me: &MemberRow) -> i64 {
    if ctx.is_owner(&me.user_id) {
        perm::ALL
    } else {
        ctx.base(me)
    }
}

/// Write a group audit entry.
pub(crate) fn audit(c: &Connection, group: i64, actor: &str, action: &str, target: &str, details: serde_json::Value) -> R<()> {
    store::audit(c, group, actor, action, target, &details)
}
