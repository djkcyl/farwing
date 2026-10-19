//! TURN REST credentials and the builtin TURN server.

mod common;

use base64::Engine;
use common::*;
use farwing_server::config::TurnMode;
use hmac::{Hmac, Mac};
use serde_json::json;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpStream, UdpSocket};
use turn::client::{Client, ClientConfig};
use webrtc_util::Conn;

fn hmac_b64(secret: &str, username: &str) -> String {
    let mut mac = Hmac::<sha1::Sha1>::new_from_slice(secret.as_bytes()).unwrap();
    mac.update(username.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(mac.finalize().into_bytes())
}

#[tokio::test(flavor = "multi_thread")]
async fn external_turn_rest_credentials() {
    let srv = start_with(|c| {
        c.turn.credential_ttl = Duration::from_secs(3600);
        c.turn.force_relay = true;
    })
    .await;
    let (sid, incoming, ack) = srv.ice_for_session().await;
    for (side, body) in [("host", &incoming), ("controller", &ack)] {
        assert_eq!(body["force_relay"], true, "{side}");
        let ice = body["ice_servers"].as_array().unwrap();
        assert_eq!(ice.len(), 2, "{side}");
        assert_eq!(ice[0]["urls"], json!(["stun:turn.test:3478"]));
        assert_eq!(ice[0]["username"], "");
        assert_eq!(ice[0]["credential"], "");
        assert_eq!(
            ice[1]["urls"],
            json!([
                "turn:turn.test:3478?transport=udp",
                "turn:turn.test:3478?transport=tcp"
            ])
        );
        let username = ice[1]["username"].as_str().unwrap();
        let (expiry, rest) = username.split_once(':').unwrap();
        assert_eq!(rest, sid);
        let expiry: i64 = expiry.parse().unwrap();
        let now = farwing_server::db::now_s();
        assert!((expiry - now - 3600).abs() <= 5, "{expiry} vs {now}");
        assert_eq!(
            ice[1]["credential"].as_str().unwrap(),
            hmac_b64("test-shared-secret", username),
            "{side}: coturn use-auth-secret compatible"
        );
    }
    let info = srv
        .send(srv.raw(reqwest::Method::GET, "/api/v2/server"))
        .await;
    assert_eq!(info.ok()["features"]["force_relay"], true);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stun_only_mode() {
    let srv = start_with(|c| {
        c.turn.mode = TurnMode::None;
        c.turn.urls = vec!["stun:stun.test:3478".into()];
        c.turn.shared_secret.clear();
    })
    .await;
    let info = srv
        .send(srv.raw(reqwest::Method::GET, "/api/v2/server"))
        .await;
    assert_eq!(info.ok()["features"]["turn"], false);
    let (_sid, incoming, ack) = srv.ice_for_session().await;
    for body in [&incoming, &ack] {
        assert_eq!(
            body["ice_servers"],
            json!([{"urls": ["stun:stun.test:3478"], "username": "", "credential": ""}])
        );
    }
    srv.shutdown().await;
}

async fn builtin_server() -> (TestServer, SocketAddr, SocketAddr) {
    let srv = start_with(|c| {
        c.turn.mode = TurnMode::Builtin;
        c.turn.urls.clear();
        c.turn.shared_secret = "builtin-secret".into();
        c.turn.listen_udp = Some("127.0.0.1:0".parse().unwrap());
        c.turn.listen_tcp = Some("127.0.0.1:0".parse().unwrap());
        c.turn.public_ip = Some("127.0.0.1".parse().unwrap());
        c.turn.relay_bind = "127.0.0.1".parse().unwrap();
    })
    .await;
    let s = srv.server.as_ref().unwrap();
    let udp = s.turn_udp.unwrap();
    let tcp = s.turn_tcp.unwrap();
    (srv, udp, tcp)
}

async fn turn_client(
    conn: Arc<dyn Conn + Send + Sync>,
    server: SocketAddr,
    username: &str,
    password: &str,
) -> Client {
    let client = Client::new(ClientConfig {
        stun_serv_addr: server.to_string(),
        turn_serv_addr: server.to_string(),
        username: username.to_owned(),
        password: password.to_owned(),
        realm: "farwing".to_owned(),
        software: String::new(),
        rto_in_ms: 0,
        conn,
        vnet: None,
    })
    .await
    .unwrap();
    client.listen().await.unwrap();
    client
}

/// Allocate, then exchange datagrams with a peer through the relay.
async fn allocate_and_relay(client: &Client) {
    let relay = tokio::time::timeout(Duration::from_secs(10), client.allocate())
        .await
        .expect("allocation in time")
        .expect("allocation succeeds");
    let relay_addr = relay.local_addr().unwrap();
    assert_eq!(relay_addr.ip().to_string(), "127.0.0.1");
    let peer = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let peer_addr = peer.local_addr().unwrap();
    // Relay -> peer (installs the permission).
    relay.send_to(b"hello peer", peer_addr).await.unwrap();
    let mut buf = [0u8; 1500];
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), peer.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"hello peer");
    assert_eq!(from, relay_addr);
    // Peer -> relay -> client.
    peer.send_to(b"hello client", relay_addr).await.unwrap();
    let (n, from) = tokio::time::timeout(Duration::from_secs(5), relay.recv_from(&mut buf))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&buf[..n], b"hello client");
    assert_eq!(from, peer_addr);
    relay.close().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_turn_udp_allocation() {
    let (srv, udp, _tcp) = builtin_server().await;
    // Credentials as issued to a session.
    let (sid, incoming, _ack) = srv.ice_for_session().await;
    let ice = incoming["ice_servers"].as_array().unwrap();
    assert!(
        ice.iter()
            .flat_map(|s| s["urls"].as_array().unwrap())
            .any(|u| u.as_str().unwrap().starts_with("turn:127.0.0.1:")),
        "{ice:?}"
    );
    let turn_entry = ice.iter().find(|s| s["username"] != "").unwrap();
    let username = turn_entry["username"].as_str().unwrap();
    assert!(username.ends_with(&sid));
    let password = turn_entry["credential"].as_str().unwrap();

    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client = turn_client(sock, udp, username, password).await;
    let mapped = client.send_binding_request().await.unwrap();
    assert_eq!(mapped.ip().to_string(), "127.0.0.1");
    allocate_and_relay(&client).await;
    client.close().await.ok();

    // Expired and forged credentials are refused.
    let expired = format!("{}:{sid}", farwing_server::db::now_s() - 10);
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client = turn_client(sock, udp, &expired, &hmac_b64("builtin-secret", &expired)).await;
    let res = tokio::time::timeout(Duration::from_secs(15), client.allocate()).await;
    assert!(matches!(res, Ok(Err(_))), "expired credential must fail");
    client.close().await.ok();
    let sock = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let client = turn_client(sock, udp, username, "forged").await;
    let res = tokio::time::timeout(Duration::from_secs(15), client.allocate()).await;
    assert!(matches!(res, Ok(Err(_))), "wrong password must fail");
    client.close().await.ok();
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn builtin_turn_tcp_allocation() {
    let (srv, _udp, tcp) = builtin_server().await;
    let username = format!("{}:tcp-test", farwing_server::db::now_s() + 600);
    let password = hmac_b64("builtin-secret", &username);
    let stream = TcpStream::connect(tcp).await.unwrap();
    let conn = farwing_server::turn_server::TcpTurnConn::from_stream(stream).unwrap();
    let client = turn_client(conn, tcp, &username, &password).await;
    allocate_and_relay(&client).await;
    client.close().await.ok();
    srv.shutdown().await;
}
