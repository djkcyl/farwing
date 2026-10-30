//! Multi-viewer forwarding, control side (`docs/protocol/deployment-design.md`
//! §5): the rooms hosts open, the sessions whose controllers may subscribe,
//! and where each room's forwarder runs — the edge node nearest to the host
//! by its measurement, else this server.

use crate::config::Config;
use crate::edge::EdgeHub;
use crate::hub::Hub;
use crate::sfu::{RoomStats, Sfu, SfuConfig, SfuRequest};
use farwing_protocol::api::ForwardMode;
use parking_lot::Mutex;
use serde::Serialize;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::sync::Arc;

/// Room IDs carry this prefix.
const ROOM_PREFIX: &str = "fwr_";

#[derive(Debug, thiserror::Error)]
pub enum ForwardError {
    #[error("forwarding is off")]
    Disabled,
    #[error("compatible forwarding is not allowed")]
    ModeNotAllowed,
    /// The room or session is unknown, or not this device's.
    #[error("not found")]
    NotFound,
    #[error("the forwarder failed: {0}")]
    Unavailable(String),
}

#[derive(Clone, Debug, Serialize)]
pub struct RoomRecord {
    pub id: String,
    pub host_device_id: String,
    /// Edge node forwarding (ID, name); none: this server.
    pub node: Option<(String, String)>,
    pub sessions: BTreeSet<String>,
    pub mode: ForwardMode,
    pub created_at: i64,
}

/// A room as the administration API shows it.
#[derive(Clone, Debug, Serialize)]
pub struct RoomStatus {
    pub id: String,
    pub host_device_id: String,
    /// Name of the edge node forwarding; none: this server.
    pub node: Option<String>,
    pub mode: ForwardMode,
    pub sessions: Vec<String>,
    pub created_at: i64,
    pub stats: Option<RoomStats>,
}

pub struct Forwarding {
    pub local: Arc<Sfu>,
    edges: Arc<EdgeHub>,
    rooms: Mutex<HashMap<String, RoomRecord>>,
}

pub fn sfu_config(cfg: &Config) -> SfuConfig {
    SfuConfig::new(cfg.sfu.port, cfg.turn.relay_bind, cfg.turn.builtin_public_ip())
}

impl Forwarding {
    pub fn new(cfg: &Config, edges: Arc<EdgeHub>, meters: Arc<crate::egress::Meters>) -> Arc<Self> {
        Arc::new(Self {
            local: Sfu::new(sfu_config(cfg), meters),
            edges,
            rooms: Mutex::default(),
        })
    }

    /// Settings changed: new connections use the new port and address;
    /// turning forwarding off closes every room (hosts send directly again).
    pub async fn reconfigure(&self, cfg: &Config) {
        self.local.set_config(sfu_config(cfg));
        if !cfg.sfu.enabled {
            let ids: Vec<String> = self.rooms.lock().keys().cloned().collect();
            for id in ids {
                self.close(None, &id).await;
            }
        }
    }

    async fn run(
        &self,
        node: Option<&(String, String)>,
        request: SfuRequest,
    ) -> Result<String, ForwardError> {
        match node {
            Some((id, _)) => self
                .edges
                .sfu(id, request)
                .await
                .map_err(ForwardError::Unavailable),
            None => self
                .local
                .handle(request)
                .await
                .map_err(|e| ForwardError::Unavailable(e.to_string())),
        }
    }

    /// Sessions `host` hosts now, among `sessions`; `NotFound` if any is not.
    fn hosted(hub: &Hub, host: &str, sessions: &[String]) -> Result<BTreeSet<String>, ForwardError> {
        sessions
            .iter()
            .map(|sid| {
                hub.session(sid)
                    .filter(|s| s.host_device_id == host)
                    .map(|_| sid.clone())
                    .ok_or(ForwardError::NotFound)
            })
            .collect()
    }

