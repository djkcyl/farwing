//! A chat connection (`docs/protocol/chat-design.md` §6.1): admitted by a
//! chat ticket, it says `chat.hello` and receives the snapshot, then the
//! pushes. It is not resumed: a client reconnects with a new ticket and
//! gets a new snapshot.

use super::{push, store};
use crate::db;
use crate::hub::Outbound;
use crate::http::App;
use crate::ratelimit::TokenBucket;
use axum::extract::ws::{CloseFrame, Message, WebSocket};
use farwing_protocol::chat::{self, HelloBody, IdleBody, Id, Presence, Typing, perm, types};
use farwing_protocol::realtime::{Envelope, Hello, Ping, close, types as rt};
use farwing_protocol::{ErrorBody, ErrorCode};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::Instant;

pub async fn run(app: App, socket: WebSocket, device: String, auth: crate::hub::LinkAuth) {
    let Some(user) = auth.user_id().map(str::to_owned) else {
        return;
    };
    let (tx, mut rx) = unbounded_channel::<Outbound>();
    let conn_id = crate::crypto::new_uuid();
    app.chat.hub.attach(&device, &auth, &conn_id, tx);
    let (mut sink, mut stream) = socket.split();
    let cfg = app.hub.config();
    let hello = Envelope::new(
        rt::HELLO,
        Hello {
            epoch: 0,
            session_id: None,
            resume_token: String::new(),
            heartbeat_ms: cfg.heartbeat_ms,
            idle_timeout_ms: cfg.idle_timeout_ms,
            resumed: false,
            probe_nodes: Vec::new(),
        },
    );
    if sink.send(Message::Text(hello.encode().into())).await.is_err() {
        app.chat.hub.detach(&device, &conn_id);
        return;
    }
    tracing::info!(device_id = %device, "chat connected");
    let heartbeat = Duration::from_millis(u64::from(cfg.heartbeat_ms));
    let idle = Duration::from_millis(u64::from(cfg.idle_timeout_ms));
    let mut ticker = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let idle_sleep = tokio::time::sleep(idle);
    tokio::pin!(idle_sleep);
    let mut bucket = TokenBucket::new(cfg.limits.rate_per_s, app.cfg().limits.ws_rate_burst);
    let mut ready = false;
    let mut close_with: Option<(u16, String)> = None;
    loop {
        tokio::select! {
            biased;
            out = rx.recv() => match out {
                Some(Outbound::Text(text)) => {
                    if sink.send(Message::Text(text.into())).await.is_err() {
                        break;
                    }
                }
                Some(Outbound::Close(code, why)) => {
                    close_with = Some((code, why));
                    break;
                }
                None => break,
            },
            msg = stream.next() => {
                let Some(Ok(msg)) = msg else { break };
                idle_sleep.as_mut().reset(Instant::now() + idle);
                match msg {
                    Message::Text(text) => {
                        if !bucket.try_take() {
                            close_with = Some((close::RATE_LIMITED, "rate limit exceeded".into()));
                            break;
                        }
                        if text.len() > cfg.limits.max_frame_bytes {
                            close_with = Some((close::PROTOCOL_ERROR, "frame too large".into()));
                            break;
                        }
                        let env = match Envelope::decode(text.as_str()) {
                            Ok(e) if e.v == farwing_protocol::PROTOCOL_VERSION => e,
                            Ok(_) => {
                                close_with = Some((close::VERSION_UNSUPPORTED, "unsupported version".into()));
                                break;
                            }
                            Err(_) => {
                                close_with = Some((close::PROTOCOL_ERROR, "invalid frame".into()));
                                break;
                            }
                        };
                        if let Some(reply) = frame(&app, &device, &user, &conn_id, &mut ready, env).await
                            && sink.send(Message::Text(reply.encode().into())).await.is_err()
                        {
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        close_with = Some((close::PROTOCOL_ERROR, "binary frames are not supported".into()));
                        break;
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            },
            _ = ticker.tick() => {
                let ping = Envelope::new(rt::PING, Ping { t: db::now_ms() });
                if sink.send(Message::Text(ping.encode().into())).await.is_err() {
                    break;
                }
            },
            _ = &mut idle_sleep => {
                close_with = Some((1001, "idle timeout".into()));
                break;
            },
        }
    }
    if let Some((code, why)) = close_with {
        let _ = sink
            .send(Message::Close(Some(CloseFrame {
                code,
                reason: why.into(),
            })))
            .await;
        let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
    }
    if app.chat.hub.detach(&device, &conn_id).is_some() {
        tracing::info!(device_id = %device, "chat disconnected");
        app.chat.voice.chat_connection(&device, false);
        app.chat.calls.chat_connection(&device, false);
        push::presence(&app, &user).await;
    }
}

fn error(re: Option<&str>, code: ErrorCode, msg: &str) -> Envelope {
    Envelope::error(re, ErrorBody::new(code, msg))
}

/// Handle one frame; what to answer at once.
async fn frame(
    app: &App,
    device: &str,
    user: &str,
    conn_id: &str,
    ready: &mut bool,
    env: Envelope,
) -> Option<Envelope> {
    let re = env.id.clone();
    match env.kind.as_str() {
        rt::PING => {
            let t = env.data.get("t").and_then(Value::as_i64).unwrap_or(0);
            Some(Envelope::new(rt::PONG, Ping { t }))
        }
        rt::PONG | rt::ACK | rt::ERROR => None,
        rt::BYE => {
            app.chat.hub.close_device(device);
            None
        }
        types::HELLO => {
            let body: HelloBody = env.body().unwrap_or_default();
            let u = user.to_owned();
            if let Some(status) = body.status.filter(|s| *s != chat::Status::Offline) {
                let u = u.clone();
                let _ = app.db.call(move |c| store::set_status(c, &u, status)).await;
            }
            let limits = chat_limits(app);
            let snap = app.db.call(move |c| super::snapshot(c, &u, limits)).await;
            let (mut snap, audience) = match snap {
                Ok(s) => s,
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "chat snapshot failed");
                    return Some(error(re.as_deref(), ErrorCode::Internal, "snapshot failed"));
                }
            };
            app.chat.hub.set_ready(device, conn_id, body.idle);
            app.chat.voice.chat_connection(device, true);
            app.chat.calls.chat_connection(device, true);
            super::fill_live(app, user, &mut snap);
            *ready = true;
            push::presence(app, user).await;
            let online = app.chat.hub.online();
            let statuses = {
                let wanted: Vec<String> = audience.into_iter().filter(|u| online.contains(u)).collect();
                let w = wanted.clone();
                let chosen = app
                    .db
                    .call(move |c| w.iter().map(|u| store::status(c, u)).collect::<Result<Vec<_>, _>>())
                    .await
                    .unwrap_or_default();
                wanted
                    .into_iter()
                    .zip(chosen)
                    .filter_map(|(u, chosen)| {
                        let s = super::effective_status(chosen, app.chat.hub.activity(&u));
                        (s != chat::Status::Offline).then_some(Presence { user_id: u, status: s })
                    })
                    .collect()
            };
            snap.presences = statuses;
            Some(match re {
                Some(id) => Envelope::ack(&id, snap),
                None => Envelope::new(rt::ACK, snap),
            })
        }
        _ if !*ready => Some(error(re.as_deref(), ErrorCode::BadRequest, "chat.hello first")),
        types::IDLE => {
            let body: IdleBody = env.body().unwrap_or_default();
            app.chat.hub.set_idle(device, conn_id, body.idle);
            push::presence(app, user).await;
            None
        }
        types::TYPING => {
            let Ok(body) = env.body::<Typing>() else {
                return Some(error(re.as_deref(), ErrorCode::BadRequest, "invalid chat.typing"));
            };
            let channel = body.channel_id.0;
            if !app.chat.hub.typing(user, channel) {
                return None;
            }
            let u = user.to_owned();
            let allowed = app
                .db
                .call(move |c| Ok(super::access(c, &u, channel)?.is_some_and(|a| a.can(perm::SEND))))
                .await
                .unwrap_or(false);
            if allowed {
                let push = Envelope::new(
                    types::TYPING,
                    Typing {
                        channel_id: Id(channel),
                        user_id: user.to_owned(),
                    },
                );
                push::to_channel(app, channel, &push, Some(user)).await;
            }
            None
        }
        types::VOICE_ANSWER => {
            let Ok(body) = env.body::<chat::VoiceOffer>() else {
                return Some(error(re.as_deref(), ErrorCode::BadRequest, "invalid voice.answer"));
            };
            if body.sdp.is_empty() || body.sdp.len() > 64 * 1024 {
                return Some(error(re.as_deref(), ErrorCode::BadRequest, "invalid voice.answer"));
            }
            super::voice::answer(app, device, body).await;
            None
        }
        types::CALL_SIGNAL => {
            let Ok(body) = env.body::<chat::CallSignal>() else {
                return Some(error(re.as_deref(), ErrorCode::BadRequest, "invalid call.signal"));
            };
            super::calls::signal(app, device, body);
            None
        }
        other => {
            tracing::debug!(kind = other, "unknown chat message type");
            Some(error(re.as_deref(), ErrorCode::BadRequest, "unknown message type"))
        }
    }
}

/// The limits clients check before sending.
pub fn chat_limits(app: &App) -> chat::ChatLimits {
    let cfg = app.cfg();
    chat::ChatLimits {
        max_attachment_bytes: u64::from(cfg.chat.max_attachment_mb) * 1024 * 1024,
        create_groups: cfg.chat.create_groups,
        voice_compat: cfg.e2ee == farwing_protocol::api::E2ee::Allowed,
        ..Default::default()
    }
}
