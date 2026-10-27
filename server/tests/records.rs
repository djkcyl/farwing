//! Connection records and account events (`docs/connection-records.md`).

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;

/// The record of a visit appears once the persisted session reaches the
/// database.
async fn records(srv: &TestServer, dev: &Dev, query: &str) -> Vec<serde_json::Value> {
    for _ in 0..50 {
        let v = srv.get(&format!("/api/v2/records{query}"), dev).await.ok().clone();
        let list = v.as_array().unwrap().clone();
        if !list.is_empty() {
            return list;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Vec::new()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_visit_is_one_record_with_both_sides_reports() {
    let srv = start().await;
    srv.create_user("alice");
    srv.create_user("bob");
    let host = srv.account_device("alice", "HOST", true).await;
    let ctrl = srv.account_device("alice", "CTRL", false).await;
    let other = srv.account_device("bob", "OTHER", false).await;
    let (_host_ws, _) = srv.presence(&host).await;
    let open = |visit: &str| json!({"target_device_id": host.id, "purpose": "desktop", "visit": visit});

    // A reconnection under the same visit replaces the session, not the record.
    let s1 = srv.post("/api/v2/sessions", &ctrl, open("v1")).await.ok()["session_id"].clone();
    let s2 = srv.post("/api/v2/sessions", &ctrl, open("v1")).await.ok()["session_id"].clone();
    assert_ne!(s1, s2);
    tokio::time::sleep(Duration::from_millis(300)).await;
    let list = records(&srv, &ctrl, "").await;
    assert_eq!(list.len(), 1, "{list:?}");
    let r = &list[0];
    assert_eq!(r["outcome"], "open");
    assert_eq!(r["reconnects"], 1);
    assert_eq!(r["relation"], "own");
    assert_eq!(r["purpose"], "desktop");
    assert_eq!(r["host"]["device_id"], host.id.as_str());
    assert_eq!(r["host"]["name"], "HOST");
    assert_eq!(r["controller"]["device_id"], ctrl.id.as_str());
    assert_eq!(r["controller"]["username"], "alice");
    assert_eq!(r["ip"], "127.0.0.1");

    // Each side adds its summary; another account is neither side.
    let report = |viewer: serde_json::Value, host_part: serde_json::Value| {
        json!({
            "visit": "v1",
            "host_device_id": host.id,
            "controller_device_id": ctrl.id,
            "viewer": viewer,
            "host": host_part,
        })
    };
    srv.post(
        "/api/v2/records/reports",
        &ctrl,
        report(json!({"route": "p2p", "fps": 60.0, "stalls": 2}), json!(null)),
    )
    .await
    .ok();
    srv.post(
        "/api/v2/records/reports",
        &host,
        report(
            json!(null),
            json!({"controlled": true, "files": [{"name": "a.txt", "count": 1, "bytes": 5, "upload": true, "outcome": "done"}]}),
        ),
    )
    .await
    .ok();
    srv.post("/api/v2/records/reports", &other, report(json!({}), json!(null)))
        .await
        .err(403, "forbidden");
    let r = &records(&srv, &host, &format!("?device={}", host.id)).await[0];
    assert_eq!(r["viewer"]["route"], "p2p");
    assert_eq!(r["viewer"]["stalls"], 2);
    assert_eq!(r["host_report"]["controlled"], true);
    assert_eq!(r["host_report"]["files"][0]["name"], "a.txt");
    assert!(srv.get("/api/v2/records", &other).await.ok().as_array().unwrap().is_empty());
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_account_sees_its_own_events() {
    let srv = start().await;
    srv.create_user("alice");
    let dev = srv.account_device("alice", "PC", false).await;
    srv.call(
        reqwest::Method::PATCH,
        &format!("/api/v2/devices/{}", dev.id),
        &dev,
        Some(json!({"alias": "Laptop"})),
    )
    .await
    .ok();
    srv.post(
        "/api/v2/devices/self/events",
        &dev,
        json!({"kind": "trusted.add", "device_id": "abcdefghijklmnop", "device_name": "Tablet"}),
    )
    .await
    .ok();
    srv.post("/api/v2/devices/self/events", &dev, json!({"kind": "screen.watch"}))
        .await
        .err(400, "bad_request");
    let events = srv.get("/api/v2/me/events", &dev).await.ok().clone();
    let kinds: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds[..2], ["trusted.add", "device.rename"], "{events}");
    assert_eq!(events[0]["detail"]["other"], "Tablet");
    assert_eq!(events[1]["detail"]["device"], "Laptop");
    // The account's own actions name nobody else.
    assert_eq!(events[1]["actor"], "");
    srv.shutdown().await;
}
