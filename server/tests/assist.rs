//! Remote assistance with the PAKE relayed by the server: password and
//! confirmation modes, wrong codes, refusals, cancellation, timeouts, rate
//! limits, participant limits and saved lists. The host side is simulated: it
//! reads pushes on its presence connection and answers over HTTPS.

mod common;

use common::*;
use farwing_protocol::crypto::{
    ASSIST_PAKE_SLOTS, PakeState, key_confirm, key_confirm_matches, pake_start,
};
use farwing_protocol::headers;
use serde_json::{Value, json};
use std::time::Duration;

const CODE: &str = "Fw2026test";
const CUSTOM: &str = "my-custom-pw";

struct Setup {
    srv: TestServer,
    host: Dev,
    host_ws: Ws,
    connect_id: String,
    ctrl: Dev,
}

async fn setup_with(customize: impl FnOnce(&mut farwing_server::Config)) -> Setup {
    let srv = start_with(customize).await;
    srv.create_user("helper");
    // Guest host: not logged in, accepts assistance (policy.assist).
    let host = srv.guest_device("GUEST-HOST").await;
    let (host_ws, _) = srv.presence(&host).await;
    let r = srv.post_empty("/api/v2/assist/identity", &host).await;
    let connect_id = r.ok()["connect_id"].as_str().unwrap().to_owned();
    assert!(farwing_protocol::ids::is_connect_id(&connect_id));
    assert_ne!(&connect_id[..1], "0");
    // Stable afterwards.
    let again = srv.post_empty("/api/v2/assist/identity", &host).await;
    assert_eq!(again.ok()["connect_id"], connect_id.as_str());
    let ctrl = srv.account_device("helper", "HELPER-PC", false).await;
    Setup {
        srv,
        host,
        host_ws,
        connect_id,
        ctrl,
    }
}

/// How the simulated host answers `assist.join`.
#[derive(Clone, Copy)]
struct Reply {
    allowed: bool,
    mode: &'static str,
    /// The host's codes, one per PAKE slot (random code, custom password).
    codes: &'static [&'static str],
}

const PASSWORD: Reply = Reply {
    allowed: true,
    mode: "password",
    codes: &[CODE, CUSTOM],
};
const CONFIRMATION: Reply = Reply {
    allowed: true,
    mode: "confirmation",
    codes: &[],
};

/// A `join` answered by the host.
struct Joined {
    resp: Resp,
    /// Controller PAKE states, one per slot.
    states: Vec<PakeState>,
    /// Host keys, one per host code.
    host_keys: Vec<[u8; 32]>,
    /// The `assist.join` push seen by the host.
    push: Value,
}

impl Joined {
    fn request_id(&self) -> String {
        self.push["data"]["request_id"].as_str().unwrap().to_owned()
    }

    /// The controller's local key check: the key of a slot whose host
    /// confirmation matches, if any (a wrong code matches none).
    fn controller_key(self) -> (String, Option<[u8; 32]>) {
        let rid = self.request_id();
        let v = self.resp.ok().clone();
        assert_eq!(v["request_id"], rid.as_str());
        let host_msgs: Vec<String> = strings(&v["pake"]);
        let host_kc: Vec<String> = strings(&v["key_confirm"]);
        let mut found = None;
        for ((state, msg), kc) in self.states.into_iter().zip(&host_msgs).zip(&host_kc) {
            let key = state.finish(msg).unwrap();
            if key_confirm_matches(&key, "host", &rid, kc) {
                found = Some(key);
            }
        }
        (rid, found)
    }
}

fn strings(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| a.iter().map(|s| s.as_str().unwrap().to_owned()).collect())
        .unwrap_or_default()
}

