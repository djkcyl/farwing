//! Calls in direct messages (`docs/protocol/chat-design.md` §7.6): the
//! server rings, connects the two devices' signaling (`call.signal`) and
//! writes the record; the media goes between the devices (through relays
//! when it must). Each device signs its descriptions' DTLS fingerprints, so
//! the other side knows the media is the device's own (§10.2).

use super::store;
use crate::egress::Payer;
use crate::error::{ApiError, ApiResult};
use crate::http::App;
use farwing_protocol::ErrorCode;
use farwing_protocol::chat::{Call, CallEnded, CallRecord, CallSignal, Id, Status, types};
use farwing_protocol::error::reason;
use farwing_protocol::realtime::Envelope;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Nobody answers within this: missed.
const RING: Duration = Duration::from_secs(45);
/// A device in a call without its chat connection this long: the call ends.
const CHAT_GRACE: Duration = Duration::from_secs(30);
const MAX_SIGNAL: usize = 64 * 1024;

struct Live {
    call: Call,
    /// Egress sessions (relay credentials) of each side's device.
    caller_session: String,
    callee_session: String,
    gone_since: HashMap<String, Instant>,
}

#[derive(Default)]
pub struct Calls {
    st: Mutex<HashMap<String, Live>>,
}

fn gone() -> ApiError {
    ApiError::reason(ErrorCode::Conflict, reason::CALL_GONE, "the call is over")
}

impl Calls {
    /// The calls `user` is in.
    pub fn of(&self, user: &str) -> Vec<Call> {
        self.st
            .lock()
            .values()
            .filter(|l| l.call.caller.user_id == user || l.call.callee.user_id == user)
            .map(|l| l.call.clone())
            .collect()
    }

    fn get(&self, id: &str) -> Option<Call> {
        self.st.lock().get(id).map(|l| l.call.clone())
    }

    fn busy(&self, device: &str) -> bool {
        self.st
            .lock()
            .values()
            .any(|l| l.call.caller_device == device || l.call.callee_device == device)
    }

    /// The device's chat connection ended (`false`) or came back.
    pub fn chat_connection(&self, device: &str, up: bool) {
        let mut st = self.st.lock();
        for live in st.values_mut() {
            if live.call.caller_device == device || live.call.callee_device == device {
                if up {
                    live.gone_since.remove(device);
                } else {
                    live.gone_since.insert(device.to_owned(), Instant::now());
                }
            }
        }
    }
}

/// The call as `device` gets it: with its relays when it takes part.
fn for_device(app: &App, live: &Live, device: &str) -> Call {
    let mut call = live.call.clone();
    let session = if device == call.caller_device {
        Some(&live.caller_session)
    } else if device == call.callee_device {
        Some(&live.callee_session)
    } else {
        None
    };
    call.ice_servers = session
        .map(|sid| {
            let access = app.egress.access(sid, true);
            app.hub.relay_servers(sid, device, &access)
        })
        .unwrap_or_default();
    call
}

fn send(app: &App, device: &str, kind: &str, body: impl serde::Serialize) {
    app.chat.hub.send_device(device, &Envelope::new(kind, body));
}

