//! Multi-viewer forwarding through the API: a host opens a room for the
//! sessions it hosts, their controllers subscribe, others are refused, and
//! rooms follow the sessions and the settings. On an edge node too.

mod common;

use bytes::Bytes;
use common::rtc;
use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn video_packet(ssrc: u32, seq: u16) -> Packet {
    Packet {
        header: Header {
            version: 2,
            payload_type: 98,
            sequence_number: seq,
            timestamp: u32::from(seq) * 3000,
            ssrc,
            ..Default::default()
        },
        payload: Bytes::from_static(b"frame"),
    }
}

struct Room {
    srv: TestServer,
    host: Dev,
    viewers: Vec<(Dev, String)>,
    _links: Vec<Ws>,
}

async fn room_setup(customize: impl FnOnce(&mut farwing_server::Config)) -> Room {
    let srv = start_with(|c| {
        c.sfu.port = free_udp_port();
        c.turn.relay_bind = "127.0.0.1".parse().unwrap();
        customize(c);
    })
    .await;
    srv.create_user("alice");
    let host = srv.account_device("alice", "HOST", true).await;
    let (hw, _) = srv.presence(&host).await;
    let mut links = vec![hw];
    let mut viewers = Vec::new();
    for name in ["C1", "C2"] {
        let ctrl = srv.account_device("alice", name, false).await;
        let (ws, sid, _) = srv.session_link(&ctrl, &host.id, "desktop").await;
        links.push(ws);
        viewers.push((ctrl, sid));
    }
    Room {
        srv,
        host,
        viewers,
        _links: links,
    }
}

/// Subscribes and connects a viewer; returns it with its packets.
async fn subscribe(srv: &TestServer, room: &str, dev: &Dev, sid: &str) -> (rtc::Peer, rtc::Received) {
    let (mut viewer, rx, _) = rtc::viewer(1, 1).await;
    let offer = viewer.offer().await;
    let r = srv
        .post(
            &format!("/api/v2/sfu/rooms/{room}/subscribe"),
            dev,
            json!({"session": sid, "connection": 1, "offer": offer}),
        )
        .await;
    viewer.answer(r.ok()["answer"].as_str().unwrap().to_owned()).await;
    viewer.connected().await;
    (viewer, rx)
}

