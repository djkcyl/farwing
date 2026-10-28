//! HTTPS control-plane tests: device identity and request signatures,
//! accounts and login sessions, devices, revocation, headers, guests,
//! wallpapers, config and updates.

mod common;

use common::*;
use farwing_protocol::crypto::{self, DeviceKey};
use farwing_protocol::headers;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn server_info_health_and_headers() {
    let srv = start().await;
    let r = srv.http.get(srv.url("/healthz")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert_eq!(r.text().await.unwrap(), "ok");

    // Discovery works without the protocol header and echoes the request ID.
    let r = srv
        .send(
            srv.http
                .get(srv.url("/api/v2/server"))
                .header(headers::REQUEST_ID, "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"),
        )
        .await;
    let v = r.ok();
    assert_eq!(
        v["protocol"],
        json!({"name": "farwing", "min": 2, "max": 2})
    );
    assert_eq!(v["server"]["name"], "Farwing Server");
    assert_eq!(v["server"]["instance_name"], "test");
    assert!(v["time_ms"].as_i64().unwrap() > 1_700_000_000_000);
    assert_eq!(r.headers[headers::PROTOCOL], "2");
    assert_eq!(
        r.headers[headers::REQUEST_ID],
        "0f1e2d3c-4b5a-6978-8796-a5b4c3d2e1f0"
    );
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn protocol_header_and_version_errors() {
    let srv = start().await;
    let dev = srv.init_device("PC", "windows", true).await;

    // Missing header.
    let r = srv
        .send(
            srv.http
                .post(srv.url("/api/v2/devices/init"))
                .json(&json!({})),
        )
        .await;
    r.err(400, "bad_request");
    assert_eq!(r.headers[headers::PROTOCOL], "2");
    // Unsupported protocol version.
    let r = srv
        .send(
            srv.http
                .get(srv.url("/api/v2/me"))
                .header(headers::PROTOCOL, "1"),
        )
        .await;
    r.err(426, "protocol_unsupported");
    assert_eq!(r.body["error"]["details"]["min"], 2);
    assert_eq!(r.body["error"]["details"]["max"], 2);
    assert_eq!(r.body["error"]["retryable"], false);

    // Realtime handshake with a bad ticket.
    let err = srv
        .ws_connect(headers::TICKET, "fwt_invalid", None)
        .await
        .err()
        .expect("rejected");
    match err {
        tokio_tungstenite::tungstenite::Error::Http(resp) => assert_eq!(resp.status(), 401),
        other => panic!("unexpected {other:?}"),
    }

    // Wrong frame version closes with 4009, garbage with 4008.
    let mut d = dev.clone();
    d.guest = Some(
        srv.post_empty("/api/v2/guest/session", &dev)
            .await
            .ok()["guest_token"]
            .as_str()
            .unwrap()
            .to_owned(),
    );
    let (mut ws, _) = srv.presence(&d).await;
    ws.send(json!({"v": 1, "type": "ping", "data": {"t": 1}}))
        .await;
    assert_eq!(ws.expect_close(&[]).await, Some(4009));
    let (mut ws, _) = srv.presence(&d).await;
    ws.send_text("not json").await;
    assert_eq!(ws.expect_close(&[]).await, Some(4008));

    // Unknown routes use the error envelope.
    srv.get("/api/v2/nope", &dev).await.err(404, "not_found");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn device_init_identity_and_profile_update() {
    let srv = start().await;
    let key = DeviceKey::generate();
    let body = TestServer::init_body(&key, "DESKTOP-1", "windows", true);
    let init = |b: &Value| srv.raw(Method::POST, "/api/v2/devices/init").json(b);
    let r = srv.send(init(&body)).await;
    let a = r.ok().clone();
    let id = a["device_id"].as_str().unwrap().to_owned();
    assert!(farwing_protocol::ids::is_device_id(&id));
    assert_eq!(a["alias"], "DESKTOP-1");
    assert!(a.get("device_token").is_none());
    // The same proof cannot be replayed.
    srv.send(init(&body)).await.err(401, "device_unauthorized");

    // Same key again (new proof): same device, profile updated.
    let mut again = TestServer::init_body(&key, "DESKTOP-RENAMED", "linux", true);
    again["version"] = json!("0.2.0");
    again["policy"]["max_participants"] = json!(1000);
    let r = srv.send(init(&again)).await;
    assert_eq!(r.ok()["device_id"], id.as_str());
    assert_eq!(r.ok()["alias"], "DESKTOP-1");
    let row = {
        let id = id.clone();
        srv.app()
            .db
            .call(move |c| farwing_server::db::get_device(c, &id))
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(row.platform, farwing_protocol::Platform::Linux);
    assert_eq!(row.version, "0.2.0");
    assert_eq!(row.name, "DESKTOP-RENAMED");
    assert_eq!(row.public_key, key.public_key());
    assert_eq!(row.policy.max_participants, farwing_protocol::limits::MAX_PARTICIPANTS);
    assert_eq!(row.hardware.as_ref().unwrap().cpu, "Test CPU");

    // A new key is a new device, even with identical hardware.
    let other = srv.init_device("DESKTOP-1", "windows", true).await;
    assert_ne!(other.id, id);

    // Proof failures.
    let mut forged = TestServer::init_body(&key, "X", "windows", true);
    forged["proof"] = json!(DeviceKey::generate().sign(b"nope"));
    srv.send(init(&forged)).await.err(401, "device_unauthorized");
    let mut stolen = TestServer::init_body(&key, "X", "windows", true);
    stolen["public_key"] = json!(DeviceKey::generate().public_key());
    srv.send(init(&stolen)).await.err(401, "device_unauthorized");
    let stale_ts = farwing_server::db::now_ms() - 10 * 60 * 1000;
    let mut stale = TestServer::init_body(&key, "X", "windows", true);
    stale["timestamp"] = json!(stale_ts);
    stale["proof"] = json!(key.sign(&crypto::init_proof_message(&key.public_key(), stale_ts)));
    srv.send(init(&stale)).await.err(401, "device_unauthorized");
    let mut bad_key = TestServer::init_body(&key, "X", "windows", true);
    bad_key["public_key"] = json!("not-a-key");
    srv.send(init(&bad_key)).await.err(400, "bad_request");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn request_signature_failures() {
    let srv = start().await;
    srv.create_user("alice");
    let a = srv.account_device("alice", "A", true).await;
    srv.get("/api/v2/me", &a).await.ok();
    let unauthorized = |r: Resp| r.err(401, "device_unauthorized");

    // Missing signature headers.
    unauthorized(
        srv.send(
            srv.raw(Method::GET, "/api/v2/me")
                .header("authorization", format!("Bearer {}", a.access.as_deref().unwrap())),
        )
        .await,
    );
    // Signed by another key.
    let mut wrong_key = a.clone();
    wrong_key.key = std::sync::Arc::new(DeviceKey::generate());
    unauthorized(srv.get("/api/v2/me", &wrong_key).await);
    // Unknown device ID.
    let mut unknown = a.clone();
    unknown.id = "zzzzzzzzzzzzzzzz".into();
    unauthorized(srv.get("/api/v2/me", &unknown).await);
    // Body differs from the signed body.
    let signed_body = serde_json::to_vec(&json!({"alias": "Signed"})).unwrap();
    let ts = farwing_server::db::now_ms();
    let sig = a.key.sign(&crypto::request_message(
        "PATCH",
        &format!("/api/v2/devices/{}", a.id),
        ts,
        &signed_body,
    ));
    let tampered = srv
        .raw(Method::PATCH, &format!("/api/v2/devices/{}", a.id))
        .header(headers::DEVICE_ID, &a.id)
        .header(headers::TIMESTAMP, ts.to_string())
        .header(headers::SIGNATURE, sig)
        .header("authorization", format!("Bearer {}", a.access.as_deref().unwrap()))
        .header("content-type", "application/json")
        .body(serde_json::to_vec(&json!({"alias": "Tampered"})).unwrap());
    unauthorized(srv.send(tampered).await);
    // Path or query differ from the signed ones.
    let ts = farwing_server::db::now_ms();
    let sig = a
        .key
        .sign(&crypto::request_message("GET", "/api/v2/me?x=1", ts, b""));
    let moved = srv
        .raw(Method::GET, "/api/v2/me?x=2")
        .header(headers::DEVICE_ID, &a.id)
        .header(headers::TIMESTAMP, ts.to_string())
        .header(headers::SIGNATURE, sig)
        .header("authorization", format!("Bearer {}", a.access.as_deref().unwrap()));
    unauthorized(srv.send(moved).await);
    // Clock skew beyond ±300 s, both directions.
    for skew in [-301_000i64, 301_000] {
        let ts = farwing_server::db::now_ms() + skew;
        unauthorized(
            srv.send(srv.signed_at(Method::GET, "/api/v2/me", &a, Vec::new(), ts))
                .await,
        );
    }
    // Within the skew it is fine.
    let ts = farwing_server::db::now_ms() - 120_000;
    srv.send(srv.signed_at(Method::GET, "/api/v2/me", &a, Vec::new(), ts))
        .await
        .ok();
    // Replays of a valid request are refused.
    let body = serde_json::to_vec(&json!({"alias": "Once"})).unwrap();
    let ts = farwing_server::db::now_ms();
    let path = format!("/api/v2/devices/{}", a.id);
    let once = || {
        srv.signed_at(Method::PATCH, &path, &a, body.clone(), ts)
            .header("content-type", "application/json")
    };
    srv.send(once()).await.ok();
    unauthorized(srv.send(once()).await);
    // The unsigned device init stays reachable; the rest of the API is not.
    unauthorized(
        srv.send(srv.raw(Method::POST, "/api/v2/presence/ticket"))
            .await,
    );
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn access_tokens_are_bound_and_slide() {
    let srv = start().await;
    srv.create_user("alice");
    let a = srv.account_device("alice", "A", true).await;
    let b = srv.account_device("alice", "B", true).await;
    // A token is only valid with the signature of the device it was issued to.
    let mut stolen = b.clone();
    stolen.access = a.access.clone();
    srv.get("/api/v2/me", &stolen)
        .await
        .err(401, "device_mismatch");
    // Without a signature the token is useless.
    srv.send(
        srv.raw(Method::GET, "/api/v2/me")
            .header("authorization", format!("Bearer {}", a.access.as_deref().unwrap())),
    )
    .await
    .err(401, "device_unauthorized");

    // Sliding expiry: last_used_at moves forward on use (at most once a
    // minute), and an idle token expires.
    let hash = farwing_server::crypto::token_hash(a.access.as_deref().unwrap());
    let set_last_used = |at: i64| {
        let hash = hash.clone();
        srv.app().db.call(move |c| {
            c.execute(
                "UPDATE access_tokens SET last_used_at = ?2 WHERE token_hash = ?1",
                rusqlite::params![hash, at],
            )
        })
    };
    let last_used = || {
        let hash = hash.clone();
        srv.app().db.call(move |c| {
            Ok(farwing_server::db::get_access_token(c, &hash)?
                .unwrap()
                .last_used_at)
        })
    };
    let now = farwing_server::db::now_s();
    set_last_used(now - 29 * 86_400).await.unwrap();
    srv.get("/api/v2/me", &a).await.ok();
    assert!(last_used().await.unwrap() >= now);
    set_last_used(now - 30).await.unwrap();
    srv.get("/api/v2/me", &a).await.ok();
    assert_eq!(last_used().await.unwrap(), now - 30);
    set_last_used(now - 31 * 86_400).await.unwrap();
    srv.get("/api/v2/me", &a)
        .await
        .err(401, "session_revoked");
    // An expired login no longer admits realtime connections or appears in
    // the login list.
    srv.post_empty("/api/v2/presence/ticket", &a)
        .await
        .err(401, "session_revoked");
    let list = srv.get("/api/v2/auth/sessions", &b).await.ok().clone();
    assert_eq!(list["sessions"].as_array().unwrap().len(), 1);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn auth_sessions_list_and_revoke() {
    let srv = start().await;
    srv.create_user("alice");
    srv.create_user("bob");
    let a = srv.account_device("alice", "A", true).await;
    let mut b = srv.init_device("B", "android", false).await;
    srv.login(&mut b, "alice").await;
    let bob = srv.account_device("bob", "BOB", false).await;
    let (mut bws, _) = srv.presence(&b).await;
    let (mut aws, _) = srv.presence(&a).await;

    let v = srv.get("/api/v2/auth/sessions", &a).await.ok().clone();
    let sessions = v["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let me = sessions.iter().find(|s| s["device_id"] == a.id.as_str()).unwrap();
    assert_eq!(me["current"], true);
    assert_eq!(me["device_alias"], "A");
    assert_eq!(me["platform"], "windows");
    assert!(me["created_at"].as_i64().unwrap() > 0);
    let other = sessions.iter().find(|s| s["device_id"] == b.id.as_str()).unwrap();
    assert_eq!(other["current"], false);
    assert_eq!(other["platform"], "android");
    // A second login on the same device replaces the first.
    let old = b.access.clone();
    srv.login(&mut b, "alice").await;
    let mut stale = b.clone();
    stale.access = old;
    srv.get("/api/v2/me", &stale)
        .await
        .err(401, "session_revoked");
    let v = srv.get("/api/v2/auth/sessions", &a).await.ok().clone();
    assert_eq!(v["sessions"].as_array().unwrap().len(), 2);

    // Other accounts' logins are not visible nor revocable.
    srv.delete(&format!("/api/v2/auth/sessions/{}", bob.id), &a)
        .await
        .err(404, "not_found");
    srv.delete("/api/v2/auth/sessions/zzzzzzzzzzzzzzzz", &a)
        .await
        .err(404, "not_found");

    // Revoking B signs it out: token revoked, presence closed, device gone.
    srv.delete(&format!("/api/v2/auth/sessions/{}", b.id), &a)
        .await
        .ok();
    assert_eq!(bws.expect_close(&[]).await, Some(4003));
    srv.get("/api/v2/me", &b).await.err(401, "session_revoked");
    let ev = aws
        .recv_matching("device removed", |v| {
            v["type"] == "device.changed" && v["data"]["kind"] == "removed"
        })
        .await;
    assert_eq!(ev["data"]["device"]["device_id"], b.id.as_str());
    let v = srv.get("/api/v2/auth/sessions", &a).await.ok().clone();
    assert_eq!(v["sessions"].as_array().unwrap().len(), 1);
    let list = srv.get("/api/v2/devices", &a).await.ok().clone();
    assert!(list["own"].as_array().unwrap().is_empty());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn account_login_devices_and_revocation() {
    let srv = start().await;
    let alice = srv.create_user("alice");
    srv.create_user("bob");

    let mut a = srv.init_device("ALICE-PC", "windows", true).await;
    // Wrong password and unknown user both yield `unauthorized`.
    a.access = None;
    srv.post(
        "/api/v2/auth/login",
        &a,
        json!({"username": "alice", "password": "wrong-password"}),
    )
    .await
    .err(401, "unauthorized");
    srv.post(
        "/api/v2/auth/login",
        &a,
        json!({"username": "nobody", "password": PASSWORD}),
    )
    .await
    .err(401, "unauthorized");
    // Login requires a device signature.
    srv.send(
        srv.raw(Method::POST, "/api/v2/auth/login")
            .json(&json!({"username": "alice", "password": PASSWORD})),
    )
    .await
    .err(401, "device_unauthorized");

    srv.login(&mut a, "alice").await;
    assert!(farwing_protocol::ids::is_token(
        a.access.as_deref().unwrap(),
        "fwa_"
    ));
    let me = srv.get("/api/v2/me", &a).await;
    assert_eq!(
        me.ok(),
        &json!({"user_id": alice, "username": "alice", "nickname": "alice-nick"})
    );

    let mut b = srv.account_device("alice", "ALICE-LAPTOP", true).await;
    let mut phone = srv.init_device("ALICE-PHONE", "android", false).await;
    srv.login(&mut phone, "alice").await;
    let mut bobs = srv.account_device("bob", "BOB-PC", true).await;

    // Lists.
    let v = srv.get("/api/v2/devices", &a).await.ok().clone();
    assert_eq!(v["current_device"]["device_id"], a.id.as_str());
    assert_eq!(v["current_device"]["granted"], json!(farwing_protocol::caps::ALL));
    let others: Vec<&str> = v["own"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["device_id"].as_str().unwrap())
        .collect();
    assert_eq!(others.len(), 2);
    assert!(others.contains(&b.id.as_str()) && others.contains(&phone.id.as_str()));
    assert_eq!(v["shared"], json!([]));
    let dev_b = v["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == b.id.as_str())
        .unwrap();
    assert_eq!(dev_b["status"], "offline");
    assert_eq!(dev_b["platform"], "windows");
    assert_eq!(dev_b["capabilities"][0], "host");
    assert_eq!(dev_b["policy"], host_policy(true));
    assert_eq!(dev_b["wol_supported"], false);
    assert_eq!(dev_b["wallpaper_url"], "");
    assert_eq!(dev_b["participants"], json!([]));
    assert_eq!(dev_b["owner"], Value::Null);
    assert_eq!(dev_b["granted"], json!(farwing_protocol::caps::ALL));

    // Detail.
    let d = srv
        .get(&format!("/api/v2/devices/{}/detail", b.id), &a)
        .await
        .ok()
        .clone();
    assert_eq!(d["device_id"], b.id.as_str());
    assert_eq!(d["alias"], "ALICE-LAPTOP");
    assert_eq!(d["platform"], "windows");
    assert_eq!(d["os"], "Windows 11 Pro");
    assert_eq!(d["online"], false);
    assert!(d["registered_at"].as_i64().unwrap() > 0);
    assert_eq!(d["hardware"]["memory"], 34359738368i64);
    assert_eq!(d["hardware"]["video"], json!(["Test GPU"]));
    assert_eq!(d["owner"], Value::Null);
    // Other accounts' devices are invisible.
    srv.get(&format!("/api/v2/devices/{}/detail", bobs.id), &a)
        .await
        .err(404, "device_not_found");

    // Rename.
    let r = srv
        .call(
            Method::PATCH,
            &format!("/api/v2/devices/{}", b.id),
            &a,
            Some(json!({"alias": "  Laptop  "})),
        )
        .await;
    assert_eq!(r.ok(), &json!({"device_id": b.id, "alias": "Laptop"}));
    srv.call(
        Method::PATCH,
        &format!("/api/v2/devices/{}", b.id),
        &a,
        Some(json!({"alias": ""})),
    )
    .await
    .err(400, "bad_request");
    srv.call(
        Method::PATCH,
        &format!("/api/v2/devices/{}", bobs.id),
        &a,
        Some(json!({"alias": "x"})),
    )
    .await
    .err(404, "device_not_found");

    // Remove B: its token is revoked.
    srv.delete(&format!("/api/v2/devices/{}", b.id), &a)
        .await
        .ok();
    srv.get("/api/v2/me", &b).await.err(401, "session_revoked");
    let v = srv.get("/api/v2/devices", &a).await.ok().clone();
    assert_eq!(v["own"].as_array().unwrap().len(), 1);
    // B can log in again.
    srv.login(&mut b, "alice").await;
    srv.get("/api/v2/me", &b).await.ok();

    // Logging another account in on a device revokes the previous account's token.
    let old = phone.access.clone();
    srv.login(&mut phone, "bob").await;
    let mut stale = phone.clone();
    stale.access = old;
    srv.get("/api/v2/me", &stale)
        .await
        .err(401, "session_revoked");
    let v = srv.get("/api/v2/devices", &a).await.ok().clone();
    assert!(
        v["own"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["device_id"] != phone.id.as_str())
    );

    // Logout revokes the token and unbinds the device.
    srv.post_empty("/api/v2/auth/logout", &bobs).await.ok();
    srv.get("/api/v2/me", &bobs)
        .await
        .err(401, "session_revoked");
    bobs.access = Some(format!("fwa_{}", "B".repeat(43)));
    srv.get("/api/v2/me", &bobs).await.err(401, "unauthorized");
    let v = srv.get("/api/v2/devices", &phone).await.ok().clone();
    assert!(
        v["own"]
            .as_array()
            .unwrap()
            .iter()
            .all(|d| d["device_id"] != bobs.id.as_str())
    );
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn device_changed_pushes_and_unbind_closes_presence() {
    let srv = start().await;
    srv.create_user("alice");
    let a = srv.account_device("alice", "A", true).await;
    let b = srv.account_device("alice", "B", true).await;
    let (mut wa, _) = srv.presence(&a).await;

    // B coming online is pushed to A.
    let (mut wb, _) = srv.presence(&b).await;
    let ev = wa.recv_type("device.changed").await;
    assert_eq!(ev["data"]["kind"], "updated");
    assert_eq!(ev["data"]["shared"], false);
    assert_eq!(ev["data"]["device"]["device_id"], b.id.as_str());
    assert_eq!(ev["data"]["device"]["status"], "online");

    // A rename reaches every online device of the account, including B itself.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/devices/{}", b.id),
        &a,
        Some(json!({"alias": "Renamed"})),
    )
    .await
    .ok();
    let ev = wb.recv_type("device.changed").await;
    assert_eq!(ev["data"]["device"]["alias"], "Renamed");
    let ev = wa.recv_type("device.changed").await;
    assert_eq!(ev["data"]["device"]["alias"], "Renamed");

    // A policy change on B reaches A; the stored policy is normalized.
    let mut policy = host_policy(true);
    policy["control"] = json!(false);
    policy["max_participants"] = json!(0);
    let r = srv.put("/api/v2/devices/self/policy", &b, policy).await;
    assert_eq!(r.ok()["max_participants"], 1);
    let ev = wa.recv_type("device.changed").await;
    assert_eq!(ev["data"]["device"]["policy"]["control"], false);
    assert_eq!(ev["data"]["device"]["policy"]["max_participants"], 1);

    // A new device bound to the account is announced.
    let c = srv.account_device("alice", "C", false).await;
    let ev = wa.recv_type("device.changed").await;
    assert_eq!(ev["data"]["kind"], "added");
    assert_eq!(ev["data"]["device"]["device_id"], c.id.as_str());

    // Re-initialising with a new version is announced too.
    let mut body = TestServer::init_body(&c.key, "C", "windows", false);
    body["version"] = json!("0.3.0");
    srv.send(srv.raw(Method::POST, "/api/v2/devices/init").json(&body))
        .await
        .ok();
    let ev = wa.recv_type("device.changed").await;
    assert_eq!(ev["data"]["device"]["version"], "0.3.0");

    // Removing B closes its presence with 4003 and tells A.
    srv.delete(&format!("/api/v2/devices/{}", b.id), &a)
        .await
        .ok();
    assert_eq!(wb.expect_close(&[]).await, Some(4003));
    let ev = wa
        .recv_matching("device removed", |v| {
            v["type"] == "device.changed" && v["data"]["kind"] == "removed"
        })
        .await;
    assert_eq!(ev["data"]["device"]["device_id"], b.id.as_str());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn admin_api_revocation_applies_at_once() {
    let srv = start().await;
    let carol = srv.create_user("carol");
    let a = srv.account_device("carol", "A", true).await;
    let (mut ws, _) = srv.presence(&a).await;
    let token = srv.admin_token("admin");
    let user = format!("/api/v2/admin/users/{carol}");

    srv.admin_call(Method::PATCH, &user, &token, Some(json!({"disabled": true})))
        .await
        .ok();
    srv.get("/api/v2/me", &a).await.err(401, "session_revoked");
    assert_eq!(ws.expect_close(&[]).await, Some(4003));
    let mut again = a.clone();
    again.access = None;
    srv.post(
        "/api/v2/auth/login",
        &again,
        json!({"username": "carol", "password": PASSWORD}),
    )
    .await
    .err(401, "unauthorized");

    srv.admin_call(Method::PATCH, &user, &token, Some(json!({"disabled": false})))
        .await
        .ok();
    srv.login(&mut again, "carol").await;
    let (mut ws, _) = srv.presence(&again).await;
    srv.admin_call(
        Method::POST,
        &format!("{user}/password"),
        &token,
        Some(json!({"password": "another-password"})),
    )
    .await
    .ok();
    assert_eq!(ws.expect_close(&[]).await, Some(4003));
    srv.get("/api/v2/me", &again)
        .await
        .err(401, "session_revoked");

    // Unbinding a device.
    let mut b = again.clone();
    b.access = None;
    let r = srv
        .post(
            "/api/v2/auth/login",
            &b,
            json!({"username": "carol", "password": "another-password"}),
        )
        .await;
    b.access = Some(r.ok()["access_token"].as_str().unwrap().to_owned());
    let (mut ws, _) = srv.presence(&b).await;
    srv.admin_call(
        Method::DELETE,
        &format!("/api/v2/admin/devices/{}", b.id),
        &token,
        None,
    )
    .await
    .ok();
    assert_eq!(ws.expect_close(&[]).await, Some(4003));
    srv.get("/api/v2/me", &b).await.err(401, "session_revoked");
    srv.admin_call(Method::GET, "/api/v2/admin/sessions", &token, None).await.ok();
    srv.admin_call(Method::DELETE, &user, &token, None).await.ok();
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn guest_registration_and_restrictions() {
    // Guests disabled: devices still register (and can only log in), but
    // there are no guest sessions.
    let srv = start_with(|c| c.guest.enabled = false).await;
    srv.create_user("erin");
    let mut d = srv.init_device("X", "windows", true).await;
    srv.post_empty("/api/v2/guest/session", &d)
        .await
        .err(403, "forbidden");
    srv.post_empty("/api/v2/presence/ticket", &d)
        .await
        .err(401, "unauthorized");
    srv.login(&mut d, "erin").await;
    srv.post_empty("/api/v2/presence/ticket", &d).await.ok();
    srv.shutdown().await;

    let srv = start().await;
    srv.create_user("dave");
    let a = srv.account_device("dave", "A", true).await;

    // Guest token: config and presence work, account endpoints do not.
    let g = srv.guest_device("GUEST").await;
    assert!(farwing_protocol::ids::is_token(
        g.guest.as_deref().unwrap(),
        "fwg_"
    ));
    srv.get("/api/v2/me", &g).await.err(401, "unauthorized");
    srv.get("/api/v2/devices", &g)
        .await
        .err(401, "unauthorized");
    let (_gw, hello) = srv.presence(&g).await;
    assert_eq!(hello["data"]["epoch"], 1);
    // Guests may set their policy (e.g. accept assistance).
    srv.set_policy(&g, host_policy(true)).await;
    // A guest token from another device is rejected.
    let mut other = srv.init_device("OTHER", "windows", true).await;
    other.guest = g.guest.clone();
    srv.put("/api/v2/devices/self/policy", &other, host_policy(true))
        .await
        .err(401, "device_mismatch");

    // An account device whose presence is guest-authenticated only accepts assistance.
    let mut b = srv.account_device("dave", "B", true).await;
    let guest_token = srv.post_empty("/api/v2/guest/session", &b).await.ok()["guest_token"]
        .as_str()
        .unwrap()
        .to_owned();
    let access = b.access.take();
    b.guest = Some(guest_token);
    let (_bw, _) = srv.presence(&b).await;
    b.guest = None;
    b.access = access;
    srv.post(
        "/api/v2/sessions",
        &a,
        json!({"target_device_id": b.id, "purpose": "desktop"}),
    )
    .await
    .err(403, "forbidden");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wallpaper_upload_download() {
    let srv = start().await;
    srv.create_user("erin");
    let a = srv.account_device("erin", "A", true).await;
    let b = srv.account_device("erin", "B", true).await;

    let send_raw = |dev: &Dev, bytes: Vec<u8>| {
        srv.signed(Method::PUT, "/api/v2/devices/self/wallpaper", dev, bytes)
            .header("content-type", "application/octet-stream")
    };
    srv.send(send_raw(&a, b"GIF89a....".to_vec()))
        .await
        .err(400, "bad_request");
    let too_big = {
        let mut v = b"\x89PNG\r\n\x1a\n".to_vec();
        v.resize(4 * 1024 * 1024 + 1, 0);
        v
    };
    srv.send(send_raw(&a, too_big))
        .await
        .err(413, "payload_too_large");
    // JSON endpoints keep their 64 KiB limit for signed bodies.
    let big_json = json!({"alias": "x".repeat(70 * 1024)});
    srv.call(
        Method::PATCH,
        &format!("/api/v2/devices/{}", a.id),
        &a,
        Some(big_json),
    )
    .await
    .err(413, "payload_too_large");

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[7u8; 1000]);
    let r = srv.send(send_raw(&a, png.clone())).await;
    let url = r.ok()["wallpaper_url"].as_str().unwrap().to_owned();
    assert!(
        url.starts_with(&format!("/api/v2/devices/{}/wallpaper?v=", a.id)),
        "{url}"
    );

    let list = srv.get("/api/v2/devices", &b).await.ok().clone();
    assert_eq!(list["own"][0]["wallpaper_url"], url.as_str());

    let resp = srv.req(Method::GET, &url, &b).send().await.unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers()["content-type"], "image/png");
    let etag = resp.headers()["etag"].to_str().unwrap().to_owned();
    assert!(url.contains(&etag.trim_matches('"')[..16]));
    assert_eq!(resp.bytes().await.unwrap().to_vec(), png);
    let resp = srv
        .req(Method::GET, &url, &b)
        .header("if-none-match", &etag)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 304);

    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limits_for_login_and_registration() {
    let srv = start_with(|c| {
        c.limits.login_failures_per_user = 3;
        c.limits.device_init_per_ip_hour = 4;
    })
    .await;
    srv.create_user("frank");
    let d = srv.init_device("A", "windows", true).await;
    for _ in 0..3 {
        srv.post(
            "/api/v2/auth/login",
            &d,
            json!({"username": "frank", "password": "bad-password"}),
        )
        .await
        .err(401, "unauthorized");
    }
    let r = srv
        .post(
            "/api/v2/auth/login",
            &d,
            json!({"username": "frank", "password": PASSWORD}),
        )
        .await;
    r.err(429, "rate_limited");
    assert!(
        r.body["error"]["details"]["retry_after_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(r.headers.contains_key("retry-after"));

    // Registration of new keys per source IP (simulated through a trusted
    // proxy header); re-initialising a known key is not limited.
    let key = DeviceKey::generate();
    let init = |ip: &str, key: &DeviceKey| {
        srv.raw(Method::POST, "/api/v2/devices/init")
            .header("x-forwarded-for", ip)
            .json(&TestServer::init_body(key, "X", "windows", false))
    };
    srv.send(init("198.51.100.1", &key)).await.ok();
    for _ in 0..3 {
        srv.send(init("198.51.100.1", &DeviceKey::generate()))
            .await
            .ok();
    }
    srv.send(init("198.51.100.1", &DeviceKey::generate()))
        .await
        .err(429, "rate_limited");
    srv.send(init("198.51.100.1", &key)).await.ok();
    srv.send(init("198.51.100.2", &DeviceKey::generate()))
        .await
        .ok();
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn updates_and_config() {
    let srv = start_with(|c| {
        c.bitrate_limit_mbps = Some(40);
    })
    .await;
    // Announcements are managed in the web console; written to the database here.
    {
        let db = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
        for (id, platform, channel, version, published_at, prerelease) in [
            ("a1", "windows", "stable", "1.0.1", 1_760_000_000, false),
            ("a2", "windows", "beta", "1.1.0-beta.1", 1_760_100_000, true),
            ("a3", "linux", "stable", "1.0.0", 1_760_000_000, false),
        ] {
            farwing_server::webdb::save_announcement(
                &db,
                &farwing_server::webdb::Announcement {
                    id: id.into(),
                    platform: platform.into(),
                    channel: channel.into(),
                    version: version.into(),
                    notes: String::new(),
                    url: format!("https://dl.example/{version}"),
                    published_at,
                    prerelease,
                },
            )
            .unwrap();
        }
    }
    let v = srv
        .send(srv.raw(Method::GET, "/api/v2/updates/windows"))
        .await;
    assert_eq!(v.ok()["latest"]["version"], "1.0.1");
    let v = srv
        .send(srv.raw(Method::GET, "/api/v2/updates/windows?channel=beta"))
        .await;
    assert_eq!(v.ok()["latest"]["version"], "1.1.0-beta.1");
    assert_eq!(v.ok()["latest"]["prerelease"], true);
    let v = srv
        .send(srv.raw(Method::GET, "/api/v2/updates/linux"))
        .await;
    assert_eq!(v.ok()["latest"]["version"], "1.0.0");
    let v = srv
        .send(srv.raw(Method::GET, "/api/v2/updates/android"))
        .await;
    assert_eq!(v.ok(), &json!({"latest": null}));
    // The configured limit is in effect.
    assert_eq!(srv.app().cfg().bitrate_limit_mbps, Some(40));
    tokio::time::sleep(Duration::from_millis(10)).await;
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn proxy_mode_serves_plain_http_behind_tls_proxy() {
    use futures_util::StreamExt;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    let srv = start_with(|c| {
        c.tls.mode = farwing_server::config::TlsMode::Proxy;
        c.public_url = "https://farwing.example.com/farwing".into();
    })
    .await;
    assert!(srv.base.starts_with("http://"));
    srv.send(srv.raw(Method::GET, "/api/v2/server")).await.ok();
    srv.create_user("pat");
    let mut d = srv.init_device("P", "windows", true).await;
    d.xff = Some("192.0.2.44".into());
    srv.login(&mut d, "pat").await;
    let r = srv.post_empty("/api/v2/presence/ticket", &d).await;
    assert_eq!(
        r.ok()["endpoints"],
        json!(["wss://farwing.example.com/farwing/api/v2/ws"])
    );
    // The proxy strips the public path prefix; a client signs the path it
    // requested (with the prefix).
    let ts = farwing_server::db::now_ms();
    let sig = d.key.sign(&crypto::request_message(
        "GET",
        "/farwing/api/v2/me",
        ts,
        b"",
    ));
    srv.send(
        srv.raw(Method::GET, "/api/v2/me")
            .header(headers::DEVICE_ID, &d.id)
            .header(headers::TIMESTAMP, ts.to_string())
            .header(headers::SIGNATURE, sig)
            .header("authorization", format!("Bearer {}", d.access.as_deref().unwrap())),
    )
    .await
    .ok();
    // Plain WebSocket upgrade (the proxy terminates TLS); the client IP comes
    // from the trusted proxy's X-Forwarded-For.
    let mut req = format!("ws://127.0.0.1:{}/api/v2/ws", srv.port)
        .into_client_request()
        .unwrap();
    req.headers_mut()
        .insert(headers::PROTOCOL, "2".parse().unwrap());
    req.headers_mut().insert(
        headers::TICKET,
        r.ok()["ticket"].as_str().unwrap().parse().unwrap(),
    );
    req.headers_mut()
        .insert("x-forwarded-for", "192.0.2.44".parse().unwrap());
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", srv.port))
        .await
        .unwrap();
    let (mut ws, _) = tokio_tungstenite::client_async(req, tcp).await.unwrap();
    let hello = ws.next().await.unwrap().unwrap();
    assert!(hello.to_text().unwrap().contains("\"hello\""));
    assert_eq!(
        srv.app()
            .hub
            .presence_ip(&d.id)
            .map(|i| i.to_string())
            .as_deref(),
        Some("192.0.2.44")
    );
    srv.shutdown().await;
}

#[test]
fn self_signed_certificate_is_generated_once_and_reused() {
    use farwing_server::config::{Config, TlsMode};
    farwing_server::tls::install_crypto_provider();
    let dir = tempfile::tempdir().unwrap();
    let mut cfg = Config::initial(dir.path().join("d"), "127.0.0.1:0".parse().unwrap());
    cfg.public_url = "https://farwing.test:8443".into();
    cfg.tls.mode = TlsMode::SelfSigned;
    cfg.tls.hostnames = vec!["farwing.test".into()];
    let fp = |cfg: &Config| {
        farwing_server::tls::prepare(cfg)
            .unwrap()
            .unwrap()
            .presented
            .fingerprint()
    };
    let a = fp(&cfg);
    assert_eq!(a, fp(&cfg));
    assert_eq!(a.len(), 95);
    // A new name needs a new certificate.
    cfg.tls.hostnames.push("other.test".into());
    let b = fp(&cfg);
    assert_ne!(a, b);
    // The same files work as an uploaded certificate.
    let tls = cfg.tls_dir();
    let cert = std::fs::read_to_string(tls.join("cert.pem")).unwrap();
    let key = std::fs::read_to_string(tls.join("key.pem")).unwrap();
    assert_eq!(farwing_server::tls::store_provided(&tls, &cert, &key).unwrap(), b);
    cfg.tls.mode = TlsMode::Provided;
    assert_eq!(fp(&cfg), b);
    // A key of another certificate is refused.
    let other = rcgen::generate_simple_self_signed(vec!["x.test".into()]).unwrap();
    let wrong = other.signing_key.serialize_pem();
    assert!(farwing_server::tls::store_provided(&tls, &cert, &wrong).is_err());
    assert!(farwing_server::tls::store_provided(&tls, "junk", &key).is_err());
}

#[test]
fn other_schema_versions_are_refused() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("farwing.db");
    {
        let c = rusqlite::Connection::open(&path).unwrap();
        c.execute_batch(
            "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             INSERT INTO meta VALUES ('schema_version', '3');",
        )
        .unwrap();
    }
    let err = farwing_server::db::open_connection(&path).unwrap_err();
    assert!(format!("{err:#}").contains("no migration"), "{err:#}");
    // A fresh database is created at the current version.
    let fresh = dir.path().join("fresh.db");
    farwing_server::db::open_connection(&fresh).unwrap();
    farwing_server::db::open_connection(&fresh).unwrap();
}
