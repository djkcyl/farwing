//! Edge relay nodes: join, configuration, health, per-side relays with the
//! node's own credentials, round-trip reports, disabling and removal.

mod common;

use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::UdpSocket;
use turn::client::{Client, ClientConfig};

async fn nodes(srv: &TestServer, token: &str) -> Value {
    srv.admin_call(Method::GET, "/api/v2/admin/nodes", token, None)
        .await
        .ok()
        .clone()
}

/// Wait until node `id` reports a running relay.
async fn wait_usable(srv: &TestServer, token: &str, id: &str, usable: bool) -> Value {
    for _ in 0..150 {
        let list = nodes(srv, token).await;
        let node = list
            .as_array()
            .unwrap()
            .iter()
            .find(|n| n["id"] == id)
            .cloned()
            .unwrap_or(Value::Null);
        if node["usable"] == usable {
            return node;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("node {id} usable != {usable}: {}", nodes(srv, token).await);
}

#[tokio::test(flavor = "multi_thread")]
async fn an_edge_node_joins_and_relays() {
    let srv = start().await;
    let token = srv.admin_token("admin");
    let created = srv
        .admin_call(
            Method::POST,
            "/api/v2/admin/nodes",
            &token,
            Some(json!({"name": "Edge A", "region": "local"})),
        )
        .await;
    let id = created.ok()["node"]["id"].as_str().unwrap().to_owned();
    let link = created.ok()["link"].as_str().unwrap().to_owned();
    assert!(created.ok()["command"].as_str().unwrap().starts_with("farwing-server edge --join "));
    assert!(link.starts_with(&format!("https://127.0.0.1:{}/edge/fwj_", srv.port)), "{link}");
    // Loopback ports for the test.
    srv.admin_call(
        Method::PATCH,
        &format!("/api/v2/admin/nodes/{id}"),
        &token,
        Some(json!({"settings": {
            "public_ip": "127.0.0.1",
            "listen_udp": "127.0.0.1:0",
            "listen_tcp": "127.0.0.1:0",
            "listen_tls": "127.0.0.1:0",
            "stun": [{"listen": "127.0.0.1:0", "public": null}],
        }})),
    )
    .await
    .ok();

    let dir = tempfile::tempdir().unwrap();
    let node_dir = dir.path().to_path_buf();
    let node = tokio::spawn(farwing_server::edge_node::run(node_dir.clone(), Some(link.clone())));
    let status = wait_usable(&srv, &token, &id, true).await;
    assert_eq!(status["online"], true);
    assert!(status["joined_at"].is_number());
    let udp = status["health"]["turn_udp"].as_str().unwrap().to_owned();
    // The join link works once.
    let again = tokio::spawn(farwing_server::edge_node::run(
        dir.path().join("second"),
        Some(link),
    ));
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(
        nodes(&srv, &token).await.as_array().unwrap().len(),
        1,
        "no second node"
    );
    again.abort();

    // Both sides get the node with credentials signed for it.
    let (sid, incoming, ack) = srv.ice_for_session().await;
    for body in [&incoming, &ack] {
        let servers = body["ice_servers"].as_array().unwrap();
        let entry = servers
            .iter()
            .find(|s| s["username"].as_str().unwrap().ends_with(&format!(":{sid}:{id}")))
            .unwrap_or_else(|| panic!("no node relay in {servers:?}"));
        assert_eq!(entry["urls"][0], format!("turn:{udp}?transport=udp"));
    }
    let info = srv.send(srv.raw(Method::GET, "/api/v2/server")).await;
    let stun = status["health"]["stun"][0].as_str().unwrap();
    assert!(
        info.ok()["features"]["nat_discovery"]
            .as_array()
            .unwrap()
            .contains(&json!(format!("stun:{stun}"))),
        "{}",
        info.ok()
    );

    // The node allocates with them, and only with them.
    let servers = incoming["ice_servers"].as_array().unwrap();
    let entry = servers
        .iter()
        .find(|s| s["username"].as_str().unwrap().ends_with(&format!(":{id}")))
        .unwrap();
    let relay: std::net::SocketAddr = udp.parse().unwrap();
    let client = turn_client(
        relay,
        entry["username"].as_str().unwrap(),
        entry["credential"].as_str().unwrap(),
    )
    .await;
    let allocation = tokio::time::timeout(Duration::from_secs(10), client.allocate())
        .await
        .expect("allocation in time")
        .expect("allocation with the node's credentials");
    // The node reports the session as relayed.
    let mut reported = false;
    for _ in 0..150 {
        let list = nodes(&srv, &token).await;
        if list[0]["health"]["sessions"]
            .as_array()
            .is_some_and(|s| s.contains(&json!(sid)))
        {
            reported = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert!(reported, "the node reports its allocation's session");
    drop(allocation);
    client.close().await.ok();
    let forged = format!("{}:{sid}", farwing_server::db::now_s() + 600);
    let client = turn_client(relay, &forged, entry["credential"].as_str().unwrap()).await;
    let res = tokio::time::timeout(Duration::from_secs(15), client.allocate()).await;
    assert!(matches!(res, Ok(Err(_))), "control credentials are not the node's");
    client.close().await.ok();

    // Disabled: no longer handed out.
    srv.admin_call(
        Method::PATCH,
        &format!("/api/v2/admin/nodes/{id}"),
        &token,
        Some(json!({"enabled": false})),
    )
    .await
    .ok();
    wait_usable(&srv, &token, &id, false).await;
    let (_, incoming, _) = srv.ice_for_session().await;
    assert!(
        !incoming["ice_servers"].to_string().contains(&id),
        "a disabled node is not handed out"
    );

    // Removed: its identity is refused.
    srv.admin_call(Method::DELETE, &format!("/api/v2/admin/nodes/{id}"), &token, None)
        .await
        .ok();
    assert_eq!(nodes(&srv, &token).await, json!([]));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!node.is_finished(), "the node keeps retrying");
    node.abort();
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn devices_report_round_trips_and_get_the_nearest_nodes() {
    let srv = start().await;
    let token = srv.admin_token("admin");
    let hub = Arc::clone(&srv.app().edges);
    // Two joined nodes, as if connected and healthy.
    let mut ids = Vec::new();
    for name in ["North", "South"] {
        let r = srv
            .admin_call(Method::POST, "/api/v2/admin/nodes", &token, Some(json!({"name": name})))
            .await;
        ids.push(r.ok()["node"]["id"].as_str().unwrap().to_owned());
    }
    for (i, id) in ids.iter().enumerate() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        std::mem::forget(rx);
        hub.attach(id, "c", format!("203.0.113.{}", i + 1).parse().unwrap(), tx);
        hub.report(
            id,
            "c",
            farwing_server::edge::Health {
                turn_udp: Some("0.0.0.0:3478".parse().unwrap()),
                stun: vec!["0.0.0.0:3479".parse().unwrap()],
                ..Default::default()
            },
        );
    }
    srv.create_user("alice");
    let host = srv.account_device("alice", "HOST", true).await;
    let (mut ws, hello) = srv.presence(&host).await;
    let probes = hello["data"]["probe_nodes"].as_array().unwrap().clone();
    assert_eq!(probes.len(), 2);
    let north = probes.iter().find(|p| p["id"] == ids[0].as_str()).unwrap();
    assert_eq!(north["stun"], "203.0.113.1:3479");
    // South is nearer to this device.
    ws.send(json!({"v": V, "type": "nodes.rtt", "data": {"rtt": {
        ids[0].clone(): 90, ids[1].clone(): 15,
    }}}))
    .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    let servers = hub.servers_for("s", &host.id);
    assert!(servers[0].username.ends_with(&ids[1]), "{servers:?}");
    assert!(servers[1].username.ends_with(&ids[0]));
    srv.shutdown().await;
}

async fn turn_client(server: std::net::SocketAddr, username: &str, password: &str) -> Client {
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client = Client::new(ClientConfig {
        transport: Default::default(),
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username: username.to_owned(),
        password: password.to_owned(),
        realm: "farwing".to_owned(),
        software: String::new(),
        rto_in_ms: 0,
        conn: sock,
        vnet: None,
    })
    .await
    .unwrap();
    client.listen().await.unwrap();
    client
}
