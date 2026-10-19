//! Realtime WebSocket endpoint (`/api/v2/ws`). Admission is by a single-use
//! ticket (obtained over signed HTTPS) or a resume token.

use crate::crypto::new_uuid;
use crate::db;
use crate::error::ApiError;
use crate::http::auth::link_auth_valid;
use crate::http::{App, ClientIp};
use crate::hub::{Admission, ConnHandle, Flow, LinkAuth, Outbound};
use crate::ratelimit::TokenBucket;
use axum::extract::State;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use farwing_protocol::realtime::{Envelope, Ping, close, types};
use farwing_protocol::{ErrorBody, ErrorCode, headers};
use futures_util::{SinkExt, StreamExt};
use std::net::IpAddr;
use std::time::Duration;
use tokio::sync::mpsc::unbounded_channel;
use tokio::time::Instant;

enum Start {
    Presence {
        device_id: String,
        auth: LinkAuth,
        epoch: u64,
    },
    Controller(String),
    Resume(String),
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

fn rejected(msg: &str) -> Response {
    ApiError::new(ErrorCode::Unauthorized, msg).into_response()
}

/// Browsers cannot set headers on a WebSocket: they offer the credential as
/// a subprotocol `ticket.<ticket>` or `resume.<token>` next to
/// [`BROWSER_PROTOCOL`], which the server selects.
pub const BROWSER_PROTOCOL: &str = "farwing.v2";

/// A credential offered as a WebSocket subprotocol.
fn offered<'a>(h: &'a HeaderMap, prefix: &str) -> Option<&'a str> {
    h.get_all("sec-websocket-protocol")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(str::trim)
        .find_map(|p| p.strip_prefix(prefix))
        .filter(|v| !v.is_empty())
}

/// `GET /api/v2/ws` with `X-Farwing-Ticket` or `X-Farwing-Resume` (or, from
/// a browser, the same credentials as subprotocols).
pub async fn upgrade(
    State(app): State<App>,
    ClientIp(ip): ClientIp,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let ticket = header(&headers, headers::TICKET).or_else(|| offered(&headers, "ticket."));
    let resume = header(&headers, headers::RESUME).or_else(|| offered(&headers, "resume."));
    let start = if let Some(ticket) = ticket {
        match app.hub.redeem_ticket(ticket) {
            Some(Admission::Presence { device_id, auth }) => {
                if !link_auth_valid(&app, &device_id, &auth).await {
                    return rejected("credentials revoked");
                }
                let id = device_id.clone();
                let epoch = match app.db.call(move |c| db::next_epoch(c, &id)).await {
                    Ok(e) => e.max(0) as u64,
                    Err(e) => return ApiError::from(e).into_response(),
                };
                Start::Presence {
                    device_id,
                    auth,
                    epoch,
                }
            }
            Some(Admission::Controller { session_id }) => Start::Controller(session_id),
            None => return rejected("invalid or expired ticket"),
        }
    } else if let Some(resume) = resume {
        if !app.hub.resume_valid(resume) {
            return rejected("invalid or expired resume token");
        }
        Start::Resume(resume.to_owned())
    } else {
        return rejected("ticket or resume token required");
    };
    let hard_cap = app
        .hub
        .config()
        .limits
        .max_frame_bytes
        .saturating_mul(2)
        .max(64 * 1024);
    ws.protocols([BROWSER_PROTOCOL])
        .max_message_size(hard_cap)
        .max_frame_size(hard_cap)
        .on_upgrade(move |socket| run(app, socket, start, ip))
}

async fn run(app: App, socket: WebSocket, start: Start, ip: IpAddr) {
    let (tx, mut rx) = unbounded_channel::<Outbound>();
    let conn_id = new_uuid();
    let conn = ConnHandle {
        conn_id: conn_id.clone(),
        tx,
        ip,
    };
    let hub = &app.hub;
    let link_id = match start {
        Start::Presence {
            device_id,
            auth,
            epoch,
        } => hub.attach_presence(&device_id, auth, epoch, conn),
        Start::Controller(sid) => hub.attach_controller(&sid, conn),
        Start::Resume(token) => hub.attach_resume(&token, conn),
    };
    let (mut sink, mut stream) = socket.split();
    let Some(link_id) = link_id else {
        // Attach failed; deliver the queued close frame.
        while let Ok(out) = rx.try_recv() {
            if let Outbound::Close(code, why) = out {
                let _ = sink
                    .send(Message::Close(Some(CloseFrame {
                        code,
                        reason: why.into(),
                    })))
                    .await;
                break;
            }
        }
        return;
    };

    let cfg = hub.config().clone();
    let heartbeat = Duration::from_millis(cfg.heartbeat_ms as u64);
    let idle = Duration::from_millis(cfg.idle_timeout_ms as u64);
    let mut ticker = tokio::time::interval_at(Instant::now() + heartbeat, heartbeat);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let idle_sleep = tokio::time::sleep(idle);
    tokio::pin!(idle_sleep);
    let mut bucket = TokenBucket::new(cfg.limits.rate_per_s, app.cfg.limits.ws_rate_burst);
    let max_frame = cfg.limits.max_frame_bytes;

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
                        if text.len() > max_frame {
                            let env = Envelope::error(None, ErrorBody::new(ErrorCode::PayloadTooLarge, "frame too large"));
                            if sink.send(Message::Text(env.encode().into())).await.is_err() {
                                break;
                            }
                            continue;
                        }
                        if let Flow::Close(code, why) = hub.handle_text(&link_id, &conn_id, text.as_str()) {
                            close_with = Some((code, why.into()));
                            break;
                        }
                    }
                    Message::Binary(_) => {
                        close_with = Some((close::PROTOCOL_ERROR, "binary frames are not supported".into()));
                        break;
                    }
                    Message::Ping(_) | Message::Pong(_) => {}
                    Message::Close(_) => break,
                }
            },
            _ = ticker.tick() => {
                let ping = Envelope::new(types::PING, Ping { t: db::now_ms() });
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
        // Complete the closing handshake: read (and discard) until the peer's
        // close frame so unread data does not turn the close into a reset.
        let _ = tokio::time::timeout(Duration::from_secs(2), async {
            while let Some(Ok(msg)) = stream.next().await {
                if matches!(msg, Message::Close(_)) {
                    break;
                }
            }
        })
        .await;
        let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
    }
    hub.connection_closed(&link_id, &conn_id);
}