/// `POST /chat/dm/{channel}/call`
pub async fn start(app: &App, user: &str, device: &crate::db::DeviceRow, channel: i64, video: bool) -> ApiResult<Call> {
    let u = user.to_owned();
    let found = app
        .db
        .call(move |c| {
            let Some(a) = super::access(c, &u, channel)? else {
                return Ok(None);
            };
            let Some((peer, friends)) = a.peer.clone() else {
                return Ok(None);
            };
            Ok(Some((
                friends,
                store::user(c, &u)?,
                store::user(c, &peer)?,
                store::status(c, &peer)?,
            )))
        })
        .await?;
    let Some((friends, caller, callee, callee_status)) = found else {
        return Err(crate::http::chat::not_found());
    };
    if !friends {
        return Err(ApiError::reason(ErrorCode::Forbidden, reason::CHAT_NOT_FRIENDS, "no longer friends"));
    }
    if app.chat.calls.busy(&device.id) {
        return Err(ApiError::new(ErrorCode::Conflict, "the device is in a call"));
    }
    let id = crate::crypto::new_uuid();
    let call = Call {
        id: id.clone(),
        channel_id: Id(channel),
        caller,
        caller_device: device.id.clone(),
        callee,
        callee_device: String::new(),
        state: "ringing".into(),
        video,
        started_at: crate::db::now_ms(),
        answered_at: None,
        caller_key: device.public_key.clone(),
        callee_key: String::new(),
        ice_servers: Vec::new(),
    };
    let caller_session = format!("call-{id}-a");
    app.egress.ledger.open(&caller_session, Payer::User(user.to_owned()));
    let access = app.egress.access(&caller_session, true);
    app.egress.apply(&caller_session, &access);
    let live = Live {
        call: call.clone(),
        caller_session,
        callee_session: format!("call-{id}-b"),
        gone_since: HashMap::new(),
    };
    let mine = for_device(app, &live, &device.id);
    app.chat.calls.st.lock().insert(id.clone(), live);
    // Do not disturb: no ringing; it stays unanswered.
    if callee_status != Status::Dnd {
        app.chat.hub.send_user(&call.callee.user_id, None, &Envelope::new(types::CALL_RING, &call));
    }
    app.chat.hub.send_user(user, Some(&device.id), &Envelope::new(types::CALL_OUTGOING, &call));
    let app2 = app.clone();
    tokio::spawn(async move {
        tokio::time::sleep(RING).await;
        if app2.chat.calls.get(&id).is_some_and(|c| c.state == "ringing") {
            end(&app2, &id, "missed").await;
        }
    });
    Ok(mine)
}

/// `POST /chat/calls/{id}/answer`
pub async fn answer(app: &App, user: &str, device: &crate::db::DeviceRow, id: &str) -> ApiResult<Call> {
    let call = {
        let mut st = app.chat.calls.st.lock();
        let live = st.get_mut(id).ok_or_else(gone)?;
        if live.call.callee.user_id != user || live.call.state != "ringing" {
            return Err(gone());
        }
        live.call.state = "active".into();
        live.call.callee_device = device.id.clone();
        live.call.callee_key = device.public_key.clone();
        live.call.answered_at = Some(crate::db::now_ms());
        live.call.clone()
    };
    let callee_session = format!("call-{id}-b");
    app.egress.ledger.open(&callee_session, Payer::User(user.to_owned()));
    let access = app.egress.access(&callee_session, true);
    app.egress.apply(&callee_session, &access);
    let (mine, theirs) = {
        let st = app.chat.calls.st.lock();
        let live = st.get(id).ok_or_else(gone)?;
        (for_device(app, live, &device.id), for_device(app, live, &call.caller_device))
    };
    send(app, &call.caller_device, types::CALL_ANSWERED, &theirs);
    // The callee's other devices stop ringing; the caller's others learn.
    for (u, except) in [(&call.callee.user_id, &device.id), (&call.caller.user_id, &call.caller_device)] {
        let ended = CallEnded {
            call_id: id.to_owned(),
            channel_id: call.channel_id,
            reason: "answered_elsewhere".into(),
        };
        app.chat.hub.send_user(u, Some(except), &Envelope::new(types::CALL_ENDED, &ended));
    }
    Ok(mine)
}

/// `POST /chat/calls/{id}/decline` (the callee) or `…/hangup` (either).
pub async fn finish(app: &App, user: &str, device: &str, id: &str, declined: bool) -> ApiResult<()> {
    let call = app.chat.calls.get(id).ok_or_else(gone)?;
    let allowed = if declined {
        call.callee.user_id == user && call.state == "ringing"
    } else {
        device == call.caller_device || device == call.callee_device
    };
    if !allowed {
        return Err(gone());
    }
    end(app, id, if declined { "declined" } else { "hangup" }).await;
    Ok(())
}

