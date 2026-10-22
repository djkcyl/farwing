//! Friends, friend requests and grants: lifecycle, pushes, shared devices and
//! the effect of grants on sessions.

mod common;

use common::*;
use serde_json::{Value, json};

/// Users `alice` and `bob`, each with one host device online.
struct Two {
    srv: TestServer,
    alice: Dev,
    alice_id: String,
    aw: Ws,
    bob: Dev,
    bob_id: String,
    bw: Ws,
}

async fn two() -> Two {
    let srv = start().await;
    let alice_id = srv.create_user("alice");
    let bob_id = srv.create_user("bob");
    let alice = srv.account_device("alice", "ALICE-PC", true).await;
    let bob = srv.account_device("bob", "BOB-PC", true).await;
    let (aw, _) = srv.presence(&alice).await;
    let (bw, _) = srv.presence(&bob).await;
    Two {
        srv,
        alice,
        alice_id,
        aw,
        bob,
        bob_id,
        bw,
    }
}

fn is_friend_change(kind: &'static str) -> impl Fn(&Value) -> bool {
    move |v| v["type"] == "friend.changed" && v["data"]["kind"] == kind
}

fn is_shared_change(kind: &'static str, device: String) -> impl Fn(&Value) -> bool {
    move |v| {
        v["type"] == "device.changed"
            && v["data"]["kind"] == kind
            && v["data"]["shared"] == true
            && v["data"]["device"]["device_id"] == device.as_str()
    }
}

