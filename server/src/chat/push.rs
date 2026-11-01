//! Pushes on the chat connection (`docs/protocol/chat-design.md` §6.3): who
//! receives what. Group events go to members whose permissions show the
//! channel; direct-message events to both sides.

use super::{GroupCtx, store};
use crate::http::App;
use farwing_protocol::chat::{
    self, ChatUser, GroupRemoved, Id, MemberPush, MemberRemoved, Presence, Status, perm, types,
};
use farwing_protocol::realtime::Envelope;

/// Send `env` to the online users who see `channel` (but `except`).
pub async fn to_channel(app: &App, channel: i64, env: &Envelope, except: Option<&str>) {
    let online: Vec<String> = app.chat.hub.online().into_iter().collect();
    if online.is_empty() {
        return;
    }
    let targets = app
        .db
        .call(move |c| {
            let Some(ch) = store::channel(c, channel)? else {
                return Ok(Vec::new());
            };
            match ch.group_id {
                None => Ok(store::dm_users(c, channel)?
                    .map(|(a, b)| vec![a, b])
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|u| online.contains(u))
                    .collect()),
                Some(g) => {
                    let Some(ctx) = GroupCtx::load(c, g)? else {
                        return Ok(Vec::new());
                    };
                    let mut out = Vec::new();
                    for u in online {
                        if let Some(m) = store::member(c, g, &u)?
                            && ctx.channel_perms(&m, channel) & perm::VIEW != 0
                        {
                            out.push(u);
                        }
                    }
                    Ok(out)
                }
            }
        })
        .await
        .unwrap_or_default();
    let targets: Vec<String> = targets.into_iter().filter(|u| Some(u.as_str()) != except).collect();
    app.chat.hub.send_users(targets.iter(), env);
}

/// Each online member of `group` (or only `only`) gets the group as it
/// sees it now (`chat.group`).
pub async fn group_state(app: &App, group: i64, only: Option<Vec<String>>) {
    let online = app.chat.hub.online();
    let states = app
        .db
        .call(move |c| {
            let Some(ctx) = GroupCtx::load(c, group)? else {
                return Ok(Vec::new());
            };
            let users = match only {
                Some(users) => users,
                None => store::member_ids(c, group)?,
            };
            let mut out = Vec::new();
            for u in users.into_iter().filter(|u| online.contains(u)) {
                if let Some(m) = store::member(c, group, &u)? {
                    out.push((u, ctx.state(c, &m)?));
                }
            }
            Ok(out)
        })
        .await
        .unwrap_or_default();
    for (user, state) in states {
        app.chat.hub.send_user(&user, None, &Envelope::new(types::GROUP, state));
    }
}

/// `users` are out of `group` (`left`, `kicked`, `banned`, `dissolved`).
pub fn group_removed(app: &App, group: i64, users: &[String], reason: &str) {
    let env = Envelope::new(
        types::GROUP_REMOVED,
        GroupRemoved {
            group_id: Id(group),
            reason: reason.to_owned(),
        },
    );
    app.chat.hub.send_users(users.iter(), &env);
}

/// Online members of `group`.
async fn online_members(app: &App, group: i64) -> Vec<String> {
    let online = app.chat.hub.online();
    app.db
        .call(move |c| store::member_ids(c, group))
        .await
        .unwrap_or_default()
        .into_iter()
        .filter(|u| online.contains(u))
        .collect()
}

/// A member joined or changed (`chat.member` to the group).
pub async fn member(app: &App, group: i64, user: &str) {
    let u = user.to_owned();
    let Ok(Some(member)) = app
        .db
        .call(move |c| {
            Ok(match store::member(c, group, &u)? {
                Some(m) => Some(super::wire_member(&m, store::user(c, &u)?)),
                None => None,
            })
        })
        .await
    else {
        return;
    };
    let env = Envelope::new(
        types::MEMBER,
        MemberPush {
            group_id: Id(group),
            member,
        },
    );
    let users = online_members(app, group).await;
    app.chat.hub.send_users(users.iter(), &env);
}

/// A member left (`chat.member_removed` to the rest of the group).
pub async fn member_removed(app: &App, group: i64, user: &str) {
    let env = Envelope::new(
        types::MEMBER_REMOVED,
        MemberRemoved {
            group_id: Id(group),
            user_id: user.to_owned(),
        },
    );
    let users = online_members(app, group).await;
    app.chat.hub.send_users(users.iter(), &env);
}

