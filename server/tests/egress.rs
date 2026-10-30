//! Who may use the relays (`docs/protocol/egress-policy.md` §3).

mod common;

use common::*;
use serde_json::json;
use std::time::Duration;

#[tokio::test(flavor = "multi_thread")]
async fn a_payer_without_relays_gets_discovery_only() {
    let srv = start_with(|c| {
        c.turn.credential_ttl = Duration::from_secs(3600);
        c.egress.users.relay = false;
    })
    .await;
    let (_sid, incoming, ack) = srv.ice_for_session().await;
    for (side, body) in [("host", &incoming), ("controller", &ack)] {
        assert_eq!(
            body["ice_servers"],
            json!([{"urls": ["stun:turn.test:3478"], "username": "", "credential": ""}]),
            "{side}"
        );
        assert_eq!(body["relay"], json!({"allowed": false, "reason": "relay_not_allowed"}), "{side}");
    }
    assert_eq!(incoming["forward"], true);
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn forced_relays_the_session_may_not_use_refuse_it() {
    let srv = start_with(|c| {
        c.turn.force_relay = true;
        c.egress.users.relay = false;
        c.egress.users.forward = false;
    })
    .await;
    srv.create_user("alice");
    let host = srv.account_device("alice", "HOST", true).await;
    let ctrl = srv.account_device("alice", "CTRL", false).await;
    let (_hw, _) = srv.presence(&host).await;
    let (mut cw, _sid, _) = srv.session_link(&ctrl, &host.id, "desktop").await;
    cw.send(json!({"v": V, "type": "session.start", "id": "s", "data": {
        "attempt_id": "a", "options": {}
    }}))
    .await;
    let error = cw.recv_type("error").await;
    assert_eq!(error["data"]["code"], "relay_unavailable");
    assert_eq!(error["data"]["details"]["reason"], "relay_not_allowed");
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn guests_have_their_own_policy() {
    let srv = start_with(|c| {
        c.egress.guests.forward = false;
    })
    .await;
    let (_sid, incoming, ack) = srv.ice_for_session().await;
    // An account's session: the accounts' default allows everything.
    assert_eq!(incoming["forward"], true);
    assert_eq!(ack["relay"]["allowed"], true);
    srv.shutdown().await;
}
