//! Joining groups (`docs/protocol/chat-design.md` §5): invitation links,
//! friends asked in, public groups, approvals.

use super::{audit, forbidden, id, mine, mine_with, not_found};
use crate::chat::store::{self, InviteRow};
use crate::chat::{next_id, push};
use crate::error::{ApiError, ApiResult};
use crate::http::auth::AccountAuth;
use crate::http::{App, JsonBody};
use axum::Json;
use axum::extract::{Path, Query, State};
use farwing_protocol::ErrorCode;
use farwing_protocol::api::Empty;
use farwing_protocol::chat::{
    DiscoverList, GroupInvite, Id, Invite, InviteCreate, InviteList, InvitePreview, JoinOutcome, JoinRequest,
    JoinRequestList, UserRef, perm,
};
use farwing_protocol::error::reason;
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::json;

const CODE_ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz23456789";
const CODE_LEN: usize = 10;

fn new_code() -> String {
    crate::crypto::random_array::<CODE_LEN>()
        .iter()
        .map(|b| char::from(CODE_ALPHABET[usize::from(*b) % CODE_ALPHABET.len()]))
        .collect()
}

fn valid_code(code: &str) -> bool {
    code.len() == CODE_LEN && code.bytes().all(|b| CODE_ALPHABET.contains(&b))
}

fn wire_invite(c: &Connection, i: &InviteRow) -> crate::db::R<Invite> {
    Ok(Invite {
        code: i.code.clone(),
        group_id: Id(i.group_id),
        by: store::user(c, &i.by_user)?,
        max_uses: i.max_uses,
        uses: i.uses,
        expires_at: i.expires_at,
        revoked: i.revoked_at.is_some(),
        created_at: i.created_at,
    })
}

/// How a user comes in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Via {
    Invite,
    Public,
    /// Asked in by a member who may invite: no approval.
    Added,
}

impl Via {
    fn as_str(self) -> &'static str {
        match self {
            Via::Invite => "invite",
            Via::Public => "public",
            Via::Added => "added",
        }
    }
}

enum Joined {
    In,
    Pending,
}

/// Let `user` into `group`, or file a request when it needs approval.
fn join(c: &mut Connection, user: &str, group: i64, via: Via) -> crate::db::R<ApiResult<Joined>> {
    let Some(g) = store::group(c, group)? else {
        return Ok(Err(not_found()));
    };
    if g.dissolved_at.is_some() {
        return Ok(Err(not_found()));
    }
    if g.frozen_at.is_some() {
        return Ok(Err(super::frozen()));
    }
    if store::banned(c, group, user)? {
        return Ok(Err(ApiError::reason(ErrorCode::Forbidden, reason::CHAT_BANNED, "banned from the group")));
    }
    if store::member(c, group, user)?.is_some() {
        return Ok(Err(ApiError::reason(ErrorCode::Conflict, reason::CHAT_ALREADY_MEMBER, "already a member")));
    }
    if g.join_approval && via != Via::Added {
        store::insert_join_request(c, group, user, via.as_str())?;
        return Ok(Ok(Joined::Pending));
    }
    store::add_member(c, group, user)?;
    audit(c, group, user, "member_join", user, json!({ "via": via.as_str() }))?;
    Ok(Ok(Joined::In))
}

/// After `join`: tell the group and answer with the new member's view.
async fn joined(app: &App, user: &str, group: i64, outcome: Joined) -> ApiResult<JoinOutcome> {
    match outcome {
        Joined::Pending => {
            let u = user.to_owned();
            let me = app.db.call(move |c| store::user(c, &u)).await?;
            push::request(app, group, Some(me), None).await;
            Ok(JoinOutcome {
                joined: None,
                pending: true,
            })
        }
        Joined::In => {
            push::member(app, group, user).await;
            push::group_state(app, group, Some(vec![user.to_owned()])).await;
            let u = user.to_owned();
            let state = app
                .db
                .call(move |c| {
                    Ok(match mine(c, &u, group)? {
                        Ok((ctx, me)) => Some(ctx.state(c, &me)?),
                        Err(_) => None,
                    })
                })
                .await?;
            Ok(JoinOutcome {
                joined: state,
                pending: false,
            })
        }
    }
}