/// Both sides get the direct-message channel `channel` anew (`chat.dm`).
pub async fn dm(app: &App, channel: i64) {
    let Ok(Some((a, b))) = app.db.call(move |c| store::dm_users(c, channel)).await else {
        return;
    };
    for (me, other) in [(a.clone(), b.clone()), (b, a)] {
        let m = me.clone();
        if let Ok(Some(dm)) = app.db.call(move |c| super::dm_for(c, &m, channel, &other)).await {
            app.chat.hub.send_user(&me, None, &Envelope::new(types::DM, dm));
        }
    }
}

/// The friendship of `a` and `b` changed: their direct messages follow.
pub async fn friendship_changed(app: &App, a: &str, b: &str) {
    let (x, y) = (a.to_owned(), b.to_owned());
    if let Ok(Some(channel)) = app.db.call(move |c| store::dm_between(c, &x, &y)).await {
        dm(app, channel).await;
    }
}

/// `user`'s status as others see it changed: tell its audience (and its
/// own devices, which see `invisible` as such).
pub async fn presence(app: &App, user: &str) {
    let u = user.to_owned();
    let chosen = app
        .db
        .call(move |c| store::status(c, &u))
        .await
        .unwrap_or(Status::Online);
    let activity = app.chat.hub.activity(user);
    let seen = super::effective_status(chosen, activity);
    if app.chat.hub.tell(user, seen) {
        let u = user.to_owned();
        let audience = app.db.call(move |c| store::audience(c, &u)).await.unwrap_or_default();
        let env = Envelope::new(
            types::PRESENCE,
            Presence {
                user_id: user.to_owned(),
                status: seen,
            },
        );
        app.chat.hub.send_users(audience.iter(), &env);
    }
    if activity.is_some() {
        let own = if chosen == Status::Invisible { Status::Invisible } else { seen };
        app.chat.hub.send_user(
            user,
            None,
            &Envelope::new(
                types::PRESENCE,
                Presence {
                    user_id: user.to_owned(),
                    status: own,
                },
            ),
        );
    }
}

/// `user`'s name or avatar changed (`chat.user` to its audience and itself).
pub async fn user_changed(app: &App, user: &str) {
    let u = user.to_owned();
    let Ok((me, audience)) = app
        .db
        .call(move |c| Ok((store::user(c, &u)?, store::audience(c, &u)?)))
        .await
    else {
        return;
    };
    let env = Envelope::new(types::USER, me);
    app.chat.hub.send_users(audience.iter(), &env);
    app.chat.hub.send_user(user, None, &env);
}

/// A group invitation waits for `user`.
pub fn invite(app: &App, user: &str, invite: &chat::GroupInvite) {
    app.chat.hub.send_user(user, None, &Envelope::new(types::INVITE, invite));
}

pub fn invite_removed(app: &App, user: &str, id: i64) {
    app.chat
        .hub
        .send_user(user, None, &Envelope::new(types::INVITE_REMOVED, chat::InviteRemoved { id: Id(id) }));
}

/// A join request for those of `group` who approve them; or its answer to
/// the one who asked.
pub async fn request(app: &App, group: i64, user: Option<ChatUser>, asker: Option<(&str, bool)>) {
    let Ok(Some((preview, approvers))) = app
        .db
        .call(move |c| {
            let Some(ctx) = GroupCtx::load(c, group)? else {
                return Ok(None);
            };
            let count = store::member_count(c, group)?;
            let mut approvers = Vec::new();
            for m in store::members(c, group, "", u32::MAX)? {
                if ctx.base(&m) & perm::MANAGE_GROUP != 0 {
                    approvers.push(m.user_id);
                }
            }
            Ok(Some((ctx.preview(count), approvers)))
        })
        .await
    else {
        return;
    };
    match asker {
        Some((asker, approved)) => app.chat.hub.send_user(
            asker,
            None,
            &Envelope::new(
                types::REQUEST,
                chat::RequestPush {
                    group: preview,
                    user: None,
                    approved,
                },
            ),
        ),
        None => app.chat.hub.send_users(
            approvers.iter(),
            &Envelope::new(
                types::REQUEST,
                chat::RequestPush {
                    group: preview,
                    user,
                    approved: false,
                },
            ),
        ),
    }
}
