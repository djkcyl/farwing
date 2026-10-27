//! Notifications (`docs/notifications.md`).

mod common;

use common::*;
use serde_json::{Value, json};
use std::time::Duration;

/// The account's notifications once the background delivery stored them.
async fn notices(srv: &TestServer, dev: &Dev, wanted: usize) -> Value {
    for _ in 0..50 {
        let v = srv.get("/api/v2/me/notices", dev).await.ok().clone();
        if v["notices"].as_array().unwrap().len() >= wanted {
            return v;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    srv.get("/api/v2/me/notices", dev).await.ok().clone()
}

fn kinds(v: &Value) -> Vec<String> {
    v["notices"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| n["kind"].as_str().unwrap().to_owned())
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_friend_request_reaches_the_other_account_and_is_read() {
    let srv = start().await;
    srv.create_user("alice");
    srv.create_user("bob");
    srv.create_user("carol");
    let alice = srv.account_device("alice", "A", false).await;
    let bob = srv.account_device("bob", "B", false).await;
    let carol = srv.account_device("carol", "C", false).await;
    let (mut bob_ws, _) = srv.presence(&bob).await;

    srv.post("/api/v2/friends/requests", &alice, json!({"username": "bob"}))
        .await
        .ok();
    let pushed = bob_ws.recv_type("notice").await;
    assert_eq!(pushed["data"]["kind"], "friend.request");
    let v = notices(&srv, &bob, 1).await;
    assert_eq!(kinds(&v), ["friend.request"]);
    assert_eq!(v["unread"], 1);
    srv.post("/api/v2/me/notices/read", &bob, json!({"all": true}))
        .await
        .ok();
    assert_eq!(srv.get("/api/v2/me/notices?limit=0", &bob).await.ok()["unread"], 0);

    // Turned off: nothing more of that kind.
    let settings = srv.get("/api/v2/me/notice-settings", &bob).await.ok().clone();
    let list = settings["settings"].as_array().unwrap();
    assert!(list.iter().all(|s| !s["kind"].as_str().unwrap().starts_with("admin.")));
    assert!(list.iter().any(|s| s["kind"] == "device.offline" && s["enabled"] == false));
    srv.put(
        "/api/v2/me/notice-settings",
        &bob,
        json!({"settings": [{"kind": "friend.request", "enabled": false}]}),
    )
    .await
    .ok();
    srv.post("/api/v2/friends/requests", &carol, json!({"username": "bob"}))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(kinds(&notices(&srv, &bob, 1).await), ["friend.request"]);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_account_hears_of_its_own_connections_and_new_devices_when_it_asks() {
    let srv = start().await;
    srv.create_user("alice");
    let host = srv.account_device("alice", "HOST", true).await;
    let ctrl = srv.account_device("alice", "CTRL", false).await;
    let phone = srv.account_device("alice", "PHONE", false).await;
    let (_host_ws, _) = srv.presence(&host).await;
    let (mut phone_ws, _) = srv.presence(&phone).await;
    // The second and third devices were new to the account.
    let v = notices(&srv, &phone, 2).await;
    assert_eq!(kinds(&v), ["login.new_device", "login.new_device"]);
    assert_eq!(v["notices"][0]["data"]["device"], "PHONE");

    srv.put(
        "/api/v2/me/notice-settings",
        &phone,
        json!({"settings": [{"kind": "connection.own", "enabled": true}]}),
    )
    .await
    .ok();
    srv.post(
        "/api/v2/sessions",
        &ctrl,
        json!({"target_device_id": host.id, "purpose": "desktop", "visit": "v1"}),
    )
    .await
    .ok();
    let (pushed, _) = phone_ws.recv_until("notice").await;
    assert_eq!(pushed["data"]["kind"], "connection.own");
    assert_eq!(pushed["data"]["data"]["host"], "HOST");
    assert_eq!(pushed["data"]["data"]["controller"], "CTRL");
    // A reconnection of the same visit is not another notification.
    srv.post(
        "/api/v2/sessions",
        &ctrl,
        json!({"target_device_id": host.id, "purpose": "desktop", "visit": "v1"}),
    )
    .await
    .ok();
    tokio::time::sleep(Duration::from_millis(300)).await;
    let v = notices(&srv, &phone, 3).await;
    assert_eq!(kinds(&v), ["connection.own", "login.new_device", "login.new_device"]);
    srv.shutdown().await;
}
