//! Power requests (own and friends' devices) and Wake-on-LAN routing.

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn reboot_push_and_result() {
    let srv = start().await;
    srv.create_user("alice");
    let host = srv.account_device("alice", "HOST", true).await;
    let ctrl = srv.account_device("alice", "CTRL", false).await;
    let path = format!("/api/v2/devices/{}/power", host.id);

    srv.post(&path, &ctrl, json!({"action": "reboot"}))
        .await
        .err(409, "device_offline");
    let (mut ws, _) = srv.presence(&host).await;

    let r = srv.post(&path, &ctrl, json!({"action": "reboot"})).await;
    let receipt = r.ok().clone();
    let rid = receipt["request_id"].as_str().unwrap().to_owned();
    assert!(farwing_protocol::ids::is_uuid(&rid));
    assert!(receipt.get("router_wol").is_none());
    let push = ws.recv_type("power.request").await;
    assert_eq!(
        push["data"],
        json!({
            "request_id": rid, "action": "reboot",
            "requester": {"user_id": push["data"]["requester"]["user_id"], "username": "alice", "nickname": "alice-nick"},
            "requester_device": "CTRL"
        })
    );

    // Only the target device reports the result.
    srv.post(
        &format!("/api/v2/power/{rid}/result"),
        &ctrl,
        json!({"code": 0, "message": ""}),
    )
    .await
    .err(403, "forbidden");
    srv.post(
        &format!("/api/v2/power/{rid}/result"),
        &host,
        json!({"code": 0, "message": ""}),
    )
    .await
    .ok();
    let row = {
        let rid = rid.clone();
        srv.app()
            .db
            .call(move |c| farwing_server::db::get_power_request(c, &rid))
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(row.result_code, Some(0));
    assert_eq!(row.action, "reboot");
    assert_eq!(row.requester_device_id, ctrl.id);
    srv.post(
        "/api/v2/power/00000000-0000-0000-0000-000000000000/result",
        &host,
        json!({"code": 0}),
    )
    .await
    .err(404, "not_found");

    // Shutdown push; remote power disabled by the owner -> host_disabled.
    srv.post(&path, &ctrl, json!({"action": "shutdown"}))
        .await
        .ok();
    assert_eq!(
        ws.recv_type("power.request").await["data"]["action"],
        "shutdown"
    );
    let mut policy = host_policy(true);
    policy["power"] = json!(false);
    srv.set_policy(&host, policy).await;
    srv.post(&path, &ctrl, json!({"action": "reboot"}))
        .await
        .err(409, "host_disabled");
    srv.post(&path, &ctrl, json!({"action": "explode"}))
        .await
        .err(400, "bad_request");
    srv.set_policy(&host, host_policy(true)).await;

    // Strangers do not see the device; friends need the `power` grant.
    srv.create_user("bob");
    let bob = srv.account_device("bob", "BOB", false).await;
    srv.post(&path, &bob, json!({"action": "reboot"}))
        .await
        .err(404, "device_not_found");
    let req = srv
        .post("/api/v2/friends/requests", &bob, json!({"username": "alice"}))
        .await;
    let request_id = req.ok()["sent"]["request"]["id"].as_str().unwrap().to_owned();
    srv.post(
        &format!("/api/v2/friends/requests/{request_id}/accept"),
        &host,
        serde_json::json!({"caps": ["view", "annotation"]}),
    )
    .await
    .ok();
    srv.post(&path, &bob, json!({"action": "reboot"}))
        .await
        .err(403, "forbidden");
    let bob_id = srv.get("/api/v2/me", &bob).await.ok()["user_id"]
        .as_str()
        .unwrap()
        .to_owned();
    srv.put(
        &format!("/api/v2/grants/{bob_id}"),
        &host,
        json!({"default": ["view", "power"]}),
    )
    .await
    .ok();
    let r = srv.post(&path, &bob, json!({"action": "reboot"})).await;
    let rid = r.ok()["request_id"].as_str().unwrap().to_owned();
    let push = ws.recv_type("power.request").await;
    assert_eq!(push["data"]["request_id"], rid.as_str());
    assert_eq!(push["data"]["requester"]["username"], "bob");
    assert_eq!(push["data"]["requester_device"], "BOB");
    // The target (alice's device) reports the result of bob's request.
    srv.post(
        &format!("/api/v2/power/{rid}/result"),
        &host,
        json!({"code": 1, "message": "declined"}),
    )
    .await
    .ok();
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn wake_on_lan_routing() {
    const HOME: &str = "203.0.113.10";
    const ELSEWHERE: &str = "198.51.100.20";
    let srv = start().await;
    srv.create_user("alice");

    let mut target = srv.account_device("alice", "TARGET", true).await;
    target.xff = Some(HOME.into());
    let r = srv
        .put(
            "/api/v2/devices/self/wol",
            &target,
            json!({"mac": "aa-bb-cc-dd-ee-ff", "inner_ip": "192.168.1.20", "subnet_mask": "255.255.255.0"}),
        )
        .await;
    assert_eq!(r.ok(), &json!({"supported": true}));
    let wake_path = format!("/api/v2/devices/{}/power", target.id);

    let mut requester = srv.account_device("alice", "PHONE", false).await;
    requester.xff = Some(HOME.into());
    // Not allowed by the owner yet.
    srv.post(&wake_path, &requester, json!({"action": "wake"}))
        .await
        .err(403, "forbidden");
    let mut policy = host_policy(true);
    policy["wol"] = json!(true);
    srv.set_policy(&target, policy).await;
    let list = srv.get("/api/v2/devices", &requester).await.ok().clone();
    let t = list["own"]
        .as_array()
        .unwrap()
        .iter()
        .find(|d| d["device_id"] == target.id.as_str())
        .unwrap()
        .clone();
    assert_eq!(t["wol_supported"], true);
    assert_eq!(t["policy"]["wol"], true);

    // Target online -> conflict.
    let (tws, _) = srv.presence(&target).await;
    srv.post(&wake_path, &requester, json!({"action": "wake"}))
        .await
        .err(409, "conflict");
    tws.abort();
    srv.wait_online(&target.id, false).await;

    // Helpers: one in the same network (registered there), one elsewhere,
    // one in the same network known only from its presence connection.
    let mut same = srv.account_device("alice", "SAME-LAN", true).await;
    same.xff = Some(HOME.into());
    srv.put(
        "/api/v2/devices/self/wol",
        &same,
        json!({"mac": "11:22:33:44:55:66", "inner_ip": "192.168.1.30", "subnet_mask": "255.255.255.0"}),
    )
    .await
    .ok();
    let (mut same_ws, _) = srv.presence(&same).await;
    let mut far = srv.account_device("alice", "FAR", true).await;
    far.xff = Some(ELSEWHERE.into());
    srv.put(
        "/api/v2/devices/self/wol",
        &far,
        json!({"mac": "11:22:33:44:55:77", "inner_ip": "10.0.0.5", "subnet_mask": "255.0.0.0"}),
    )
    .await
    .ok();
    let (mut far_ws, _) = srv.presence(&far).await;
    let mut lan2 = srv.account_device("alice", "LAN-2", true).await;
    lan2.xff = Some(HOME.into());
    let (mut lan2_ws, _) = srv.presence(&lan2).await;
    // Another account in the same network is never used.
    srv.create_user("mallory");
    let mut stranger = srv.account_device("mallory", "STRANGER", true).await;
    stranger.xff = Some(HOME.into());
    let (mut stranger_ws, _) = srv.presence(&stranger).await;

    let expected_target = json!({"mac": "AA:BB:CC:DD:EE:FF", "inner_ip": "192.168.1.20", "subnet_mask": "255.255.255.0"});
    // The requester has not registered its own LAN: helpers only.
    let r = srv
        .post(&wake_path, &requester, json!({"action": "wake"}))
        .await;
    let receipt = r.ok().clone();
    assert_eq!(receipt["current_device_in_same_network"], false);
    assert_eq!(receipt["target"], serde_json::Value::Null);
    assert_eq!(receipt["assist_count"], 2);
    let rid = receipt["request_id"].as_str().unwrap();
    for ws in [&mut same_ws, &mut lan2_ws] {
        let wake = ws.recv_type("wol.wake").await;
        assert_eq!(
            wake["data"],
            json!({"request_id": rid, "target": expected_target})
        );
    }
    far_ws.expect_silence(Duration::from_millis(300)).await;
    stranger_ws.expect_silence(Duration::from_millis(100)).await;

    // Registered in the target's subnet: the requester wakes it itself.
    srv.put(
        "/api/v2/devices/self/wol",
        &requester,
        json!({"mac": "11:22:33:44:55:88", "inner_ip": "192.168.1.77", "subnet_mask": "255.255.255.0"}),
    )
    .await
    .ok();
    let r = srv
        .post(&wake_path, &requester, json!({"action": "wake"}))
        .await;
    assert_eq!(r.ok()["current_device_in_same_network"], true);
    assert_eq!(r.ok()["target"], expected_target);
    assert_eq!(r.ok()["assist_count"], 2);

    // A different subnet behind the same address is not the target's LAN.
    srv.put(
        "/api/v2/devices/self/wol",
        &requester,
        json!({"mac": "11:22:33:44:55:88", "inner_ip": "192.168.2.77", "subnet_mask": "255.255.255.0"}),
    )
    .await
    .ok();
    let r = srv
        .post(&wake_path, &requester, json!({"action": "wake"}))
        .await;
    assert_eq!(r.ok()["current_device_in_same_network"], false);

    // From elsewhere: not in the same network.
    srv.put(
        "/api/v2/devices/self/wol",
        &requester,
        json!({"mac": "11:22:33:44:55:88", "inner_ip": "192.168.1.77", "subnet_mask": "255.255.255.0"}),
    )
    .await
    .ok();
    requester.xff = Some(ELSEWHERE.into());
    let r = srv
        .post(&wake_path, &requester, json!({"action": "wake"}))
        .await;
    assert_eq!(r.ok()["current_device_in_same_network"], false);
    assert_eq!(r.ok()["assist_count"], 2);

    // Invalid WoL info is reported as unsupported.
    let r = srv
        .put(
            "/api/v2/devices/self/wol",
            &far,
            json!({"mac": "nope", "inner_ip": "x", "subnet_mask": ""}),
        )
        .await;
    assert_eq!(r.ok(), &json!({"supported": false}));
    srv.shutdown().await;
}