/// `POST /assist/join` from `ctrl` with `typed` as the code (none: no PAKE),
/// the host answering per `reply`.
async fn join(s: &mut Setup, ctrl: &Dev, typed: Option<&str>, reply: Reply) -> Joined {
    let (states, msgs): (Vec<PakeState>, Vec<String>) = match typed {
        Some(code) => (0..ASSIST_PAKE_SLOTS).map(|_| pake_start(code)).unzip(),
        None => (Vec::new(), Vec::new()),
    };
    let srv = &s.srv;
    let host = s.host.clone();
    let ws = &mut s.host_ws;
    let cid = s.connect_id.clone();
    let (resp, (push, host_keys)) = tokio::join!(
        srv.post(
            "/api/v2/assist/join",
            ctrl,
            json!({"connect_id": cid, "pake": msgs})
        ),
        async move {
            let push = ws.recv_type("assist.join").await;
            let rid = push["data"]["request_id"].as_str().unwrap().to_owned();
            let controller_msgs = strings(&push["data"]["pake"]);
            let (mut keys, mut pake, mut kc) = (Vec::new(), Vec::new(), Vec::new());
            if reply.allowed && !controller_msgs.is_empty() {
                for (code, msg) in reply.codes.iter().zip(&controller_msgs) {
                    let (state, answer) = pake_start(code);
                    let key = state.finish(msg).unwrap();
                    kc.push(key_confirm(&key, "host", &rid));
                    pake.push(answer);
                    keys.push(key);
                }
            }
            srv.post(
                &format!("/api/v2/assist/requests/{rid}/pake"),
                &host,
                json!({"allowed": reply.allowed, "mode": reply.mode, "pake": pake, "key_confirm": kc}),
            )
            .await
            .ok();
            (push, keys)
        }
    );
    Joined {
        resp,
        states,
        host_keys,
        push,
    }
}

/// The host's decision on `assist.accept`: verify the controller's key
/// confirmation against its keys (password modes) or accept (confirmation).
fn verify_decision(push: &Value, keys: &[[u8; 32]], rid: &str) -> Value {
    if keys.is_empty() {
        return json!({"allowed": true});
    }
    let kc = push["data"]["key_confirm"].as_str().unwrap_or("");
    if keys
        .iter()
        .any(|k| key_confirm_matches(k, "controller", rid, kc))
    {
        json!({"allowed": true})
    } else {
        json!({"allowed": false, "refusal": "code_invalid"})
    }
}

/// `POST /assist/requests/{rid}/accept` with `confirm`, the host deciding
/// with `decide(push)`.
async fn accept(
    s: &mut Setup,
    ctrl: &Dev,
    rid: &str,
    confirm: &str,
    decide: impl FnOnce(&Value) -> Value,
) -> (Resp, Value) {
    let srv = &s.srv;
    let host = s.host.clone();
    let ws = &mut s.host_ws;
    let path = format!("/api/v2/assist/requests/{rid}/accept");
    tokio::join!(
        srv.post(&path, ctrl, json!({"key_confirm": confirm})),
        async move {
            let push = ws.recv_type("assist.accept").await;
            assert_eq!(push["data"]["request_id"], rid);
            let decision = decide(&push);
            srv.post(
                &format!("/api/v2/assist/requests/{rid}/decision"),
                &host,
                decision,
            )
            .await
            .ok();
            push
        }
    )
}

/// The full password flow for `typed`; returns the accept response.
async fn password_flow(s: &mut Setup, ctrl: &Dev, typed: &str) -> Resp {
    let joined = join(s, ctrl, Some(typed), PASSWORD).await;
    let keys = joined.host_keys.clone();
    let (rid, key) = joined.controller_key();
    let confirm = key
        .map(|k| key_confirm(&k, "controller", &rid))
        .unwrap_or_else(|| "bm90LWEta2V5".to_owned());
    let r = rid.clone();
    accept(s, ctrl, &rid, &confirm, move |push| {
        verify_decision(push, &keys, &r)
    })
    .await
    .0
}