/// `GET /chat/groups/{id}/invites` (`manage_group`: all; `invite`: own)
pub async fn list(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<InviteList>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    let list = app
        .db
        .call(move |c| {
            let (ctx, me) = match mine_with(c, &u, group, perm::INVITE)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            let all = ctx.base(&me) & perm::MANAGE_GROUP != 0;
            let mut invites = Vec::new();
            for i in store::invites(c, group)? {
                if all || i.by_user == u {
                    invites.push(wire_invite(c, &i)?);
                }
            }
            Ok(Ok(InviteList { invites }))
        })
        .await??;
    Ok(Json(list))
}

/// `POST /chat/groups/{id}/invites {max_uses?, expires_s?}` (`invite`)
pub async fn create(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<InviteCreate>,
) -> ApiResult<Json<Invite>> {
    let group = id(&gid)?;
    if req.max_uses.is_some_and(|n| !(1..=100).contains(&n)) {
        return Err(ApiError::bad_request("max_uses is 1-100"));
    }
    if req.expires_s.is_some_and(|s| !(1800..=365 * 86_400).contains(&s)) {
        return Err(ApiError::bad_request("expires_s is 30 minutes to a year"));
    }
    app.limiter
        .take(&format!("chat-invite:{}", auth.user.id), 50, std::time::Duration::from_secs(3600))
        .map_err(ApiError::rate_limited)?;
    let u = auth.user.id.clone();
    let invite = app
        .db
        .call(move |c| {
            if let Err(e) = mine_with(c, &u, group, perm::INVITE)? {
                return Ok(Err(e));
            }
            let now = crate::db::now_s();
            let row = InviteRow {
                code: new_code(),
                group_id: group,
                by_user: u.clone(),
                max_uses: req.max_uses,
                uses: 0,
                expires_at: req.expires_s.map(|s| now + s as i64),
                revoked_at: None,
                created_at: now,
            };
            store::insert_invite(c, &row)?;
            audit(c, group, &u, "invite_create", &row.code, json!({ "max_uses": row.max_uses, "expires_at": row.expires_at }))?;
            Ok(Ok(wire_invite(c, &row)?))
        })
        .await??;
    Ok(Json(invite))
}

/// `GET /chat/invites/{code}`: what the invitation leads to.
pub async fn preview_invite(app: &App, user: &str, code: &str) -> ApiResult<InvitePreview> {
    if !valid_code(code) {
        return Err(not_found());
    }
    let (u, code) = (user.to_owned(), code.to_owned());
    app.db
        .call(move |c| {
            let Some(i) = store::invite(c, &code)? else {
                return Ok(Err(not_found()));
            };
            let Some(g) = store::group(c, i.group_id)?.filter(|g| g.dissolved_at.is_none()) else {
                return Ok(Err(not_found()));
            };
            Ok(Ok(InvitePreview {
                code: i.code.clone(),
                group: crate::chat::preview(&g, store::member_count(c, g.id)?),
                expired: !i.usable(crate::db::now_s()) || g.frozen_at.is_some(),
                member: store::member(c, g.id, &u)?.is_some(),
            }))
        })
        .await?
}

pub async fn preview(State(app): State<App>, auth: AccountAuth, Path(code): Path<String>) -> ApiResult<Json<InvitePreview>> {
    Ok(Json(preview_invite(&app, &auth.user.id, &code).await?))
}