    /// Opens a room of `host` for `sessions`; returns it and the answer.
    pub async fn create(
        &self,
        cfg: &Config,
        hub: &Hub,
        host: &str,
        sessions: &[String],
        mode: ForwardMode,
        offer: &str,
    ) -> Result<(RoomRecord, String), ForwardError> {
        if !cfg.sfu.enabled {
            return Err(ForwardError::Disabled);
        }
        if mode == ForwardMode::Compatible && !cfg.sfu.allow_compatible {
            return Err(ForwardError::ModeNotAllowed);
        }
        let sessions = Self::hosted(hub, host, sessions)?;
        let id = format!(
            "{ROOM_PREFIX}{}",
            uuid::Uuid::new_v4().simple().to_string().get(..20).unwrap_or_default()
        );
        let node = self.edges.forwarder_for(host);
        let answer = self
            .run(
                node.as_ref(),
                SfuRequest::Publish {
                    room: id.clone(),
                    sdp: offer.to_owned(),
                },
            )
            .await?;
        let record = RoomRecord {
            id: id.clone(),
            host_device_id: host.to_owned(),
            node,
            sessions,
            mode,
            created_at: crate::db::now_s(),
        };
        self.rooms.lock().insert(id, record.clone());
        tracing::info!(
            room = %record.id,
            host = %host,
            node = ?record.node.as_ref().map(|n| &n.1),
            sessions = record.sessions.len(),
            "forwarding room opened"
        );
        Ok((record, answer))
    }

    fn room_of(&self, host: Option<&str>, room: &str) -> Result<RoomRecord, ForwardError> {
        self.rooms
            .lock()
            .get(room)
            .filter(|r| host.is_none_or(|h| r.host_device_id == h))
            .cloned()
            .ok_or(ForwardError::NotFound)
    }

    /// Changes the sessions that may subscribe and, with an offer, the tracks.
    pub async fn update(
        &self,
        hub: &Hub,
        host: &str,
        room: &str,
        sessions: Option<&[String]>,
        offer: Option<&str>,
    ) -> Result<Option<String>, ForwardError> {
        let record = self.room_of(Some(host), room)?;
        if let Some(sessions) = sessions {
            let sessions = Self::hosted(hub, host, sessions)?;
            for gone in record.sessions.difference(&sessions) {
                let _ = self
                    .run(
                        record.node.as_ref(),
                        SfuRequest::Unsubscribe {
                            room: room.to_owned(),
                            leg: gone.clone(),
                        },
                    )
                    .await;
            }
            if let Some(r) = self.rooms.lock().get_mut(room) {
                r.sessions = sessions;
            }
        }
        match offer {
            Some(offer) => self
                .run(
                    record.node.as_ref(),
                    SfuRequest::Publish {
                        room: room.to_owned(),
                        sdp: offer.to_owned(),
                    },
                )
                .await
                .map(Some),
            None => Ok(None),
        }
    }

    /// Closes `room` (of `host`, when given).
    pub async fn close(&self, host: Option<&str>, room: &str) -> bool {
        let Ok(record) = self.room_of(host, room) else {
            return false;
        };
        self.rooms.lock().remove(room);
        let _ = self
            .run(
                record.node.as_ref(),
                SfuRequest::Close {
                    room: room.to_owned(),
                },
            )
            .await;
        tracing::info!(room = %room, "forwarding room closed");
        true
    }

    /// Subscribes the controller `device` of `session` to `room`.
    pub async fn subscribe(
        &self,
        hub: &Hub,
        device: &str,
        room: &str,
        session: &str,
        connection: u32,
        offer: &str,
        tracks: Option<Vec<String>>,
        layer: farwing_protocol::api::ForwardLayer,
    ) -> Result<(String, ForwardMode), ForwardError> {
        let record = self.room_of(None, room)?;
        let view = hub.session(session);
        let controls = view.as_ref().is_some_and(|s| s.controller_device_id == device);
        if !controls || !record.sessions.contains(session) {
            return Err(ForwardError::NotFound);
        }
        // The frame key opens the audio too: only a participant granted
        // `audio` receives it from the forwarder.
        let audio = view
            .as_ref()
            .is_some_and(|s| s.grants.iter().any(|c| c == farwing_protocol::caps::AUDIO));
        if !audio && crate::sfu::sdp::media_sections(offer).contains(&crate::sfu::sdp::Kind::Audio) {
            return Err(ForwardError::NotFound);
        }
        let answer = self
            .run(
                record.node.as_ref(),
                SfuRequest::Subscribe {
                    room: room.to_owned(),
                    leg: crate::sfu::leg_id(session, connection),
                    sdp: offer.to_owned(),
                    tracks,
                    layer,
                },
            )
            .await?;
        Ok((answer, record.mode))
    }

