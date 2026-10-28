//! Realtime plane: presence, heartbeat, resume, sessions, relay and multiple
//! participants per host.

mod common;

use common::*;
use farwing_protocol::headers;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

struct Pair {
    srv: TestServer,
    host: Dev,
    ctrl: Dev,
    host_ws: Ws,
}

async fn pair_with(customize: impl FnOnce(&mut farwing_server::Config)) -> Pair {
    let srv = start_with(customize).await;
    srv.create_user("alice");
    let host = srv.account_device("alice", "HOST", true).await;
    let ctrl = srv.account_device("alice", "CTRL", false).await;
    let (host_ws, _) = srv.presence(&host).await;
    Pair {
        srv,
        host,
        ctrl,
        host_ws,
    }
}

/// Create a session, connect the controller and run `session.start`.
async fn open_session(p: &mut Pair, ctrl: &Dev, attempt: &str) -> (Ws, String, Value) {
    let (mut ws, sid, v) = p.srv.session_link(ctrl, &p.host.id, "desktop").await;
    assert_eq!(v["target"]["device_id"], p.host.id.as_str());
    assert_eq!(v["target"]["capabilities"][0], "host");
    assert_eq!(v["target"]["relation"], "own");
    assert_eq!(v["target"]["public_key"], p.host.public_key().as_str());
    ws.send(start_frame("s1", &sid, attempt)).await;
    let (incoming, before) = p.host_ws.recv_until("session.incoming").await;
    assert!(
        before
            .iter()
            .all(|v| v["type"] == "session.ended"
                || v["type"] == "device.changed"
                || v["type"] == "notice"),
        "{before:?}"
    );
    assert_eq!(incoming["sid"], sid.as_str());
    let ack = ws.recv_type("ack").await;
    assert_eq!(ack["re"], "s1");
    let mut incoming = incoming;
    incoming["_before"] = Value::Array(before);
    incoming["_ack"] = ack;
    (ws, sid, incoming)
}