/// `DELETE /chat/invites/{code}`: its creator or `manage_group`.
pub async fn revoke(State(app): State<App>, auth: AccountAuth, Path(code): Path<String>) -> ApiResult<Json<Empty>> {
    if !valid_code(&code) {
        return Err(not_found());
    }
    let u = auth.user.id.clone();
    app.db
        .call(move |c| {
            let Some(i) = store::invite(c, &code)? else {
                return Ok(Err(not_found()));
            };
            let (ctx, me) = match mine(c, &u, i.group_id)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if i.by_user != u && ctx.base(&me) & perm::MANAGE_GROUP == 0 {
                return Ok(Err(forbidden()));
            }
            store::revoke_invite(c, &code)?;
            audit(c, i.group_id, &u, "invite_revoke", &code, json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    Ok(Json(Empty {}))
}

/// `POST /chat/invites/{code}/join`
pub async fn join_by_invite(
    State(app): State<App>,
    auth: AccountAuth,
    Path(code): Path<String>,
) -> ApiResult<Json<JoinOutcome>> {
    if !valid_code(&code) {
        return Err(not_found());
    }
    app.limiter
        .take(&format!("chat-join:{}", auth.user.id), 30, std::time::Duration::from_secs(3600))
        .map_err(ApiError::rate_limited)?;
    let u = auth.user.id.clone();
    let (group, outcome) = app
        .db
        .call(move |c| {
            let Some(i) = store::invite(c, &code)? else {
                return Ok(Err(not_found()));
            };
            if !i.usable(crate::db::now_s()) {
                return Ok(Err(ApiError::reason(
                    ErrorCode::BadRequest,
                    reason::INVITE_EXPIRED,
                    "the invitation is used up, revoked or expired",
                )));
            }
            match join(c, &u, i.group_id, Via::Invite)? {
                Ok(outcome) => {
                    store::use_invite(c, &code)?;
                    Ok(Ok((i.group_id, outcome)))
                }
                Err(e) => Ok(Err(e)),
            }
        })
        .await??;
    Ok(Json(joined(&app, &auth.user.id, group, outcome).await?))
}

/// `POST /chat/groups/{id}/add {user}` (`invite`): ask a friend in.
pub async fn add_friend(
    State(app): State<App>,
    auth: AccountAuth,
    Path(gid): Path<String>,
    JsonBody(req): JsonBody<UserRef>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    let target = req.user.clone();
    let invite = app
        .db
        .call(move |c| {
            let (ctx, _) = match mine_with(c, &u, group, perm::INVITE)? {
                Ok(v) => v,
                Err(e) => return Ok(Err(e)),
            };
            if !crate::db::are_friends(c, &u, &target)? {
                return Ok(Err(ApiError::reason(ErrorCode::Forbidden, reason::CHAT_NOT_FRIENDS, "not friends")));
            }
            if store::member(c, group, &target)?.is_some() {
                return Ok(Err(ApiError::reason(ErrorCode::Conflict, reason::CHAT_ALREADY_MEMBER, "already a member")));
            }
            if store::banned(c, group, &target)? {
                return Ok(Err(ApiError::reason(ErrorCode::Forbidden, reason::CHAT_BANNED, "banned from the group")));
            }
            let id = store::insert_group_invite(c, next_id(), group, &target, &u)?;
            audit(c, group, &u, "member_invite", &target, json!({}))?;
            Ok(Ok(GroupInvite {
                id: Id(id),
                group: ctx.preview(store::member_count(c, group)?),
                by: store::user(c, &u)?,
                at: crate::db::now_s(),
            }))
        })
        .await??;
    push::invite(&app, &req.user, &invite);
    Ok(Json(Empty {}))
}

/// `POST /chat/group-invites/{id}/accept|reject`
pub async fn answer_group_invite(
    State(app): State<App>,
    auth: AccountAuth,
    Path((iid, answer)): Path<(String, String)>,
) -> ApiResult<Json<JoinOutcome>> {
    let invite = id(&iid)?;
    let accept = match answer.as_str() {
        "accept" => true,
        "reject" => false,
        _ => return Err(not_found()),
    };
    let u = auth.user.id.clone();
    let result = app
        .db
        .call(move |c| {
            let Some((group, user, _, _)) = store::group_invite(c, invite)? else {
                return Ok(Err(not_found()));
            };
            if user != u {
                return Ok(Err(not_found()));
            }
            store::delete_group_invite(c, invite)?;
            if !accept {
                return Ok(Ok(None));
            }
            Ok(join(c, &u, group, Via::Added)?.map(|o| Some((group, o))))
        })
        .await??;
    push::invite_removed(&app, &auth.user.id, invite);
    match result {
        Some((group, outcome)) => Ok(Json(joined(&app, &auth.user.id, group, outcome).await?)),
        None => Ok(Json(JoinOutcome::default())),
    }
}

#[derive(Deserialize)]
pub struct DiscoverQuery {
    #[serde(default)]
    q: String,
}

/// `GET /chat/discover?q=`: public groups by name.
pub async fn discover(
    State(app): State<App>,
    _auth: AccountAuth,
    Query(q): Query<DiscoverQuery>,
) -> ApiResult<Json<DiscoverList>> {
    let query = q.q.trim().chars().take(100).collect::<String>();
    let list = app
        .db
        .call(move |c| {
            let mut groups = Vec::new();
            for g in store::discover(c, &query, 50)? {
                groups.push(crate::chat::preview(&g, store::member_count(c, g.id)?));
            }
            Ok(DiscoverList { groups })
        })
        .await?;
    Ok(Json(list))
}

/// `POST /chat/groups/{id}/join`: a public group.
pub async fn join_public(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<JoinOutcome>> {
    let group = id(&gid)?;
    app.limiter
        .take(&format!("chat-join:{}", auth.user.id), 30, std::time::Duration::from_secs(3600))
        .map_err(ApiError::rate_limited)?;
    let u = auth.user.id.clone();
    let outcome = app
        .db
        .call(move |c| {
            match store::group(c, group)? {
                Some(g) if g.public && g.dissolved_at.is_none() => {}
                _ => return Ok(Err(not_found())),
            }
            join(c, &u, group, Via::Public)
        })
        .await??;
    Ok(Json(joined(&app, &auth.user.id, group, outcome).await?))
}

/// `GET /chat/groups/{id}/requests` (`manage_group`)
pub async fn requests(State(app): State<App>, auth: AccountAuth, Path(gid): Path<String>) -> ApiResult<Json<JoinRequestList>> {
    let group = id(&gid)?;
    let u = auth.user.id.clone();
    let list = app
        .db
        .call(move |c| {
            if let Err(e) = mine_with(c, &u, group, perm::MANAGE_GROUP)? {
                return Ok(Err(e));
            }
            let rows = store::join_requests(c, group)?;
            let ids: Vec<String> = rows.iter().map(|r| r.0.clone()).collect();
            let users = store::users(c, &ids)?;
            Ok(Ok(JoinRequestList {
                requests: rows
                    .into_iter()
                    .map(|(user, via, at)| JoinRequest {
                        user: users.get(&user).cloned().unwrap_or_default(),
                        via,
                        at,
                    })
                    .collect(),
            }))
        })
        .await??;
    Ok(Json(list))
}

/// `POST /chat/groups/{id}/requests/{user}/approve|reject` (`manage_group`)
pub async fn answer_request(
    State(app): State<App>,
    auth: AccountAuth,
    Path((gid, user, answer)): Path<(String, String, String)>,
) -> ApiResult<Json<Empty>> {
    let group = id(&gid)?;
    let approve = match answer.as_str() {
        "approve" => true,
        "reject" => false,
        _ => return Err(not_found()),
    };
    let u = auth.user.id.clone();
    let target = user.clone();
    app.db
        .call(move |c| {
            if let Err(e) = mine_with(c, &u, group, perm::MANAGE_GROUP)? {
                return Ok(Err(e));
            }
            if !store::take_join_request(c, group, &target)? {
                return Ok(Err(not_found()));
            }
            if approve {
                if store::banned(c, group, &target)? {
                    return Ok(Err(ApiError::reason(ErrorCode::Forbidden, reason::CHAT_BANNED, "banned from the group")));
                }
                store::add_member(c, group, &target)?;
                audit(c, group, &target, "member_join", &target, json!({ "via": "approved" }))?;
            }
            audit(c, group, &u, if approve { "request_approve" } else { "request_reject" }, &target, json!({}))?;
            Ok(Ok(()))
        })
        .await??;
    push::request(&app, group, None, Some((&user, approve))).await;
    if approve {
        push::member(&app, group, &user).await;
        push::group_state(&app, group, Some(vec![user])).await;
    }
    Ok(Json(Empty {}))
}