/// `POST /chat/calls/{id}/move`: another device of a side takes its place.
pub async fn take_over(app: &App, user: &str, device: &crate::db::DeviceRow, id: &str) -> ApiResult<Call> {
    let (old, call) = {
        let mut st = app.chat.calls.st.lock();
        let live = st.get_mut(id).ok_or_else(gone)?;
        if live.call.state != "active" {
            return Err(gone());
        }
        let old = if live.call.caller.user_id == user {
            let old = std::mem::replace(&mut live.call.caller_device, device.id.clone());
            live.call.caller_key = device.public_key.clone();
            old
        } else if live.call.callee.user_id == user {
            let old = std::mem::replace(&mut live.call.callee_device, device.id.clone());
            live.call.callee_key = device.public_key.clone();
            old
        } else {
            return Err(gone());
        };
        live.gone_since.remove(&old);
        (old, live.call.clone())
    };
    if old != device.id {
        send(
            app,
            &old,
            types::CALL_ENDED,
            CallEnded {
                call_id: id.to_owned(),
                channel_id: call.channel_id,
                reason: "moved".into(),
            },
        );
    }
    let (mine, other) = {
        let st = app.chat.calls.st.lock();
        let live = st.get(id).ok_or_else(gone)?;
        let other = if call.caller_device == device.id { &call.callee_device } else { &call.caller_device };
        (for_device(app, live, &device.id), (other.clone(), for_device(app, live, other)))
    };
    send(app, &other.0, types::CALL_ANSWERED, &other.1);
    Ok(mine)
}

/// `call.signal` on the chat connection: to the other device of the call.
pub fn signal(app: &App, device: &str, mut body: CallSignal) {
    if body.data.len() > MAX_SIGNAL || !matches!(body.kind.as_str(), "offer" | "answer" | "candidate") {
        return;
    }
    let Some(call) = app.chat.calls.get(&body.call_id) else { return };
    if call.state != "active" {
        return;
    }
    let to = if device == call.caller_device {
        call.callee_device.clone()
    } else if device == call.callee_device {
        call.caller_device.clone()
    } else {
        return;
    };
    body.from_device = device.to_owned();
    send(app, &to, types::CALL_SIGNAL, body);
}

/// The call is over: everyone's devices learn, the record goes in.
pub async fn end(app: &App, id: &str, why: &str) {
    let Some(live) = app.chat.calls.st.lock().remove(id) else { return };
    let call = live.call;
    for sid in [&live.caller_session, &live.callee_session] {
        app.egress.ledger.close(sid);
        app.egress.ended(sid);
    }
    let ended = CallEnded {
        call_id: id.to_owned(),
        channel_id: call.channel_id,
        reason: why.to_owned(),
    };
    for u in [&call.caller.user_id, &call.callee.user_id] {
        app.chat.hub.send_user(u, None, &Envelope::new(types::CALL_ENDED, &ended));
    }
    let record = CallRecord {
        duration_s: call
            .answered_at
            .map_or(0, |at| ((crate::db::now_ms() - at).max(0) / 1000) as u32),
        missed: call.answered_at.is_none(),
    };
    let (channel, caller) = (call.channel_id.0, call.caller.user_id.clone());
    let message = app
        .db
        .call(move |c| {
            let row = store::MessageRow {
                id: super::next_id(),
                channel_id: channel,
                author_id: caller.clone(),
                call: Some(record),
                ..Default::default()
            };
            store::insert_message(c, &row)?;
            store::mark_read(c, &caller, channel, row.id)?;
            super::wire_message(c, &row)
        })
        .await;
    match message {
        Ok(m) => super::push::to_channel(app, channel, &Envelope::new(types::MESSAGE, &m), None).await,
        Err(e) => tracing::warn!(error = %format!("{e:#}"), "call record not written"),
    }
}

/// Calls whose devices lost their chat connections end.
pub async fn sweep(app: &App) {
    let stale: Vec<String> = app
        .chat
        .calls
        .st
        .lock()
        .iter()
        .filter(|(_, l)| l.gone_since.values().any(|t| t.elapsed() > CHAT_GRACE))
        .map(|(id, _)| id.clone())
        .collect();
    for id in stale {
        end(app, &id, "failed").await;
    }
}