#[tokio::test(flavor = "multi_thread")]
async fn presence_hello_heartbeat_and_resume() {
    let srv = start_with(|c| {
        c.limits.heartbeat_ms = 200;
        c.limits.idle_timeout_ms = 5000;
    })
    .await;
    srv.create_user("alice");
    let dev = srv.account_device("alice", "HOST", true).await;
    let (mut ws, hello) = srv.presence(&dev).await;
    let d = &hello["data"];
    assert_eq!(hello["v"], V);
    assert_eq!(d["epoch"], 1);
    assert_eq!(d["resumed"], false);
    assert_eq!(d["heartbeat_ms"], 200);
    assert_eq!(d["idle_timeout_ms"], 5000);
    assert!(d.get("session_id").is_none());
    let token1 = d["resume_token"].as_str().unwrap().to_owned();
    assert!(farwing_protocol::ids::is_token(&token1, "fwr_"));
    srv.wait_online(&dev.id, true).await;

    // Server ping, client pong.
    let ping = loop {
        let v = ws.next(Duration::from_secs(2)).await.unwrap().unwrap();
        if v["type"] == "ping" {
            break v;
        }
    };
    ws.send(json!({"v": V, "type": "pong", "data": {"t": ping["data"]["t"]}}))
        .await;
    // Client ping, server pong with the same value.
    ws.send(json!({"v": V, "type": "ping", "data": {"t": 42}}))
        .await;
    let pong = ws.recv_type("pong").await;
    assert_eq!(pong["data"]["t"], 42);

    // Token refresh.
    ws.send(json!({"v": V, "type": "resume.refresh", "id": "r1"}))
        .await;
    let ack = ws.recv_type("ack").await;
    assert_eq!(ack["re"], "r1");
    let token2 = ack["data"]["resume_token"].as_str().unwrap().to_owned();
    assert_ne!(token1, token2);

    // Unknown types get an error reply.
    ws.send(json!({"v": V, "type": "nope", "id": "x1"})).await;
    let e = ws.recv_type("error").await;
    assert_eq!(e["re"], "x1");
    assert_eq!(e["data"]["code"], "bad_request");

    // Drop the connection: offline, but resumable with the new token only.
    ws.abort();
    srv.wait_online(&dev.id, false).await;
    let err = srv
        .ws_connect(headers::RESUME, &token1, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 401));
    let mut ws = srv
        .ws_connect(headers::RESUME, &token2, None)
        .await
        .unwrap();
    let hello = ws.recv_type("hello").await;
    assert_eq!(hello["data"]["resumed"], true);
    assert_eq!(hello["data"]["epoch"], 1);
    srv.wait_online(&dev.id, true).await;
    let list = srv.get("/api/v2/devices", &dev).await.ok().clone();
    assert_eq!(list["current_device"]["status"], "online");
    assert!(list["current_device"]["last_seen_at"].as_i64().unwrap() > 0);

    // A fresh presence connection supersedes the old one and bumps the epoch.
    let (_ws2, hello2) = srv.presence(&dev).await;
    assert_eq!(hello2["data"]["epoch"], 2);
    assert_eq!(ws.expect_close(&[]).await, Some(4001));

    // Tickets are single use.
    let t = srv.post_empty("/api/v2/presence/ticket", &dev).await.ok()["ticket"]
        .as_str()
        .unwrap()
        .to_owned();
    let _w = srv.ws_connect(headers::TICKET, &t, None).await.unwrap();
    assert!(srv.ws_connect(headers::TICKET, &t, None).await.is_err());

    // bye ends the presence cleanly.
    let (mut w3, _) = srv.presence(&dev).await;
    w3.send(json!({"v": V, "type": "bye"})).await;
    assert_eq!(w3.expect_close(&[]).await, Some(1000));
    srv.wait_online(&dev.id, false).await;
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn idle_timeout_and_rate_limit() {
    let srv = start_with(|c| {
        c.limits.heartbeat_ms = 60_000;
        c.limits.idle_timeout_ms = 400;
        c.limits.ws_rate_per_s = 5;
        c.limits.ws_rate_burst = 5;
    })
    .await;
    let g = srv.guest_device("G").await;
    let (mut ws, _) = srv.presence(&g).await;
    assert_eq!(ws.expect_close(&[]).await, Some(1001));

    let (mut ws, _) = srv.presence(&g).await;
    for i in 0..20 {
        if !ws
            .try_send(json!({"v": V, "type": "ping", "data": {"t": i}}))
            .await
        {
            break;
        }
    }
    assert_eq!(ws.expect_close(&["pong"]).await, Some(4029));
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn full_session_flow_relay_and_endings() {
    let mut p = pair_with(|_| {}).await;
    let ctrl = p.ctrl.clone();
    let (mut cw, sid, incoming) = open_session(&mut p, &ctrl, "att-1").await;
    let d = &incoming["data"];
    assert_eq!(d["attempt_id"], "att-1");
    assert_eq!(d["purpose"], "desktop");
    assert_eq!(d["relation"], "own");
    assert_eq!(d["grants"], json!(farwing_protocol::caps::ALL));
    assert_eq!(d["controller"]["device_id"], ctrl.id.as_str());
    assert_eq!(d["controller"]["username"], "alice");
    assert_eq!(d["controller"]["nickname"], "alice-nick");
    assert_eq!(d["controller"]["alias"], "CTRL");
    assert_eq!(d["controller"]["platform"], "windows");
    assert_eq!(d["controller"]["public_key"], ctrl.public_key().as_str());
    assert_eq!(d["options"]["decoders"][0]["codec"], "hevc");
    assert_eq!(d["force_relay"], false);
    assert!(d.get("network").is_none());
    let ice = d["ice_servers"].as_array().unwrap();
    assert_eq!(ice.len(), 2);
    assert!(
        ice[1]["username"]
            .as_str()
            .unwrap()
            .ends_with(&format!(":{sid}"))
    );
    let participant = d["participant_id"].as_str().unwrap().to_owned();
    // The controller learns the host's identity from the server.
    let ack = &incoming["_ack"]["data"];
    assert_eq!(ack["participant_id"], participant.as_str());
    assert_eq!(ack["host"]["device_id"], p.host.id.as_str());
    assert_eq!(ack["host"]["public_key"], p.host.public_key().as_str());
    assert_eq!(ack["relation"], "own");
    assert_eq!(ack["grants"], json!(farwing_protocol::caps::ALL));

    // Participants are visible in the device list.
    let list = p.srv.get("/api/v2/devices", &ctrl).await.ok().clone();
    let host_entry = &list["own"][0];
    let part = &host_entry["participants"][0];
    assert_eq!(part["session_id"], sid.as_str());
    assert_eq!(part["participant_id"], participant.as_str());
    assert_eq!(part["device_id"], ctrl.id.as_str());
    assert_eq!(part["device_alias"], "CTRL");
    assert_eq!(part["nickname"], "alice-nick");
    assert_eq!(part["relation"], "own");
    assert_eq!(part["purpose"], "desktop");

    // Offer C -> H with ack.
    cw.send(
        json!({"v": V, "type": "rtc.offer", "id": "o1", "sid": sid, "gen": 1,
        "data": {"attempt_id": "att-1", "restart": false, "network": 3, "sdp": "v=0 offer"}}),
    )
    .await;
    let offer = p.host_ws.recv_type("rtc.offer").await;
    assert_eq!(offer["sid"], sid.as_str());
    assert_eq!(offer["gen"], 1);
    assert_eq!(offer["data"]["sdp"], "v=0 offer");
    assert!(offer.get("id").is_none());
    assert_eq!(cw.recv_type("ack").await["re"], "o1");

    // Answer H -> C.
    p.host_ws
        .send(json!({"v": V, "type": "rtc.answer", "sid": sid, "gen": 1,
            "data": {"attempt_id": "att-1", "restart": false, "network": 3, "sdp": "v=0 answer"}}))
        .await;
    let answer = cw.recv_type("rtc.answer").await;
    assert_eq!(answer["data"]["sdp"], "v=0 answer");
    assert_eq!(answer["gen"], 1);

    // Candidates both ways.
    let cand = json!({"candidate": "candidate:1 1 udp 2122260223 192.0.2.1 50000 typ host", "sdpMid": "0", "sdpMLineIndex": 0});
    cw.send(json!({"v": V, "type": "rtc.candidate", "sid": sid, "gen": 1, "data": {"attempt_id": "att-1", "candidate": cand}}))
        .await;
    assert_eq!(
        p.host_ws.recv_type("rtc.candidate").await["data"]["candidate"]["sdpMid"],
        "0"
    );
    p.host_ws
        .send(json!({"v": V, "type": "rtc.candidate", "sid": sid, "gen": 1, "data": {"attempt_id": "att-1", "candidate": cand}}))
        .await;
    assert_eq!(
        cw.recv_type("rtc.candidate").await["data"]["candidate"]["sdpMLineIndex"],
        0
    );

    // Validation: stale attempt, wrong direction, wrong session, host-side start.
    cw.send(
        json!({"v": V, "type": "rtc.offer", "id": "o2", "sid": sid, "gen": 2,
        "data": {"attempt_id": "att-0", "sdp": "x"}}),
    )
    .await;
    let e = cw.recv_type("error").await;
    assert_eq!(
        (e["re"].as_str(), e["data"]["code"].as_str()),
        (Some("o2"), Some("conflict"))
    );
    cw.send(json!({"v": V, "type": "rtc.answer", "id": "o3", "sid": sid, "data": {"attempt_id": "att-1", "sdp": "x"}}))
        .await;
    assert_eq!(cw.recv_type("error").await["data"]["code"], "forbidden");
    cw.send(json!({"v": V, "type": "rtc.offer", "id": "o4", "sid": "00000000-0000-0000-0000-000000000000", "data": {"attempt_id": "att-1", "sdp": "x"}}))
        .await;
    assert_eq!(cw.recv_type("error").await["data"]["code"], "not_found");
    p.host_ws.send(start_frame("h1", &sid, "a")).await;
    assert_eq!(
        p.host_ws.recv_type("error").await["data"]["code"],
        "forbidden"
    );

    // A new attempt (session rebuild) re-announces the session to the host;
    // purpose and relation still come from the session, not the frame.
    let mut restart = start_frame("s2", &sid, "att-2");
    restart["data"]["purpose"] = json!("files");
    cw.send(restart).await;
    let again = p.host_ws.recv_type("session.incoming").await;
    assert_eq!(again["data"]["attempt_id"], "att-2");
    assert_eq!(again["data"]["participant_id"], participant.as_str());
    assert_eq!(again["data"]["purpose"], "desktop");
    let ack = cw.recv_type("ack").await;
    assert_eq!(ack["data"]["participant_id"], participant.as_str());
    assert_eq!(ack["data"]["session_id"], sid.as_str());

    // Host ends the session; host-stated reasons are relayed.
    p.host_ws
        .send(json!({"v": V, "type": "session.end", "id": "e1", "sid": sid, "data": {"reason": "kicked"}}))
        .await;
    assert_eq!(p.host_ws.recv_type("ack").await["re"], "e1");
    let ended = cw.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid.as_str());
    assert_eq!(ended["data"]["reason"], "kicked");
    assert_eq!(cw.expect_close(&[]).await, Some(4004));
    // Ended sessions cannot be re-ticketed.
    p.srv
        .post_empty(&format!("/api/v2/sessions/{sid}/ticket"), &ctrl)
        .await
        .err(404, "not_found");

    // Unknown host reasons are reported as host_closed.
    let (mut cw, sid, _) = open_session(&mut p, &ctrl, "att-x").await;
    p.host_ws
        .send(json!({"v": V, "type": "session.end", "sid": sid, "data": {"reason": "whatever"}}))
        .await;
    assert_eq!(
        cw.recv_type("session.ended").await["data"]["reason"],
        "host_closed"
    );

    // Controller bye.
    let (mut cw, sid2, _) = open_session(&mut p, &ctrl, "att-3").await;
    cw.send(json!({"v": V, "type": "bye"})).await;
    assert_eq!(cw.expect_close(&[]).await, Some(1000));
    let ended = p.host_ws.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid2.as_str());
    assert_eq!(ended["data"]["reason"], "controller_left");
    let list = p.srv.get("/api/v2/devices", &ctrl).await.ok().clone();
    assert_eq!(list["own"][0]["participants"], json!([]));

    // Persistence mirror for the admin CLI.
    tokio::time::sleep(Duration::from_millis(100)).await;
    let open = p
        .srv
        .app()
        .db
        .call(|c| farwing_server::db::list_open_sessions(c))
        .await
        .unwrap();
    assert!(open.is_empty());
    p.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_start_requires_online_host_and_controller_connection() {
    let mut p = pair_with(|c| c.limits.resume_ttl_s = 60).await;
    let ctrl = p.ctrl.clone();
    // Target offline at POST time.
    let lone = p.srv.account_device("alice", "OFF", true).await;
    p.srv
        .post(
            "/api/v2/sessions",
            &ctrl,
            json!({"target_device_id": lone.id, "purpose": "desktop"}),
        )
        .await
        .err(409, "device_offline");
    // No host implementation.
    let nc = p.srv.account_device("alice", "NC", false).await;
    let (_nw, _) = p.srv.presence(&nc).await;
    p.srv
        .post(
            "/api/v2/sessions",
            &ctrl,
            json!({"target_device_id": nc.id, "purpose": "files"}),
        )
        .await
        .err(409, "host_disabled");
    // `control` admits sessions at all; files and ports need their own switch.
    p.srv
        .set_policy(
            &p.host,
            json!({"control": false, "files": true, "ports": true, "max_participants": 8}),
        )
        .await;
    for purpose in ["desktop", "audio", "files", "ports"] {
        p.srv
            .post(
                "/api/v2/sessions",
                &ctrl,
                json!({"target_device_id": p.host.id, "purpose": purpose}),
            )
            .await
            .err(409, "host_disabled");
    }
    p.srv
        .set_policy(
            &p.host,
            json!({"control": true, "files": true, "ports": false, "max_participants": 8}),
        )
        .await;
    p.srv
        .post(
            "/api/v2/sessions",
            &ctrl,
            json!({"target_device_id": p.host.id, "purpose": "ports"}),
        )
        .await
        .err(409, "host_disabled");
    let (fw, _, _) = p.srv.session_link(&ctrl, &p.host.id, "files").await;
    fw.close().await;
    p.srv.set_policy(&p.host, host_policy(true)).await;
    // Another account's device cannot open a session to this host, even
    // when the host is online and controllable.
    p.srv.create_user("mallory");
    let other = p.srv.account_device("mallory", "OTHER", true).await;
    p.srv
        .post(
            "/api/v2/sessions",
            &other,
            json!({"target_device_id": p.host.id, "purpose": "desktop"}),
        )
        .await
        .err(404, "device_not_found");
    // Self.
    p.srv
        .post(
            "/api/v2/sessions",
            &p.host,
            json!({"target_device_id": p.host.id, "purpose": "desktop"}),
        )
        .await
        .err(400, "bad_request");
    p.srv
        .post(
            "/api/v2/sessions",
            &ctrl,
            json!({"target_device_id": p.host.id, "purpose": "bogus"}),
        )
        .await
        .err(400, "bad_request");

    // Host drops (resume window) between POST and session.start.
    let (mut cw, sid, _) = p.srv.session_link(&ctrl, &p.host.id, "desktop").await;
    let host_ws = std::mem::replace(&mut p.host_ws, {
        // placeholder: the guest's presence stands in for the host's
        let g = p.srv.guest_device("TMP").await;
        p.srv.presence(&g).await.0
    });
    host_ws.abort();
    p.srv.wait_online(&p.host.id, false).await;
    cw.send(start_frame("s1", &sid, "a1")).await;
    let e = cw.recv_type("error").await;
    assert_eq!(
        (e["re"].as_str(), e["data"]["code"].as_str()),
        (Some("s1"), Some("device_offline"))
    );
    // session.start without id.
    let mut start = start_frame("x", &sid, "a3");
    start.as_object_mut().unwrap().remove("id");
    cw.send(start).await;
    assert_eq!(cw.recv_type("error").await["data"]["code"], "bad_request");
    p.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn multiple_participants_replacement_and_limit() {
    let mut p = pair_with(|_| {}).await;
    let c1 = p.ctrl.clone();
    let c2 = p.srv.account_device("alice", "CTRL-2", false).await;
    let c3 = p.srv.account_device("alice", "CTRL-3", false).await;
    let (mut w1, sid1, _) = open_session(&mut p, &c1, "a1").await;

    // A second controller joins without displacing the first.
    let (mut w2, sid2, incoming2) = open_session(&mut p, &c2, "b1").await;
    assert!(
        incoming2["_before"]
            .as_array()
            .unwrap()
            .iter()
            .all(|v| v["type"] != "session.ended"),
        "{incoming2}"
    );
    assert_ne!(sid1, sid2);
    w1.send(json!({"v": V, "type": "ping", "data": {"t": 1}}))
        .await;
    assert_eq!(w1.recv_type("pong").await["data"]["t"], 1);
    let list = p.srv.get("/api/v2/devices", &c1).await.ok().clone();
    let host = list["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == p.host.id.as_str())
        .unwrap()
        .clone();
    let sids: Vec<&str> = host["participants"]
        .as_array()
        .unwrap()
        .iter()
        .map(|x| x["session_id"].as_str().unwrap())
        .collect();
    assert_eq!(sids.len(), 2);
    assert!(sids.contains(&sid1.as_str()) && sids.contains(&sid2.as_str()));

    // The same controller opening a new session replaces only its own.
    let (_w3, sid3, incoming3) = open_session(&mut p, &c2, "b2").await;
    let before = incoming3["_before"].as_array().unwrap();
    assert!(
        before.iter().any(|v| v["type"] == "session.ended"
            && v["sid"] == sid2.as_str()
            && v["data"]["reason"] == "replaced"),
        "{before:?}"
    );
    assert!(before.iter().all(|v| v["sid"] != sid1.as_str()));
    let ended = w2.recv_type("session.ended").await;
    assert_eq!(ended["data"]["reason"], "replaced");
    assert_eq!(ended["sid"], sid2.as_str());
    assert_eq!(w2.expect_close(&[]).await, Some(4004));
    assert_ne!(sid2, sid3);
    w1.send(json!({"v": V, "type": "ping", "data": {"t": 2}}))
        .await;
    assert_eq!(w1.recv_type("pong").await["data"]["t"], 2);

    // The host's participant limit: a third controller is busy, nobody is
    // displaced; the same controllers can still replace their own sessions.
    let mut policy = host_policy(true);
    policy["max_participants"] = json!(2);
    p.srv.set_policy(&p.host, policy).await;
    p.srv
        .post(
            "/api/v2/sessions",
            &c3,
            json!({"target_device_id": p.host.id, "purpose": "desktop"}),
        )
        .await
        .err(409, "busy");
    w1.send(json!({"v": V, "type": "ping", "data": {"t": 3}}))
        .await;
    assert_eq!(w1.recv_type("pong").await["data"]["t"], 3);
    let (_w4, _sid4, _) = open_session(&mut p, &c1, "a2").await;
    assert_eq!(w1.recv_type("session.ended").await["data"]["reason"], "replaced");
    // `force` is gone: it is ignored, still busy.
    p.srv
        .post(
            "/api/v2/sessions",
            &c3,
            json!({"target_device_id": p.host.id, "purpose": "desktop", "force": true}),
        )
        .await
        .err(409, "busy");
    p.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn controller_resume_and_resume_window_expiry() {
    let mut p = pair_with(|c| c.limits.resume_ttl_s = 1).await;
    let ctrl = p.ctrl.clone();
    let (mut cw, sid, _) = open_session(&mut p, &ctrl, "a1").await;

    // Refresh the token, drop, resume: the session continues.
    cw.send(json!({"v": V, "type": "resume.refresh", "id": "r"}))
        .await;
    let token = cw.recv_type("ack").await["data"]["resume_token"]
        .as_str()
        .unwrap()
        .to_owned();
    cw.abort();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let mut cw = p
        .srv
        .ws_connect(headers::RESUME, &token, None)
        .await
        .unwrap();
    let hello = cw.recv_type("hello").await;
    assert_eq!(hello["data"]["resumed"], true);
    assert_eq!(hello["data"]["session_id"], sid.as_str());
    cw.send(json!({"v": V, "type": "rtc.offer", "sid": sid, "gen": 1, "data": {"attempt_id": "a1", "sdp": "o"}}))
        .await;
    p.host_ws.recv_type("rtc.offer").await;

    // Disconnect without bye: after the resume window the host is told.
    cw.abort();
    let ended = p.host_ws.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid.as_str());
    assert_eq!(ended["data"]["reason"], "controller_left");
    let err = p
        .srv
        .ws_connect(headers::RESUME, &token, None)
        .await
        .err()
        .unwrap();
    assert!(matches!(err, tokio_tungstenite::tungstenite::Error::Http(ref r) if r.status() == 401));
    p.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn host_offline_and_new_epoch_end_sessions() {
    let mut p = pair_with(|c| c.limits.resume_ttl_s = 1).await;
    let ctrl = p.ctrl.clone();

    // A fresh presence connection (new epoch) ends sessions of the old epoch.
    let (mut cw, sid, _) = open_session(&mut p, &ctrl, "a1").await;
    let (new_ws, hello) = p.srv.presence(&p.host).await;
    assert_eq!(hello["data"]["epoch"], 2);
    let old_ws = std::mem::replace(&mut p.host_ws, new_ws);
    let mut old_ws = old_ws;
    assert_eq!(old_ws.expect_close(&[]).await, Some(4001));
    let ended = cw.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid.as_str());
    assert_eq!(ended["data"]["reason"], "host_offline");
    assert_eq!(cw.expect_close(&[]).await, Some(4004));

    // Host presence lost beyond the resume window.
    let (mut cw, sid, _) = open_session(&mut p, &ctrl, "a2").await;
    let host_ws = std::mem::replace(&mut p.host_ws, {
        let g = p.srv.guest_device("TMP").await;
        p.srv.presence(&g).await.0
    });
    host_ws.abort();
    let ended = cw.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid.as_str());
    assert_eq!(ended["data"]["reason"], "host_offline");
    assert_eq!(cw.expect_close(&[]).await, Some(4004));
    p.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn relay_limits() {
    let mut p = pair_with(|c| {
        c.limits.max_candidates = 2;
        c.limits.max_sdp_bytes = 64;
    })
    .await;
    let ctrl = p.ctrl.clone();
    let (mut cw, sid, _) = open_session(&mut p, &ctrl, "a1").await;
    cw.send(json!({"v": V, "type": "rtc.offer", "id": "big", "sid": sid, "data": {"attempt_id": "a1", "sdp": "x".repeat(65)}}))
        .await;
    assert_eq!(
        cw.recv_type("error").await["data"]["code"],
        "payload_too_large"
    );
    let cand = |s: String| json!({"v": V, "type": "rtc.candidate", "id": "c", "sid": sid, "data": {"attempt_id": "a1", "candidate": {"candidate": s}}});
    cw.send(cand("x".repeat(3000))).await;
    assert_eq!(
        cw.recv_type("error").await["data"]["code"],
        "payload_too_large"
    );
    for _ in 0..2 {
        cw.send(cand("candidate:1".into())).await;
        assert_eq!(cw.recv_type("ack").await["re"], "c");
        p.host_ws.recv_type("rtc.candidate").await;
    }
    cw.send(cand("candidate:1".into())).await;
    assert_eq!(
        cw.recv_type("error").await["data"]["code"],
        "payload_too_large"
    );
    // Binary frames are a protocol error.
    use futures_util::SinkExt;
    cw.stream
        .send(tokio_tungstenite::tungstenite::Message::Binary(
            vec![1, 2, 3].into(),
        ))
        .await
        .unwrap();
    assert_eq!(cw.expect_close(&[]).await, Some(4008));
    p.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn session_ticket_refresh_and_expiry() {
    let mut p = pair_with(|c| c.limits.ticket_ttl_s = 1).await;
    let ctrl = p.ctrl.clone();
    let (mut cw, sid, _) = open_session(&mut p, &ctrl, "a1").await;
    // A refreshed ticket opens a new controller connection that supersedes the old.
    let r = p
        .srv
        .post_empty(&format!("/api/v2/sessions/{sid}/ticket"), &ctrl)
        .await;
    let v = r.ok().clone();
    assert_eq!(v["session_id"], sid.as_str());
    assert_eq!(v["target"]["device_id"], p.host.id.as_str());
    assert_eq!(v["target"]["relation"], "own");
    assert_eq!(v["target"]["public_key"], p.host.public_key().as_str());
    let mut cw2 = p
        .srv
        .ws_connect(headers::TICKET, v["ticket"].as_str().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(
        cw2.recv_type("hello").await["data"]["session_id"],
        sid.as_str()
    );
    assert_eq!(cw.expect_close(&[]).await, Some(4001));
    // Another account device cannot refresh it.
    let other = p.srv.account_device("alice", "OTHER", false).await;
    p.srv
        .post_empty(&format!("/api/v2/sessions/{sid}/ticket"), &other)
        .await
        .err(404, "not_found");

    // A session whose ticket is never used expires.
    let r = p
        .srv
        .post(
            "/api/v2/sessions",
            &ctrl,
            json!({"target_device_id": p.host.id, "purpose": "files"}),
        )
        .await;
    let sid2 = r.ok()["session_id"].as_str().unwrap().to_owned();
    // (The previous session was replaced by the same controller.)
    assert_eq!(
        cw2.recv_type("session.ended").await["data"]["reason"],
        "replaced"
    );
    tokio::time::sleep(Duration::from_millis(1600)).await;
    p.srv
        .post_empty(&format!("/api/v2/sessions/{sid2}/ticket"), &ctrl)
        .await
        .err(404, "not_found");
    let _ = Method::GET;
    p.srv.shutdown().await;
}

/// Controller sessions signalled on the controller device's own presence
/// connection (`docs/protocol/v2-design.md` §11).
#[tokio::test(flavor = "multi_thread")]
async fn presence_connection_carries_controller_sessions() {
    let mut p = pair_with(|c| c.limits.resume_ttl_s = 1).await;
    let ctrl = p.ctrl.clone();
    let (mut cp, _) = p.srv.presence(&ctrl).await;
    let create = |presence: bool| {
        json!({"target_device_id": p.host.id, "purpose": "desktop", "presence": presence})
    };

    // No ticket for a presence-signalled session.
    let v = p.srv.post("/api/v2/sessions", &ctrl, create(true)).await.ok().clone();
    assert_eq!(v["ticket"], "");
    let sid = v["session_id"].as_str().unwrap().to_owned();

    // Start, relay both ways, settings.
    cp.send(start_frame("p1", &sid, "att-1")).await;
    let incoming = p.host_ws.recv_type("session.incoming").await;
    assert_eq!(incoming["sid"], sid.as_str());
    assert_eq!(incoming["data"]["controller"]["device_id"], ctrl.id.as_str());
    let ack = cp.recv_type("ack").await;
    assert_eq!(ack["re"], "p1");
    assert_eq!(ack["data"]["session_id"], sid.as_str());
    assert_eq!(ack["data"]["host"]["device_id"], p.host.id.as_str());
    cp.send(json!({"v": V, "type": "rtc.offer", "id": "o1", "sid": sid, "gen": 1,
        "data": {"attempt_id": "att-1", "restart": false, "network": 3, "sdp": "v=0 offer"}}))
        .await;
    assert_eq!(p.host_ws.recv_type("rtc.offer").await["sid"], sid.as_str());
    assert_eq!(cp.recv_type("ack").await["re"], "o1");
    p.host_ws
        .send(json!({"v": V, "type": "rtc.answer", "sid": sid, "gen": 1,
            "data": {"attempt_id": "att-1", "restart": false, "network": 3, "sdp": "v=0 answer"}}))
        .await;
    let answer = cp.recv_type("rtc.answer").await;
    assert_eq!(answer["sid"], sid.as_str());
    assert_eq!(answer["data"]["sdp"], "v=0 answer");
    // Wrong direction on the carrying link.
    cp.send(json!({"v": V, "type": "rtc.answer", "id": "w1", "sid": sid, "data": {"attempt_id": "att-1", "sdp": "x"}}))
        .await;
    assert_eq!(cp.recv_type("error").await["data"]["code"], "forbidden");

    // The host ends it: the controller is told, its presence stays open.
    p.host_ws
        .send(json!({"v": V, "type": "session.end", "sid": sid, "data": {"reason": "kicked"}}))
        .await;
    let ended = cp.recv_type("session.ended").await;
    assert_eq!((ended["sid"].as_str(), ended["data"]["reason"].as_str()), (Some(sid.as_str()), Some("kicked")));
    cp.send(json!({"v": V, "type": "ping", "data": {"t": 7}})).await;
    assert_eq!(cp.recv_type("pong").await["data"]["t"], 7);

    // The controller leaves.
    let v = p.srv.post("/api/v2/sessions", &ctrl, create(true)).await.ok().clone();
    let sid = v["session_id"].as_str().unwrap().to_owned();
    cp.send(start_frame("p2", &sid, "att-2")).await;
    p.host_ws.recv_type("session.incoming").await;
    cp.recv_type("ack").await;
    cp.send(json!({"v": V, "type": "session.leave", "id": "l1", "sid": sid})).await;
    assert_eq!(cp.recv_type("ack").await["re"], "l1");
    let ended = p.host_ws.recv_type("session.ended").await;
    assert_eq!((ended["sid"].as_str(), ended["data"]["reason"].as_str()), (Some(sid.as_str()), Some("controller_left")));
    cp.send(json!({"v": V, "type": "session.leave", "id": "l2", "sid": sid})).await;
    assert_eq!(cp.recv_type("error").await["data"]["code"], "not_found");

    // Only the controller device's own presence may start it.
    let other = p.srv.account_device("alice", "OTHER", false).await;
    let (mut op, _) = p.srv.presence(&other).await;
    let v = p.srv.post("/api/v2/sessions", &ctrl, create(true)).await.ok().clone();
    let sid = v["session_id"].as_str().unwrap().to_owned();
    op.send(start_frame("x1", &sid, "att-3")).await;
    assert_eq!(op.recv_type("error").await["data"]["code"], "forbidden");

    // A ticketed session moves to the presence connection.
    let (mut cw, sid, _) = p.srv.session_link(&ctrl, &p.host.id, "desktop").await;
    cp.send(start_frame("p3", &sid, "att-4")).await;
    p.host_ws.recv_type("session.incoming").await;
    assert_eq!(cp.recv_type("ack").await["re"], "p3");
    assert_eq!(cw.expect_close(&[]).await, Some(4001));

    // The carrying presence ends: so does the session.
    cp.send(json!({"v": V, "type": "bye"})).await;
    assert_eq!(cp.expect_close(&[]).await, Some(1000));
    let ended = p.host_ws.recv_type("session.ended").await;
    assert_eq!((ended["sid"].as_str(), ended["data"]["reason"].as_str()), (Some(sid.as_str()), Some("controller_left")));
    p.srv.shutdown().await;
}