/// Check an `AssistJoin` and run the realtime start of the assistance session.
async fn check_join_and_start(s: &mut Setup, join: &Value) {
    assert_eq!(join["host"]["device_id"], s.host.id.as_str());
    assert_eq!(join["host"]["alias"], "GUEST-HOST");
    assert_eq!(join["host"]["platform"], "windows");
    assert_eq!(join["host"]["version"], "0.1.0");
    assert_eq!(join["host"]["public_key"], s.host.public_key().as_str());
    assert_eq!(join["target"]["device_id"], s.host.id.as_str());
    assert_eq!(join["target"]["relation"], "assist");
    assert_eq!(join["target"]["granted"], json!([]));
    let sid = join["session_id"].as_str().unwrap();
    let mut cw = s
        .srv
        .ws_connect(headers::TICKET, join["ticket"].as_str().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(cw.recv_type("hello").await["data"]["session_id"], sid);
    cw.send(start_frame("s", sid, "a1")).await;
    let (incoming, _) = s.host_ws.recv_until("session.incoming").await;
    assert_eq!(incoming["sid"], sid);
    let d = &incoming["data"];
    assert_eq!(d["relation"], "assist");
    assert_eq!(d["purpose"], "desktop");
    assert_eq!(d["grants"], json!([]));
    assert_eq!(d["controller"]["username"], "helper");
    assert_eq!(d["controller"]["public_key"], s.ctrl.public_key().as_str());
    let ack = cw.recv_type("ack").await;
    assert_eq!(ack["data"]["relation"], "assist");
    assert_eq!(ack["data"]["host"]["public_key"], s.host.public_key().as_str());
    cw.send(json!({"v": V, "type": "bye"})).await;
    assert_eq!(cw.expect_close(&[]).await, Some(1000));
    let ended = s.host_ws.recv_type("session.ended").await;
    assert_eq!(ended["data"]["reason"], "controller_left");
}

#[tokio::test(flavor = "multi_thread")]
async fn password_mode_with_pake() {
    let mut s = setup_with(|_| {}).await;
    let ctrl = s.ctrl.clone();

    // Correct random code: the host's confirmation matches, the controller
    // confirms back, the host allows.
    let joined = join(&mut s, &ctrl, Some(CODE), PASSWORD).await;
    let push = &joined.push["data"];
    // Before admission the host learns the visitor's device and the name it
    // goes by, not its account.
    assert_eq!(push["controller"]["username"], "");
    assert_eq!(push["controller"]["user_id"], "");
    assert_eq!(push["controller"]["nickname"], "helper-nick");
    assert_eq!(push["controller"]["device_id"], ctrl.id.as_str());
    assert_eq!(push["controller"]["public_key"], ctrl.public_key().as_str());
    assert_eq!(push["pake"].as_array().unwrap().len(), ASSIST_PAKE_SLOTS);
    let pending = joined.resp.ok().clone();
    assert_eq!(pending["mode"], "password");
    assert_eq!(pending["pake"].as_array().unwrap().len(), 2);
    assert_eq!(pending["key_confirm"].as_array().unwrap().len(), 2);
    // Nothing about the host before it admits the controller.
    assert!(pending.get("host").is_none());
    let keys = joined.host_keys.clone();
    let (rid, key) = joined.controller_key();
    let key = key.expect("the right code matches a host confirmation");
    let r = rid.clone();
    let (resp, accept_push) = accept(
        &mut s,
        &ctrl,
        &rid,
        &key_confirm(&key, "controller", &rid),
        move |push| verify_decision(push, &keys, &r),
    )
    .await;
    assert!(!accept_push["data"]["key_confirm"].as_str().unwrap().is_empty());
    let join_body = resp.ok().clone();
    check_join_and_start(&mut s, &join_body).await;
    // A request is used once.
    s.srv
        .post(
            &format!("/api/v2/assist/requests/{rid}/accept"),
            &ctrl,
            json!({"key_confirm": ""}),
        )
        .await
        .err(404, "not_found");

    // The custom password (second slot) works too.
    password_flow(&mut s, &ctrl, CUSTOM).await.ok();

    // Wrong code: the controller detects the mismatch locally and cancels.
    let joined = join(&mut s, &ctrl, Some("wrong-code-1"), PASSWORD).await;
    let (rid, key) = joined.controller_key();
    assert!(key.is_none());
    s.srv
        .post_empty(&format!("/api/v2/assist/requests/{rid}/cancel"), &ctrl)
        .await
        .ok();
    let cancelled = s.host_ws.recv_type("assist.cancelled").await;
    assert_eq!(cancelled["data"]["request_id"], rid.as_str());
    // A controller that accepts anyway is refused by the host.
    password_flow(&mut s, &ctrl, "wrong-code-2")
        .await
        .err(403, "assist_code_invalid");

    // The target is recorded in the controller's recent list.
    let recent = s
        .srv
        .get("/api/v2/assist/saved/recent", &ctrl)
        .await
        .ok()
        .clone();
    assert_eq!(recent["saved"][0]["connect_id"], s.connect_id.as_str());
    assert_eq!(
        recent["saved"][0]["publisher_device_id"],
        s.host.id.as_str()
    );
    // The device key, shown as its fingerprint.
    assert_eq!(
        recent["saved"][0]["public_key"],
        s.host.public_key().as_str()
    );
    assert!(recent["saved"][0]["last_connected_at"].as_i64().unwrap() > 0);
    s.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn confirmation_mode_refusals_and_missing_code() {
    let mut s = setup_with(|_| {}).await;
    let ctrl = s.ctrl.clone();

    // Confirmation mode: no PAKE, the host user decides.
    let joined = join(&mut s, &ctrl, None, CONFIRMATION).await;
    let pending = joined.resp.ok().clone();
    assert_eq!(pending["mode"], "confirmation");
    assert_eq!(pending["pake"], json!([]));
    assert_eq!(pending["key_confirm"], json!([]));
    assert_eq!(joined.push["data"]["pake"], json!([]));
    let rid = joined.request_id();
    let (resp, push) = accept(&mut s, &ctrl, &rid, "", |_| json!({"allowed": true})).await;
    assert_eq!(push["data"]["key_confirm"], "");
    let join_body = resp.ok().clone();
    check_join_and_start(&mut s, &join_body).await;

    // Refusals map to error codes.
    for (decision, status, code) in [
        (json!({"allowed": false, "refusal": "rejected"}), 403, "assist_rejected"),
        (json!({"allowed": false}), 403, "assist_rejected"),
        (json!({"allowed": false, "refusal": "unavailable"}), 409, "assist_unavailable"),
        (json!({"allowed": false, "refusal": "code_required"}), 409, "assist_code_required"),
        (json!({"allowed": false, "refusal": "code_invalid"}), 403, "assist_code_invalid"),
    ] {
        let joined = join(&mut s, &ctrl, None, CONFIRMATION).await;
        let rid = joined.request_id();
        joined.resp.ok();
        let (resp, _) = accept(&mut s, &ctrl, &rid, "", move |_| decision).await;
        resp.err(status, code);
        if code == "assist_code_required" {
            assert_eq!(resp.body["error"]["details"]["mode"], "confirmation");
        }
    }

    // A password mode without a code: refused before the controller accepts,
    // and the host is told to forget the request.
    for mode in ["password", "password_confirmation"] {
        let reply = Reply {
            allowed: true,
            mode,
            codes: &[CODE],
        };
        let joined = join(&mut s, &ctrl, None, reply).await;
        joined.resp.err(409, "assist_code_required");
        assert_eq!(joined.resp.body["error"]["details"]["mode"], mode);
        let cancelled = s.host_ws.recv_type("assist.cancelled").await;
        assert_eq!(cancelled["data"]["request_id"], joined.request_id().as_str());
    }

    // The host is not accepting assistance right now.
    let off = Reply {
        allowed: false,
        mode: "password",
        codes: &[],
    };
    join(&mut s, &ctrl, Some(CODE), off)
        .await
        .resp
        .err(409, "assist_unavailable");
    s.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn cancel_timeouts_and_request_ownership() {
    let mut s = setup_with(|c| {
        c.limits.assist_host_reply_ms = 300;
        c.limits.assist_decision_wait_ms = 500;
        c.limits.assist_failures_per_minute = 100;
    })
    .await;
    let ctrl = s.ctrl.clone();

    // Cancel while the host is deciding: the long poll ends, the host is told.
    let joined = join(&mut s, &ctrl, None, CONFIRMATION).await;
    let rid = joined.request_id();
    joined.resp.ok();
    {
        let srv = &s.srv;
        let ws = &mut s.host_ws;
        let path = format!("/api/v2/assist/requests/{rid}/accept");
        let (resp, _) = tokio::join!(
            srv.post(&path, &ctrl, json!({"key_confirm": ""})),
            async {
                ws.recv_type("assist.accept").await;
                srv.post_empty(&format!("/api/v2/assist/requests/{rid}/cancel"), &ctrl)
                    .await
                    .ok();
                let cancelled = ws.recv_type("assist.cancelled").await;
                assert_eq!(cancelled["data"]["request_id"], rid.as_str());
            }
        );
        resp.err(409, "conflict");
    }

    // The host never decides: timeout, and the host is told.
    let joined = join(&mut s, &ctrl, None, CONFIRMATION).await;
    let rid = joined.request_id();
    joined.resp.ok();
    s.srv
        .post(
            &format!("/api/v2/assist/requests/{rid}/accept"),
            &ctrl,
            json!({"key_confirm": ""}),
        )
        .await
        .err(408, "timeout");
    s.host_ws.recv_type("assist.accept").await;
    let cancelled = s.host_ws.recv_type("assist.cancelled").await;
    assert_eq!(cancelled["data"]["request_id"], rid.as_str());
    // A late decision is refused.
    s.srv
        .post(
            &format!("/api/v2/assist/requests/{rid}/decision"),
            &s.host,
            json!({"allowed": true}),
        )
        .await
        .err(404, "not_found");

    // The host never answers the PAKE: timeout.
    s.srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": s.connect_id, "pake": []}),
        )
        .await
        .err(408, "timeout");
    s.host_ws.recv_type("assist.join").await;

    // Only the target host answers; only the requesting controller accepts.
    let other_host = s.srv.guest_device("OTHER").await;
    s.srv.create_user("second");
    let other_ctrl = s.srv.account_device("second", "SECOND", false).await;
    {
        let srv = &s.srv;
        let host = s.host.clone();
        let ws = &mut s.host_ws;
        let cid = s.connect_id.clone();
        let (resp, _) = tokio::join!(
            srv.post(
                "/api/v2/assist/join",
                &ctrl,
                json!({"connect_id": cid, "pake": []})
            ),
            async {
                let push = ws.recv_type("assist.join").await;
                let rid = push["data"]["request_id"].as_str().unwrap().to_owned();
                let reply = json!({"allowed": true, "mode": "confirmation"});
                srv.post(&format!("/api/v2/assist/requests/{rid}/pake"), &other_host, reply.clone())
                    .await
                    .err(404, "not_found");
                srv.post(&format!("/api/v2/assist/requests/{rid}/decision"), &host, json!({"allowed": true}))
                    .await
                    .err(409, "conflict");
                srv.post(&format!("/api/v2/assist/requests/{rid}/pake"), &host, reply.clone())
                    .await
                    .ok();
                // Answered already (404) or waiting for the controller (409).
                let again = srv
                    .post(&format!("/api/v2/assist/requests/{rid}/pake"), &host, reply)
                    .await;
                assert!(matches!(again.status.as_u16(), 404 | 409), "{}", again.body);
            }
        );
        let rid = resp.ok()["request_id"].as_str().unwrap().to_owned();
        srv.post(
            &format!("/api/v2/assist/requests/{rid}/accept"),
            &other_ctrl,
            json!({"key_confirm": ""}),
        )
        .await
        .err(404, "not_found");
        // Another controller cannot cancel it either (and learns nothing).
        srv.post_empty(&format!("/api/v2/assist/requests/{rid}/cancel"), &other_ctrl)
            .await
            .ok();
        srv.post_empty(&format!("/api/v2/assist/requests/{rid}/cancel"), &ctrl)
            .await
            .ok();
        let cancelled = s.host_ws.recv_type("assist.cancelled").await;
        assert_eq!(cancelled["data"]["request_id"], rid.as_str());
    }
    s.srv
        .post(
            "/api/v2/assist/requests/00000000-0000-0000-0000-000000000000/accept",
            &ctrl,
            json!({"key_confirm": ""}),
        )
        .await
        .err(404, "not_found");

    // Malformed requests, unknown and offline hosts, logged-out controllers.
    s.srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": s.connect_id, "pake": ["a", "b", "c"]}),
        )
        .await
        .err(400, "bad_request");
    s.srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": "999999999", "pake": []}),
        )
        .await
        .err(404, "device_not_found");
    s.srv
        .post("/api/v2/assist/join", &ctrl, json!({"connect_id": "12ab"}))
        .await
        .err(400, "bad_request");
    let offline_id = s
        .srv
        .post_empty("/api/v2/assist/identity", &other_host)
        .await
        .ok()["connect_id"]
        .as_str()
        .unwrap()
        .to_owned();
    s.srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": offline_id}),
        )
        .await
        .err(409, "device_offline");
    // A device without an account may ask too (`docs/access-model.md`
    // §2.5); this host never answers, so the request times out.
    s.srv
        .post(
            "/api/v2/assist/join",
            &other_host,
            json!({"connect_id": s.connect_id}),
        )
        .await
        .err(408, "timeout");
    let asked = s.host_ws.recv_type("assist.join").await;
    assert_eq!(asked["data"]["controller"]["user_id"], "");
    // Addressing by device ID needs exactly one target.
    s.srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": s.connect_id, "device_id": "abcdefghijklmnop"}),
        )
        .await
        .err(400, "bad_request");
    // A host whose owner turned assistance off is not even asked.
    let mut policy = host_policy(true);
    policy["assist"] = json!(false);
    s.srv.set_policy(&s.host, policy).await;
    s.srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": s.connect_id, "pake": []}),
        )
        .await
        .err(409, "assist_unavailable");
    s.host_ws.expect_silence(Duration::from_millis(200)).await;
    s.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn rate_limit_counts_attempts_without_a_session() {
    let mut s = setup_with(|c| c.limits.assist_failures_per_minute = 2).await;
    let ctrl = s.ctrl.clone();
    // A successful assistance does not count.
    password_flow(&mut s, &ctrl, CODE).await.ok();
    // Two attempts that never reach a session: a refused wrong code and an
    // abandoned request (wrong code detected by the controller).
    password_flow(&mut s, &ctrl, "000000")
        .await
        .err(403, "assist_code_invalid");
    // A visitor that was not let in learns nothing about the device it
    // tried; the device it was let in to keeps its name, not its account.
    let records = s.srv.get("/api/v2/records", &ctrl).await.ok().clone();
    let records = records.as_array().unwrap();
    let refused = records
        .iter()
        .find(|r| r["outcome"] == "code_invalid")
        .expect("the refused attempt");
    assert_eq!(refused["host"]["device_id"], "");
    assert_eq!(refused["host"]["name"], "");
    let admitted = records
        .iter()
        .find(|r| r["outcome"] != "code_invalid")
        .expect("the admitted visit");
    assert_eq!(admitted["host"]["device_id"], s.host.id.as_str());
    assert_eq!(admitted["host"]["user_id"], "");
    let joined = join(&mut s, &ctrl, Some("111111"), PASSWORD).await;
    assert!(joined.controller_key().1.is_none());
    // The next attempt is refused before the host is asked.
    let r = s
        .srv
        .post(
            "/api/v2/assist/join",
            &ctrl,
            json!({"connect_id": s.connect_id, "pake": []}),
        )
        .await;
    r.err(429, "rate_limited");
    assert!(
        r.body["error"]["details"]["retry_after_ms"]
            .as_u64()
            .unwrap()
            > 0
    );
    s.host_ws.expect_silence(Duration::from_millis(200)).await;
    // Another account still has its own budget.
    s.srv.create_user("second");
    let other = s.srv.account_device("second", "SECOND", false).await;
    password_flow(&mut s, &other, CODE).await.ok();
    s.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn assist_never_displaces_and_respects_participant_limit() {
    let mut s = setup_with(|_| {}).await;
    let ctrl = s.ctrl.clone();
    let mut policy = host_policy(true);
    policy["max_participants"] = json!(1);
    s.srv.set_policy(&s.host, policy).await;
    let first = password_flow(&mut s, &ctrl, CODE).await.ok().clone();
    let mut cw = s
        .srv
        .ws_connect(headers::TICKET, first["ticket"].as_str().unwrap(), None)
        .await
        .unwrap();
    cw.recv_type("hello").await;
    // A second helper account passes the code check but the host is full.
    s.srv.create_user("second");
    let other = s.srv.account_device("second", "SECOND", false).await;
    let joined = join(&mut s, &other, Some(CODE), PASSWORD).await;
    let keys = joined.host_keys.clone();
    let (rid, key) = joined.controller_key();
    let confirm = key_confirm(&key.unwrap(), "controller", &rid);
    let r = rid.clone();
    let (resp, _) = accept(&mut s, &other, &rid, &confirm, move |push| {
        verify_decision(push, &keys, &r)
    })
    .await;
    resp.err(409, "busy");
    let cancelled = s.host_ws.recv_type("assist.cancelled").await;
    assert_eq!(cancelled["data"]["request_id"], rid.as_str());
    cw.send(json!({"v": V, "type": "ping", "data": {"t": 7}}))
        .await;
    assert_eq!(cw.recv_type("pong").await["data"]["t"], 7);

    // With room for two, both helpers are connected at once.
    s.srv.set_policy(&s.host, host_policy(true)).await;
    let second = password_flow(&mut s, &other, CODE).await.ok().clone();
    assert_ne!(second["session_id"], first["session_id"]);
    cw.send(json!({"v": V, "type": "ping", "data": {"t": 8}}))
        .await;
    assert_eq!(cw.recv_type("pong").await["data"]["t"], 8);
    let participants = s.srv.app().hub.participants(&s.host.id);
    assert_eq!(participants.len(), 2);
    assert!(
        participants
            .iter()
            .all(|p| p.relation == farwing_protocol::api::Relation::Assist)
    );
    s.srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_lists() {
    let mut s = setup_with(|_| {}).await;
    let ctrl = s.ctrl.clone();
    // Favorites.
    let r = s
        .srv
        .put(
            "/api/v2/assist/saved/favorites",
            &ctrl,
            json!({"favorites": [
                {"connect_id": s.connect_id, "remark": "Mom", "favorited_at": 1760000000},
                {"connect_id": "123456789", "remark": "Unknown", "favorited_at": 1760000001}
            ]}),
        )
        .await;
    let saved = r.ok()["saved"].as_array().unwrap().clone();
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[0]["connect_id"], "123456789");
    assert_eq!(saved[1]["remark"], "Mom");
    assert_eq!(saved[1]["is_favorite"], true);
    // Nothing about the device behind a short number before a connection.
    for item in &saved {
        assert_eq!(item["publisher_device_id"], "");
        assert_eq!(item["public_key"], "");
    }
    s.srv
        .delete("/api/v2/assist/saved/favorites/123456789", &ctrl)
        .await
        .ok();
    let fav = s
        .srv
        .get("/api/v2/assist/saved/favorites", &ctrl)
        .await
        .ok()
        .clone();
    assert_eq!(fav["saved"].as_array().unwrap().len(), 1);
    // A successful connection binds the favourite to its device.
    password_flow(&mut s, &ctrl, CODE).await.ok();
    let fav = s
        .srv
        .get("/api/v2/assist/saved/favorites", &ctrl)
        .await
        .ok()
        .clone();
    assert_eq!(fav["saved"][0]["publisher_device_id"], s.host.id.as_str());
    assert_eq!(fav["saved"][0]["public_key"], s.host.public_key().as_str());

    // Recent: populated by a session, then cleared.
    password_flow(&mut s, &ctrl, CODE).await.ok();
    let recent = s
        .srv
        .get("/api/v2/assist/saved/recent", &ctrl)
        .await
        .ok()
        .clone();
    assert_eq!(recent["saved"].as_array().unwrap().len(), 1);
    s.srv
        .delete(
            &format!("/api/v2/assist/saved/recent/{}", s.host.id),
            &ctrl,
        )
        .await
        .ok();
    let recent = s
        .srv
        .get("/api/v2/assist/saved/recent", &ctrl)
        .await
        .ok()
        .clone();
    assert!(recent["saved"].as_array().unwrap().is_empty());
    password_flow(&mut s, &ctrl, CODE).await.ok();
    s.srv
        .delete("/api/v2/assist/saved/recent", &ctrl)
        .await
        .ok();
    let recent = s
        .srv
        .get("/api/v2/assist/saved/recent", &ctrl)
        .await
        .ok()
        .clone();
    assert!(recent["saved"].as_array().unwrap().is_empty());
    s.srv
        .get("/api/v2/assist/saved/other", &ctrl)
        .await
        .err(404, "not_found");
    s.srv.shutdown().await;
}

/// A device without an account asks a host by its device ID; the host
/// trusts it, so no code and no confirmation (`docs/access-model.md` §2.2).
#[tokio::test(flavor = "multi_thread")]
async fn trusted_guest_by_device_id() {
    let mut s = setup_with(|_| {}).await;
    let guest = s.srv.guest_device("GUEST-CTRL").await;
    let host_id = s.host.id.clone();
    // The device ID is the key's.
    assert_eq!(
        farwing_protocol::crypto::local_device_id(&s.host.public_key()).as_deref(),
        Some(host_id.as_str())
    );
    let srv = &s.srv;
    let host = s.host.clone();
    let ws = &mut s.host_ws;
    let (resp, push) = tokio::join!(
        srv.post("/api/v2/assist/join", &guest, json!({"device_id": host_id})),
        async move {
            let push = ws.recv_type("assist.join").await;
            let rid = push["data"]["request_id"].as_str().unwrap().to_owned();
            srv.post(
                &format!("/api/v2/assist/requests/{rid}/pake"),
                &host,
                json!({"allowed": true, "mode": "password", "trusted": true}),
            )
            .await
            .ok();
            push
        }
    );
    // No account: no user, but the device and its key.
    assert_eq!(push["data"]["controller"]["user_id"], "");
    assert_eq!(push["data"]["controller"]["public_key"], guest.public_key().as_str());
    let pending = resp.ok().clone();
    assert_eq!(pending["trusted"], true);
    let rid = pending["request_id"].as_str().unwrap().to_owned();
    let srv = &s.srv;
    let host = s.host.clone();
    let ws = &mut s.host_ws;
    let path = format!("/api/v2/assist/requests/{rid}/accept");
    let (joined, _) = tokio::join!(srv.post(&path, &guest, json!({})), async move {
        ws.recv_type("assist.accept").await;
        srv.post(
            &format!("/api/v2/assist/requests/{rid}/decision"),
            &host,
            json!({"allowed": true, "trusted": true}),
        )
        .await
        .ok();
    });
    let joined = joined.ok().clone();
    assert_eq!(joined["target"]["relation"], "trusted");
    let sid = joined["session_id"].as_str().unwrap();
    let mut cw = s
        .srv
        .ws_connect(headers::TICKET, joined["ticket"].as_str().unwrap(), None)
        .await
        .unwrap();
    assert_eq!(cw.recv_type("hello").await["data"]["session_id"], sid);
    cw.send(start_frame("s", sid, "a1")).await;
    let (incoming, _) = s.host_ws.recv_until("session.incoming").await;
    assert_eq!(incoming["data"]["relation"], "trusted");
    assert_eq!(incoming["data"]["controller"]["device_id"], guest.id.as_str());
    // A guest has no recent list to record into.
    s.srv
        .get("/api/v2/assist/saved/recent", &guest)
        .await
        .err(401, "unauthorized");
}