/// `alice` asks `bob`, `bob` accepts.
async fn befriend(t: &Two) {
    let r = t
        .srv
        .post("/api/v2/friends/requests", &t.alice, json!({"username": "bob"}))
        .await;
    let id = r.ok()["sent"]["request"]["id"].as_str().unwrap().to_owned();
    t.srv
        .post_empty(&format!("/api/v2/friends/requests/{id}/accept"), &t.bob)
        .await
        .ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn friend_requests_lifecycle_and_pushes() {
    let mut t = two().await;
    let srv = &t.srv;
    let carol_id = srv.create_user("carol");
    let carol = srv.account_device("carol", "CAROL-PC", true).await;
    let (mut cw, _) = srv.presence(&carol).await;

    // Errors.
    srv.post("/api/v2/friends/requests", &t.alice, json!({"username": "nobody"}))
        .await
        .err(404, "user_not_found");
    srv.post("/api/v2/friends/requests", &t.alice, json!({"username": "alice"}))
        .await
        .err(400, "bad_request");

    // alice -> bob: bob is told, both see the pending request.
    let r = srv
        .post("/api/v2/friends/requests", &t.alice, json!({"username": "BOB"}))
        .await;
    let sent = r.ok()["sent"]["request"].clone();
    assert_eq!(sent["user"]["user_id"], t.bob_id.as_str());
    assert_eq!(sent["user"]["username"], "bob");
    assert_eq!(sent["user"]["nickname"], "bob-nick");
    let id = sent["id"].as_str().unwrap().to_owned();
    let ev = t
        .bw
        .recv_matching("request_received", is_friend_change("request_received"))
        .await;
    assert_eq!(ev["data"]["user"]["username"], "alice");
    // Asking again returns the pending request.
    let again = srv
        .post("/api/v2/friends/requests", &t.alice, json!({"username": "bob"}))
        .await;
    assert_eq!(again.ok()["sent"]["request"]["id"], id.as_str());
    let reqs = srv.get("/api/v2/friends/requests", &t.bob).await.ok().clone();
    assert_eq!(reqs["incoming"][0]["id"], id.as_str());
    assert_eq!(reqs["incoming"][0]["user"]["user_id"], t.alice_id.as_str());
    assert_eq!(reqs["outgoing"], json!([]));
    let reqs = srv.get("/api/v2/friends/requests", &t.alice).await.ok().clone();
    assert_eq!(reqs["outgoing"][0]["user"]["username"], "bob");

    // Only the recipient accepts; only the sender cancels.
    srv.post_empty(&format!("/api/v2/friends/requests/{id}/accept"), &t.alice)
        .await
        .err(404, "not_found");
    srv.delete(&format!("/api/v2/friends/requests/{id}"), &t.bob)
        .await
        .err(404, "not_found");

    // bob accepts: alice is told; each now sees the other's devices.
    let r = srv
        .post_empty(&format!("/api/v2/friends/requests/{id}/accept"), &t.bob)
        .await;
    assert_eq!(r.ok()["user"]["username"], "alice");
    assert!(r.ok()["since"].as_i64().unwrap() > 0);
    let ev = t
        .aw
        .recv_matching("request_accepted", is_friend_change("request_accepted"))
        .await;
    assert_eq!(ev["data"]["user"]["username"], "bob");
    let ev = t
        .aw
        .recv_matching("bob's device", is_shared_change("added", t.bob.id.clone()))
        .await;
    assert_eq!(ev["data"]["device"]["owner"]["username"], "bob");
    assert_eq!(ev["data"]["device"]["granted"], json!(["view", "annotation"]));
    t.bw
        .recv_matching("alice's device", is_shared_change("added", t.alice.id.clone()))
        .await;
    let friends = srv.get("/api/v2/friends", &t.alice).await.ok().clone();
    assert_eq!(friends["friends"][0]["user"]["user_id"], t.bob_id.as_str());
    assert!(friends["friends"][0]["since"].as_i64().unwrap() > 0);
    let list = srv.get("/api/v2/devices", &t.alice).await.ok().clone();
    let shared = list["shared"].as_array().unwrap();
    assert_eq!(shared.len(), 1);
    assert_eq!(shared[0]["device_id"], t.bob.id.as_str());
    assert_eq!(shared[0]["owner"]["user_id"], t.bob_id.as_str());
    assert_eq!(shared[0]["granted"], json!(["view", "annotation"]));
    assert_eq!(shared[0]["status"], "online");
    // Default grants both ways.
    let g = srv
        .get(&format!("/api/v2/grants/{}", t.bob_id), &t.alice)
        .await
        .ok()
        .clone();
    assert_eq!(g, json!({"default": ["view", "annotation"], "devices": {}}));
    // Friends' devices show limited details and cannot be managed.
    let d = srv
        .get(&format!("/api/v2/devices/{}/detail", t.bob.id), &t.alice)
        .await
        .ok()
        .clone();
    assert_eq!(d["owner"]["username"], "bob");
    assert_eq!(d["connect_id"], Value::Null);
    assert_eq!(d["hardware"], Value::Null);
    assert_eq!(d["online"], true);
    srv.call(
        reqwest::Method::PATCH,
        &format!("/api/v2/devices/{}", t.bob.id),
        &t.alice,
        Some(json!({"alias": "mine now"})),
    )
    .await
    .err(403, "forbidden");
    srv.delete(&format!("/api/v2/devices/{}", t.bob.id), &t.alice)
        .await
        .err(403, "forbidden");
    // Already friends.
    srv.post("/api/v2/friends/requests", &t.alice, json!({"username": "bob"}))
        .await
        .err(409, "already_friends");
    // Presence changes of a shared device reach the friend.
    t.bw.send(json!({"v": V, "type": "bye"})).await;
    let offline = t
        .aw
        .recv_matching("bob offline", is_shared_change("updated", t.bob.id.clone()))
        .await;
    assert_eq!(offline["data"]["device"]["status"], "offline");
    t.bw = srv.presence(&t.bob).await.0;
    let online = t
        .aw
        .recv_matching("bob online", is_shared_change("updated", t.bob.id.clone()))
        .await;
    assert_eq!(online["data"]["device"]["status"], "online");

    // Mutual requests: carol asks alice, alice asks carol -> friends at once.
    let r = srv
        .post("/api/v2/friends/requests", &carol, json!({"username": "alice"}))
        .await;
    r.ok();
    let r = srv
        .post("/api/v2/friends/requests", &t.alice, json!({"username": "carol"}))
        .await;
    assert_eq!(r.ok()["accepted"]["friend"]["user"]["user_id"], carol_id.as_str());
    cw.recv_matching("request_accepted", is_friend_change("request_accepted"))
        .await;
    let reqs = srv.get("/api/v2/friends/requests", &carol).await.ok().clone();
    assert_eq!(reqs, json!({"incoming": [], "outgoing": []}));

    // Reject and cancel.
    srv.create_user("dave");
    let dave = srv.account_device("dave", "DAVE-PC", false).await;
    let (mut dw, _) = srv.presence(&dave).await;
    let r = srv
        .post("/api/v2/friends/requests", &dave, json!({"username": "bob"}))
        .await;
    let id = r.ok()["sent"]["request"]["id"].as_str().unwrap().to_owned();
    srv.post_empty(&format!("/api/v2/friends/requests/{id}/reject"), &t.bob)
        .await
        .ok();
    let ev = dw
        .recv_matching("request_rejected", is_friend_change("request_rejected"))
        .await;
    assert_eq!(ev["data"]["user"]["username"], "bob");
    let r = srv
        .post("/api/v2/friends/requests", &dave, json!({"username": "bob"}))
        .await;
    let id = r.ok()["sent"]["request"]["id"].as_str().unwrap().to_owned();
    srv.delete(&format!("/api/v2/friends/requests/{id}"), &dave)
        .await
        .ok();
    t.bw
        .recv_matching("request_cancelled", is_friend_change("request_cancelled"))
        .await;
    let reqs = srv.get("/api/v2/friends/requests", &t.bob).await.ok().clone();
    assert_eq!(reqs["incoming"], json!([]));

    // Removing a friend: pushes on both sides, shared devices disappear.
    srv.delete(&format!("/api/v2/friends/{}", t.bob_id), &t.alice)
        .await
        .ok();
    let ev = t
        .bw
        .recv_matching("removed", is_friend_change("removed"))
        .await;
    assert_eq!(ev["data"]["user"]["username"], "alice");
    t.bw
        .recv_matching("alice's device removed", is_shared_change("removed", t.alice.id.clone()))
        .await;
    t.aw
        .recv_matching("bob's device removed", is_shared_change("removed", t.bob.id.clone()))
        .await;
    let list = srv.get("/api/v2/devices", &t.alice).await.ok().clone();
    let shared: Vec<&str> = list["shared"]
        .as_array()
        .unwrap()
        .iter()
        .map(|d| d["owner"]["username"].as_str().unwrap())
        .collect();
    assert_eq!(shared, ["carol"]);
    srv.get(&format!("/api/v2/grants/{}", t.bob_id), &t.alice)
        .await
        .err(404, "not_found");
    srv.delete(&format!("/api/v2/friends/{}", t.bob_id), &t.alice)
        .await
        .err(404, "not_found");
    t.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn friend_request_rate_limit() {
    let srv = start().await;
    srv.create_user("alice");
    let alice = srv.account_device("alice", "A", false).await;
    for i in 0..20 {
        srv.post(
            "/api/v2/friends/requests",
            &alice,
            json!({"username": format!("ghost{i}")}),
        )
        .await
        .err(404, "user_not_found");
    }
    srv.post("/api/v2/friends/requests", &alice, json!({"username": "ghost"}))
        .await
        .err(429, "rate_limited");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn grants_gate_sessions_and_follow_changes() {
    let mut t = two().await;
    befriend(&t).await;
    let srv = &t.srv;
    let bob_laptop = srv.account_device("bob", "BOB-LAPTOP", true).await;
    // What alice grants bob.
    let grants_path = format!("/api/v2/grants/{}", t.bob_id);

    // Without `view` (but with `files`) bob sees alice's device but cannot
    // open a desktop session; files work.
    srv.put(&grants_path, &t.alice, json!({"default": ["files"]}))
        .await
        .ok();
    t.bw.recv_matching("grants_changed", is_friend_change("grants_changed"))
        .await;
    srv.post(
        "/api/v2/sessions",
        &t.bob,
        json!({"target_device_id": t.alice.id, "purpose": "desktop"}),
    )
    .await
    .err(403, "forbidden");
    let (fw, _, _) = srv.session_link(&t.bob, &t.alice.id, "files").await;
    fw.close().await;
    // No grant at all: the device is not visible.
    srv.put(&grants_path, &t.alice, json!({"default": []})).await.ok();
    srv.post(
        "/api/v2/sessions",
        &t.bob,
        json!({"target_device_id": t.alice.id, "purpose": "files"}),
    )
    .await
    .err(404, "device_not_found");
    let list = srv.get("/api/v2/devices", &t.bob).await.ok().clone();
    assert_eq!(list["shared"], json!([]));

    // With `view` (plus control, as a device override): a desktop session.
    let r = srv
        .put(
            &grants_path,
            &t.alice,
            json!({"default": ["annotation", "bogus", "view"], "devices": {t.alice.id.clone(): ["control", "view", "clipboard", "view"]}}),
        )
        .await;
    assert_eq!(
        r.ok(),
        &json!({"default": ["view", "annotation"], "devices": {t.alice.id.clone(): ["view", "control", "clipboard"]}})
    );
    t.bw
        .recv_matching("alice's device visible", is_shared_change("added", t.alice.id.clone()))
        .await;
    let (mut cw, sid, ticket) = srv.session_link(&t.bob, &t.alice.id, "desktop").await;
    assert_eq!(ticket["target"]["relation"], "friend");
    assert_eq!(ticket["target"]["granted"], json!(["view", "control", "clipboard"]));
    assert_eq!(ticket["target"]["public_key"], t.alice.public_key().as_str());
    cw.send(start_frame("s", &sid, "a1")).await;
    let incoming = t.aw.recv_type("session.incoming").await;
    assert_eq!(incoming["data"]["relation"], "friend");
    assert_eq!(incoming["data"]["grants"], json!(["view", "control", "clipboard"]));
    assert_eq!(incoming["data"]["controller"]["username"], "bob");
    assert_eq!(incoming["data"]["controller"]["public_key"], t.bob.public_key().as_str());
    let ack = cw.recv_type("ack").await;
    assert_eq!(ack["data"]["relation"], "friend");
    assert_eq!(ack["data"]["host"]["public_key"], t.alice.public_key().as_str());
    // Friends only see their own participation on a shared device.
    let list = srv.get("/api/v2/devices", &t.bob).await.ok().clone();
    assert_eq!(list["shared"][0]["participants"][0]["session_id"], sid.as_str());
    assert_eq!(list["shared"][0]["participants"][0]["relation"], "friend");
    let list = srv.get("/api/v2/devices", &bob_laptop).await.ok().clone();
    assert_eq!(list["shared"][0]["participants"][0]["user_id"], t.bob_id.as_str());

    // Grants may only name the owner's devices; non-friends have none.
    srv.put(
        &grants_path,
        &t.alice,
        json!({"default": ["view"], "devices": {t.bob.id.clone(): ["view"]}}),
    )
    .await
    .err(400, "bad_request");
    srv.create_user("mallory");
    let mallory = srv.account_device("mallory", "M", false).await;
    let mallory_id = srv.get("/api/v2/me", &mallory).await.ok()["user_id"]
        .as_str()
        .unwrap()
        .to_owned();
    srv.put(
        &format!("/api/v2/grants/{mallory_id}"),
        &t.alice,
        json!({"default": ["view"]}),
    )
    .await
    .err(404, "not_found");

    // Downgrade that keeps `view`: both sides get session.permissions.
    srv.put(
        &grants_path,
        &t.alice,
        json!({"default": ["view", "annotation"]}),
    )
    .await
    .ok();
    let perm = cw.recv_type("session.permissions").await;
    assert_eq!(perm["sid"], sid.as_str());
    assert_eq!(perm["data"]["grants"], json!(["view", "annotation"]));
    let perm = t.aw.recv_type("session.permissions").await;
    assert_eq!(perm["sid"], sid.as_str());
    assert_eq!(perm["data"]["grants"], json!(["view", "annotation"]));
    t.bw
        .recv_matching("grants_changed", is_friend_change("grants_changed"))
        .await;
    let ev = t
        .bw
        .recv_matching("alice's device updated", is_shared_change("updated", t.alice.id.clone()))
        .await;
    assert_eq!(ev["data"]["device"]["granted"], json!(["view", "annotation"]));
    // Unchanged grants do not push again; revoking `view` ends the session.
    srv.put(&grants_path, &t.alice, json!({"default": ["annotation"]}))
        .await
        .ok();
    let ended = cw.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid.as_str());
    assert_eq!(ended["data"]["reason"], "permission_revoked");
    assert_eq!(cw.expect_close(&[]).await, Some(4004));
    let ended = t.aw.recv_type("session.ended").await;
    assert_eq!(ended["sid"], sid.as_str());
    assert_eq!(ended["data"]["reason"], "permission_revoked");

    // Removing the friendship ends sessions in both directions.
    srv.put(&grants_path, &t.alice, json!({"default": ["view"]}))
        .await
        .ok();
    let (mut c1, sid1, _) = srv.session_link(&t.bob, &t.alice.id, "desktop").await;
    let (mut c2, sid2, _) = srv.session_link(&t.alice, &t.bob.id, "desktop").await;
    c1.send(start_frame("s", &sid1, "a")).await;
    c1.recv_type("ack").await;
    c2.send(start_frame("s", &sid2, "a")).await;
    c2.recv_type("ack").await;
    srv.delete(&format!("/api/v2/friends/{}", t.alice_id), &t.bob)
        .await
        .ok();
    for (ws, sid) in [(&mut c1, &sid1), (&mut c2, &sid2)] {
        let ended = ws.recv_type("session.ended").await;
        assert_eq!(ended["sid"], sid.as_str());
        assert_eq!(ended["data"]["reason"], "permission_revoked");
    }
    t.srv.shutdown().await;
}

/// A host learns which friends' devices may control it, for admitting them
/// on direct connections (`docs/access-model.md` §2.2).
#[tokio::test(flavor = "multi_thread")]
async fn friend_access_lists_the_devices_that_may_control_a_host() {
    let t = two().await;
    let srv = &t.srv;
    let empty = srv.get("/api/v2/devices/self/friend-access", &t.alice).await;
    assert_eq!(empty.ok()["devices"], json!([]));
    befriend(&t).await;
    srv.put(
        &format!("/api/v2/grants/{}", t.bob_id),
        &t.alice,
        json!({"default": ["view", "control"], "devices": {}}),
    )
    .await
    .ok();
    let list = srv.get("/api/v2/devices/self/friend-access", &t.alice).await;
    let devices = list.ok()["devices"].as_array().unwrap().clone();
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["device_id"], t.bob.id.as_str());
    assert_eq!(devices[0]["public_key"], t.bob.public_key().as_str());
    assert_eq!(devices[0]["user_id"], t.bob_id.as_str());
    assert_eq!(devices[0]["grants"], json!(["view", "control"]));
    // A new friendship starts with the default grants the other way too.
    let theirs = srv.get("/api/v2/devices/self/friend-access", &t.bob).await;
    let theirs = theirs.ok()["devices"].as_array().unwrap().clone();
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0]["device_id"], t.alice.id.as_str());
    assert_eq!(theirs[0]["grants"], json!(["view", "annotation"]));
    // Sharing nothing hides the device again.
    srv.put(
        &format!("/api/v2/grants/{}", t.bob_id),
        &t.alice,
        json!({"default": [], "devices": {}}),
    )
    .await
    .ok();
    let none = srv.get("/api/v2/devices/self/friend-access", &t.alice).await;
    assert_eq!(none.ok()["devices"], json!([]));
    let _ = &t.alice_id;
}