    /// The stream the controller `device` of `session` chose in `room`.
    pub async fn choose_layer(
        &self,
        hub: &Hub,
        device: &str,
        room: &str,
        session: &str,
        layer: farwing_protocol::api::ForwardLayer,
    ) -> Result<(), ForwardError> {
        let record = self.room_of(None, room)?;
        let controls = hub
            .session(session)
            .is_some_and(|s| s.controller_device_id == device);
        if !controls || !record.sessions.contains(session) {
            return Err(ForwardError::NotFound);
        }
        self.run(
            record.node.as_ref(),
            SfuRequest::Layer {
                room: room.to_owned(),
                session: session.to_owned(),
                layer,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn unsubscribe(
        &self,
        hub: &Hub,
        device: &str,
        room: &str,
        session: &str,
        connection: u32,
    ) -> bool {
        let Ok(record) = self.room_of(None, room) else {
            return false;
        };
        let allowed = record.host_device_id == device
            || hub
                .session(session)
                .is_some_and(|s| s.controller_device_id == device);
        if !allowed || !record.sessions.contains(session) {
            return false;
        }
        let _ = self
            .run(
                record.node.as_ref(),
                SfuRequest::Unsubscribe {
                    room: room.to_owned(),
                    leg: crate::sfu::leg_id(session, connection),
                },
            )
            .await;
        true
    }

    /// Drops ended sessions; closes rooms none is left in.
    pub async fn sweep(&self, hub: &Hub) {
        let mut empty = Vec::new();
        let mut ended = Vec::new();
        {
            let mut rooms = self.rooms.lock();
            for room in rooms.values_mut() {
                let gone: Vec<String> = room
                    .sessions
                    .iter()
                    .filter(|sid| hub.session(sid).is_none())
                    .cloned()
                    .collect();
                for sid in gone {
                    room.sessions.remove(&sid);
                    ended.push((room.id.clone(), room.node.clone(), sid));
                }
                if room.sessions.is_empty() {
                    empty.push(room.id.clone());
                }
            }
        }
        for (room, node, leg) in ended {
            let _ = self
                .run(node.as_ref(), SfuRequest::Unsubscribe { room, leg })
                .await;
        }
        for room in empty {
            self.close(None, &room).await;
        }
    }

    /// Sessions whose controllers may receive through a forwarder.
    pub fn forwarded_sessions(&self) -> HashMap<String, String> {
        let mut out = HashMap::new();
        for room in self.rooms.lock().values() {
            let node = room.node.as_ref().map(|n| n.1.clone()).unwrap_or_default();
            for sid in &room.sessions {
                out.insert(sid.clone(), node.clone());
            }
        }
        out
    }

    pub fn status(&self) -> Vec<RoomStatus> {
        let mut stats: HashMap<String, RoomStats> = self
            .local
            .rooms()
            .into_iter()
            .map(|r| (r.id.clone(), r))
            .collect();
        stats.extend(
            self.edges
                .forwarded_rooms()
                .into_iter()
                .map(|(_, r)| (r.id.clone(), r)),
        );
        let mut out: Vec<RoomStatus> = self
            .rooms
            .lock()
            .values()
            .map(|r| RoomStatus {
                id: r.id.clone(),
                host_device_id: r.host_device_id.clone(),
                node: r.node.as_ref().map(|n| n.1.clone()),
                mode: r.mode,
                sessions: r.sessions.iter().cloned().collect(),
                created_at: r.created_at,
                stats: stats.remove(&r.id),
            })
            .collect();
        out.sort_by(|a, b| (a.created_at, &a.id).cmp(&(b.created_at, &b.id)));
        out
    }

    /// Hosts with a room.
    pub fn hosts(&self) -> HashSet<String> {
        self.rooms
            .lock()
            .values()
            .map(|r| r.host_device_id.clone())
            .collect()
    }
}