/// Sends video packets from `seq` on until the viewer receives one: packets
/// sent before both ends finish their handshake are lost.
async fn receives(publisher: &rtc::Peer, ssrc: u32, rx: &mut rtc::Received, seq: u16) {
    for n in 0..50u16 {
        publisher.write(&video_packet(ssrc, seq.wrapping_add(n))).await;
        let got = tokio::time::timeout(Duration::from_millis(200), async {
            loop {
                let (kind, p) = rx.recv().await.unwrap();
                if kind == "media" && p.header.payload_type == 98 {
                    return p.header.sequence_number;
                }
            }
        })
        .await;
        if let Ok(got) = got {
            assert!(got.wrapping_sub(seq) <= n, "{got}");
            return;
        }
    }
    panic!("no forwarded packet");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_host_forwards_to_the_sessions_it_hosts() {
    let Room {
        srv, host, viewers, ..
    } = room_setup(|_| {}).await;
    let info = srv.send(srv.raw(Method::GET, "/api/v2/server")).await;
    assert_eq!(
        info.ok()["features"]["forwarding"],
        json!({"threshold": 3, "modes": ["e2ee", "compatible"]})
    );
    let (c1, sid1) = &viewers[0];
    let (c2, sid2) = &viewers[1];

    let mut publisher = rtc::host().await;
    let offer = publisher.offer().await;
    // Only the host of the sessions opens a room for them.
    srv.post(
        "/api/v2/sfu/rooms",
        c1,
        json!({"sessions": [sid1], "offer": offer}),
    )
    .await
    .err(404, "not_found");
    let r = srv
        .post(
            "/api/v2/sfu/rooms",
            &host,
            json!({"sessions": [sid1, sid2], "mode": "e2ee", "offer": offer}),
        )
        .await;
    let room = r.ok()["room"].as_str().unwrap().to_owned();
    assert_eq!(r.ok()["mode"], "e2ee");
    assert!(r.ok()["node"].is_null());
    publisher
        .answer(r.ok()["answer"].as_str().unwrap().to_owned())
        .await;
    publisher.connected().await;
    let video = farwing_server::sfu::sdp::tracks(&offer)
        .into_iter()
        .find(|t| t.id == "video_0")
        .unwrap();

    // Each controller subscribes with its own session only.
    let offer = rtc::viewer(1, 1).await.0.offer().await;
    srv.post(
        &format!("/api/v2/sfu/rooms/{room}/subscribe"),
        c2,
        json!({"session": sid1, "connection": 1, "offer": offer}),
    )
    .await
    .err(404, "not_found");
    let (_v1, mut rx1) = subscribe(&srv, &room, c1, sid1).await;
    let r = srv
        .post(
            &format!("/api/v2/sfu/rooms/{room}/subscribe"),
            c2,
            json!({"session": sid2, "connection": 1, "offer": rtc::viewer(1, 1).await.0.offer().await}),
        )
        .await;
    assert_eq!(r.ok()["mode"], "e2ee");
    receives(&publisher, video.ssrc, &mut rx1, 7).await;

    let token = srv.admin_token("read");
    let rooms = srv
        .admin_call(Method::GET, "/api/v2/admin/sfu", &token, None)
        .await;
    let listed = &rooms.ok()[0];
    assert_eq!(listed["id"], room.as_str());
    assert_eq!(listed["host_device_id"], host.id.as_str());
    assert_eq!(listed["sessions"].as_array().unwrap().len(), 2);
    assert_eq!(listed["stats"]["legs"].as_array().unwrap().len(), 2);
    let overview = srv
        .admin_call(Method::GET, "/api/v2/admin/overview", &token, None)
        .await;
    assert_eq!(overview.ok()["forwarding_rooms"], 1);
    let sessions = srv
        .admin_call(Method::GET, "/api/v2/admin/sessions", &token, None)
        .await;
    assert!(sessions.ok().as_array().unwrap().iter().all(|s| s["forwarded_by"] == ""));

    // A session taken out of the room loses its subscription.
    srv.put(
        &format!("/api/v2/sfu/rooms/{room}"),
        &host,
        json!({"sessions": [sid2]}),
    )
    .await
    .ok();
    srv.post(
        &format!("/api/v2/sfu/rooms/{room}/subscribe"),
        c1,
        json!({"session": sid1, "connection": 1, "offer": rtc::viewer(1, 1).await.0.offer().await}),
    )
    .await
    .err(404, "not_found");
    let legs = srv.app().forwarding.status()[0]
        .stats
        .as_ref()
        .unwrap()
        .legs
        .len();
    assert_eq!(legs, 1);

    // The room closes with its last session.
    let admin = srv.admin_token("admin");
    srv.admin_call(
        Method::DELETE,
        &format!("/api/v2/admin/sessions/{sid2}"),
        &admin,
        None,
    )
    .await
    .ok();
    let mut closed = false;
    for _ in 0..100 {
        if srv.app().forwarding.status().is_empty() {
            closed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(closed, "the room closed with its sessions");
    assert!(srv.app().forwarding.local.rooms().is_empty());
    srv.delete(&format!("/api/v2/sfu/rooms/{room}"), &host)
        .await
        .err(404, "not_found");
    let _ = publisher.pc.close().await;
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn settings_decide_whether_and_how_hosts_forward() {
    let Room {
        srv, host, viewers, ..
    } = room_setup(|c| c.e2ee = farwing_protocol::api::E2ee::Required).await;
    let info = srv.send(srv.raw(Method::GET, "/api/v2/server")).await;
    assert_eq!(info.ok()["features"]["forwarding"]["modes"], json!(["e2ee"]));
    assert_eq!(info.ok()["features"]["e2ee"], "required");
    let offer = rtc::host().await.offer().await;
    let sid = &viewers[0].1;
    srv.post(
        "/api/v2/sfu/rooms",
        &host,
        json!({"sessions": [sid], "mode": "compatible", "offer": offer}),
    )
    .await
    .err_reason(403, "forbidden", "forwarding_mode_not_allowed");

    let r = srv
        .post(
            "/api/v2/sfu/rooms",
            &host,
            json!({"sessions": [sid], "offer": offer}),
        )
        .await;
    assert_eq!(r.ok()["mode"], "e2ee");
    let token = srv.admin_token("admin");
    // End-to-end encryption off: compatible mode only.
    srv.admin_call(
        Method::PUT,
        "/api/v2/admin/settings",
        &token,
        Some(json!({"e2ee": "off"})),
    )
    .await
    .ok();
    let info = srv.send(srv.raw(Method::GET, "/api/v2/server")).await;
    assert_eq!(info.ok()["features"]["forwarding"]["modes"], json!(["compatible"]));
    let offer = rtc::host().await.offer().await;
    srv.post(
        "/api/v2/sfu/rooms",
        &host,
        json!({"sessions": [sid], "mode": "e2ee", "offer": offer}),
    )
    .await
    .err_reason(403, "forbidden", "forwarding_mode_not_allowed");
    // Allowed: both, end-to-end first.
    srv.admin_call(
        Method::PUT,
        "/api/v2/admin/settings",
        &token,
        Some(json!({"e2ee": "allowed"})),
    )
    .await
    .ok();
    let info = srv.send(srv.raw(Method::GET, "/api/v2/server")).await;
    assert_eq!(
        info.ok()["features"]["forwarding"]["modes"],
        json!(["e2ee", "compatible"])
    );
    // Turning forwarding off closes the rooms.
    srv.admin_call(
        Method::PUT,
        "/api/v2/admin/settings",
        &token,
        Some(json!({"sfu": {"enabled": false}})),
    )
    .await
    .ok();
    assert!(srv.app().forwarding.status().is_empty());
    let info = srv.send(srv.raw(Method::GET, "/api/v2/server")).await;
    assert!(info.ok()["features"].get("forwarding").is_none());
    srv.post(
        "/api/v2/sfu/rooms",
        &host,
        json!({"sessions": [sid], "offer": offer}),
    )
    .await
    .err_reason(403, "forbidden", "forwarding_disabled");
    // The port may not take the relay's.
    let r = srv
        .admin_call(
            Method::PUT,
            "/api/v2/admin/settings",
            &token,
            Some(json!({"sfu": {"enabled": true, "threshold": 1}})),
        )
        .await;
    assert_eq!(r.status, 400, "{}", r.body);
    assert_eq!(r.body["error"]["details"]["field"], "sfu.threshold");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_room_forwards_on_the_node_nearest_to_the_host() {
    let Room {
        srv, host, viewers, ..
    } = room_setup(|_| {}).await;
    let token = srv.admin_token("admin");
    let created = srv
        .admin_call(
            Method::POST,
            "/api/v2/admin/nodes",
            &token,
            Some(json!({"name": "Edge A"})),
        )
        .await;
    let id = created.ok()["node"]["id"].as_str().unwrap().to_owned();
    let link = created.ok()["link"].as_str().unwrap().to_owned();
    srv.admin_call(
        Method::PATCH,
        &format!("/api/v2/admin/nodes/{id}"),
        &token,
        Some(json!({"settings": {
            "public_ip": "127.0.0.1",
            "relay_bind": "127.0.0.1",
            "listen_udp": "127.0.0.1:0",
            "listen_tcp": "127.0.0.1:0",
            "listen_tls": "127.0.0.1:0",
            "stun": [{"listen": "127.0.0.1:0", "public": null}],
            "sfu_port": free_udp_port(),
        }})),
    )
    .await
    .ok();
    let dir = tempfile::tempdir().unwrap();
    let node = tokio::spawn(farwing_server::edge_node::run(dir.path().to_path_buf(), Some(link)));
    let mut ready = false;
    for _ in 0..150 {
        let list = srv
            .admin_call(Method::GET, "/api/v2/admin/nodes", &token, None)
            .await;
        if list.ok()[0]["usable"] == true && list.ok()[0]["health"]["sfu_port"].is_number() {
            ready = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(ready, "the node runs a forwarder");
    // The host measured the node.
    srv.app().edges.record_rtt(
        &host.id,
        std::collections::BTreeMap::from([(id.clone(), Some(12))]),
    );

    let mut publisher = rtc::host().await;
    let offer = publisher.offer().await;
    let (c1, sid1) = &viewers[0];
    let r = srv
        .post(
            "/api/v2/sfu/rooms",
            &host,
            json!({"sessions": [sid1], "offer": offer}),
        )
        .await;
    assert_eq!(r.ok()["node"], "Edge A");
    let room = r.ok()["room"].as_str().unwrap().to_owned();
    publisher
        .answer(r.ok()["answer"].as_str().unwrap().to_owned())
        .await;
    publisher.connected().await;
    let (_v1, mut rx1) = subscribe(&srv, &room, c1, sid1).await;
    let video = farwing_server::sfu::sdp::tracks(&offer)
        .into_iter()
        .find(|t| t.id == "video_0")
        .unwrap();
    receives(&publisher, video.ssrc, &mut rx1, 3).await;
    assert!(srv.app().forwarding.local.rooms().is_empty(), "not forwarded here");
    let status: Value = serde_json::to_value(srv.app().forwarding.status()).unwrap();
    assert_eq!(status[0]["node"], "Edge A");

    srv.delete(&format!("/api/v2/sfu/rooms/{room}"), &host)
        .await
        .ok();
    let _ = publisher.pc.close().await;
    node.abort();
    srv.shutdown().await;
}
