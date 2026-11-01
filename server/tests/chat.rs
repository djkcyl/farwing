//! Chat (`docs/protocol/chat-design.md` §14.1): permissions, messages,
//! direct messages, search, attachments, joining, dissolving, limits and
//! the chat connection's pushes.

mod common;

use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::time::Duration;

const VIEW: i64 = 1 << 0;
const SEND: i64 = 1 << 1;
const ATTACH: i64 = 1 << 2;
const MANAGE_MESSAGES: i64 = 1 << 4;
const READ_HISTORY: i64 = 1 << 5;
const KICK: i64 = 1 << 17;
const MANAGE_ROLES: i64 = 1 << 21;
const EVERYONE: i64 = VIEW | SEND | ATTACH | READ_HISTORY | (1 << 8) | (1 << 9) | (1 << 10) | (1 << 16);

struct Folk {
    srv: TestServer,
    alice: Dev,
    alice_id: String,
    bob: Dev,
    bob_id: String,
    carol: Dev,
    carol_id: String,
}

async fn folk_with(customize: impl FnOnce(&mut farwing_server::Config)) -> Folk {
    let srv = start_with(customize).await;
    let alice_id = srv.create_user("alice");
    let bob_id = srv.create_user("bob");
    let carol_id = srv.create_user("carol");
    let alice = srv.account_device("alice", "ALICE-PC", false).await;
    let bob = srv.account_device("bob", "BOB-PC", false).await;
    let carol = srv.account_device("carol", "CAROL-PC", false).await;
    Folk {
        srv,
        alice,
        alice_id,
        bob,
        bob_id,
        carol,
        carol_id,
    }
}

async fn folk() -> Folk {
    folk_with(|_| {}).await
}

fn s(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())
}

async fn make_group(srv: &TestServer, dev: &Dev, name: &str) -> Value {
    srv.post(
        "/api/v2/chat/groups",
        dev,
        json!({"name": name, "text_channel": "综合", "voice_channel": "语音", "category": "文字频道", "admin_role": "管理员"}),
    )
    .await
    .ok()
    .clone()
}

fn channel(state: &Value, kind: &str) -> String {
    s(&state["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == kind)
        .unwrap_or_else(|| panic!("no {kind} channel in {state}"))["id"])
}

fn channel_named(state: &Value, name: &str) -> Option<Value> {
    state["channels"].as_array().unwrap().iter().find(|c| c["name"] == name).cloned()
}

/// `member` joins `group` with an invitation from `owner`.
async fn invite_in(srv: &TestServer, owner: &Dev, member: &Dev, group: &str) -> Value {
    let inv = srv.post(&format!("/api/v2/chat/groups/{group}/invites"), owner, json!({})).await;
    let code = s(&inv.ok()["code"]);
    let r = srv.post_empty(&format!("/api/v2/chat/invites/{code}/join"), member).await;
    r.ok()["joined"].clone()
}

async fn send(srv: &TestServer, dev: &Dev, channel: &str, content: &str) -> Value {
    srv.post(
        &format!("/api/v2/chat/channels/{channel}/messages"),
        dev,
        json!({"content": content}),
    )
    .await
    .ok()
    .clone()
}

async fn group_state(srv: &TestServer, dev: &Dev, group: &str) -> Value {
    srv.get(&format!("/api/v2/chat/groups/{group}"), dev).await.ok().clone()
}

async fn befriend(srv: &TestServer, a: &Dev, b: &Dev, b_name: &str) {
    let r = srv.post("/api/v2/friends/requests", a, json!({"username": b_name})).await;
    let id = s(&r.ok()["sent"]["request"]["id"]);
    srv.post(&format!("/api/v2/friends/requests/{id}/accept"), b, json!({"caps": ["view"]}))
        .await
        .ok();
}

/// Open the chat connection and say `chat.hello`; the snapshot.
async fn chat_ws(srv: &TestServer, dev: &Dev) -> (Ws, Value) {
    let r = srv.post_empty("/api/v2/chat/ticket", dev).await;
    let ticket = s(&r.ok()["ticket"]);
    let mut ws = srv
        .ws_connect(farwing_protocol::headers::TICKET, &ticket, None)
        .await
        .expect("chat connects");
    ws.recv_type("hello").await;
    ws.send(json!({"v": V, "type": "chat.hello", "id": "h1", "data": {"idle": false}})).await;
    let ack = ws.recv_matching("snapshot", |v| v["type"] == "ack" && v["re"] == "h1").await;
    (ws, ack["data"].clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn groups_roles_overwrites_mutes_and_freezing() {
    let f = folk().await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    assert_eq!(state["group"]["owner_id"], f.alice_id.as_str());
    assert_eq!(state["roles"].as_array().unwrap().len(), 2);
    assert_eq!(state["channels"].as_array().unwrap().len(), 3);
    let general = channel(&state, "text");
    assert!(channel_named(&state, "综合").is_some());

    let bob_state = invite_in(srv, &f.alice, &f.bob, &group).await;
    assert_eq!(bob_state["group"]["permissions"].as_i64(), Some(EVERYONE));
    assert_eq!(bob_state["group"]["member_count"], 2);
    invite_in(srv, &f.alice, &f.carol, &group).await;

    // Bob may not manage channels.
    srv.post("/api/v2/chat/channels", &f.bob, json!({"group_id": group, "kind": "text", "name": "x"}))
        .await
        .err_reason(403, "forbidden", "chat_forbidden");

    // A hidden channel: gone for bob until his role may see it.
    let secret = srv
        .post("/api/v2/chat/channels", &f.alice, json!({"group_id": group, "kind": "text", "name": "secret"}))
        .await;
    let secret = s(&secret.ok()["id"]);
    srv.put(
        &format!("/api/v2/chat/channels/{secret}/overwrites/{group}"),
        &f.alice,
        json!({"allow": 0, "deny": VIEW}),
    )
    .await
    .ok();
    assert!(channel_named(&group_state(srv, &f.bob, &group).await, "secret").is_none());
    srv.post(&format!("/api/v2/chat/channels/{secret}/messages"), &f.bob, json!({"content": "hi"}))
        .await
        .err(404, "not_found");
    let role = srv
        .post(
            &format!("/api/v2/chat/groups/{group}/roles"),
            &f.alice,
            json!({"name": "mod", "permissions": MANAGE_MESSAGES | KICK}),
        )
        .await;
    let role = s(&role.ok()["id"]);
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/members/{}", f.bob_id),
        &f.alice,
        Some(json!({"roles": [role]})),
    )
    .await
    .ok();
    srv.put(
        &format!("/api/v2/chat/channels/{secret}/overwrites/{role}"),
        &f.alice,
        json!({"allow": VIEW, "deny": 0}),
    )
    .await
    .ok();
    let bob_state = group_state(srv, &f.bob, &group).await;
    assert!(channel_named(&bob_state, "secret").is_some());
    assert!(channel_named(&group_state(srv, &f.carol, &group).await, "secret").is_none());
    send(srv, &f.bob, &secret, "now I see").await;

    // A category's overwrite reaches its channels.
    let category = channel(&state, "category");
    srv.put(
        &format!("/api/v2/chat/channels/{category}/overwrites/{group}"),
        &f.alice,
        json!({"allow": 0, "deny": SEND}),
    )
    .await
    .ok();
    srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.carol, json!({"content": "x"}))
        .await
        .err_reason(403, "forbidden", "chat_forbidden");
    srv.put(
        &format!("/api/v2/chat/channels/{category}/overwrites/{group}"),
        &f.alice,
        json!({"allow": 0, "deny": 0}),
    )
    .await
    .ok();
    send(srv, &f.carol, &general, "back").await;

    // Ranks: bob (mod) kicks carol, but neither the owner nor above himself.
    srv.delete(&format!("/api/v2/chat/groups/{group}/members/{}", f.alice_id), &f.bob)
        .await
        .err_reason(409, "conflict", "chat_owner");
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/members/{}", f.carol_id),
        &f.bob,
        Some(json!({"roles": [role]})),
    )
    .await
    .err_reason(403, "forbidden", "chat_forbidden");
    let admin_role = s(&state["roles"].as_array().unwrap().iter().find(|r| r["name"] == "管理员").unwrap()["id"]);
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/roles/{admin_role}"),
        &f.alice,
        Some(json!({"permissions": MANAGE_ROLES})),
    )
    .await
    .ok();
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/roles/{role}"),
        &f.alice,
        Some(json!({"permissions": MANAGE_MESSAGES | KICK | MANAGE_ROLES})),
    )
    .await
    .ok();
    // The admin role ranks above mod: bob cannot hand it out.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/members/{}", f.carol_id),
        &f.bob,
        Some(json!({"roles": [admin_role]})),
    )
    .await
    .err_reason(403, "forbidden", "chat_role_rank");
    srv.delete(&format!("/api/v2/chat/groups/{group}/members/{}", f.carol_id), &f.bob)
        .await
        .ok();
    assert_eq!(group_state(srv, &f.alice, &group).await["group"]["member_count"], 2);

    // Muted: reading yes, sending no.
    let until = farwing_server::db::now_s() + 600;
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/members/{}", f.bob_id),
        &f.alice,
        Some(json!({"muted_until": until})),
    )
    .await
    .ok();
    let r = srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.bob, json!({"content": "x"})).await;
    r.err_reason(403, "forbidden", "chat_muted");
    assert_eq!(r.body["error"]["details"]["until"], until);
    srv.get(&format!("/api/v2/chat/channels/{general}/messages"), &f.bob).await.ok();
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}/members/{}", f.bob_id),
        &f.alice,
        Some(json!({"muted_until": null})),
    )
    .await
    .ok();
    send(srv, &f.bob, &general, "unmuted").await;

    // Frozen by the server's administrator: read-only for everyone.
    let token = srv.admin_token("admin");
    srv.admin_call(Method::PATCH, &format!("/api/v2/admin/chat/groups/{group}"), &token, Some(json!({"frozen": true})))
        .await
        .ok();
    srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.alice, json!({"content": "x"}))
        .await
        .err_reason(403, "forbidden", "chat_frozen");
    let frozen = group_state(srv, &f.alice, &group).await;
    assert_eq!(frozen["group"]["frozen"], true);
    assert_eq!(frozen["group"]["permissions"].as_i64(), Some(VIEW | READ_HISTORY));
    srv.admin_call(Method::PATCH, &format!("/api/v2/admin/chat/groups/{group}"), &token, Some(json!({"frozen": false})))
        .await
        .ok();
    send(srv, &f.alice, &general, "thawed").await;
    let groups = srv.admin_call(Method::GET, "/api/v2/admin/chat/groups", &token, None).await;
    let listed = &groups.ok()[0];
    assert_eq!(listed["members"], 2);
    assert_eq!(listed["channels"], 3);
}

#[tokio::test(flavor = "multi_thread")]
async fn messages_pages_edits_pins_mentions_and_reads() {
    let f = folk_with(|c| c.chat.rate_messages = 100).await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    let general = channel(&state, "text");
    invite_in(srv, &f.alice, &f.bob, &group).await;

    let mut ids = Vec::new();
    for i in 0..120 {
        let m = srv
            .post(
                &format!("/api/v2/chat/channels/{general}/messages"),
                if i % 2 == 0 { &f.alice } else { &f.bob },
                json!({"content": format!("message {i}"), "nonce": format!("n{i}")}),
            )
            .await;
        assert_eq!(m.ok()["nonce"], format!("n{i}"));
        ids.push(s(&m.ok()["id"]));
    }
    // The same nonce again: the same message.
    let again = srv
        .post(
            &format!("/api/v2/chat/channels/{general}/messages"),
            &f.alice,
            json!({"content": "message 118", "nonce": "n118"}),
        )
        .await;
    assert_eq!(s(&again.ok()["id"]), ids[118]);

    let latest = srv.get(&format!("/api/v2/chat/channels/{general}/messages?limit=50"), &f.bob).await;
    let latest = latest.ok()["messages"].as_array().unwrap().clone();
    assert_eq!(latest.len(), 50);
    assert_eq!(latest[49]["content"], "message 119");
    assert_eq!(latest[0]["content"], "message 70");
    let older = srv
        .get(&format!("/api/v2/chat/channels/{general}/messages?before={}&limit=100", ids[70]), &f.bob)
        .await;
    let older = older.ok()["messages"].as_array().unwrap().clone();
    assert_eq!(older.len(), 70);
    assert_eq!(older[0]["content"], "message 0");
    let around = srv
        .get(&format!("/api/v2/chat/channels/{general}/messages?around={}&limit=10", ids[60]), &f.bob)
        .await;
    let around = around.ok()["messages"].as_array().unwrap().clone();
    assert_eq!(around.len(), 10);
    assert!(around.iter().any(|m| s(&m["id"]) == ids[60]));
    let newer = srv
        .get(&format!("/api/v2/chat/channels/{general}/messages?after={}", ids[115]), &f.bob)
        .await;
    assert_eq!(newer.ok()["messages"].as_array().unwrap().len(), 4);

    // Edit (author only), reply, delete, pins.
    srv.call(Method::PATCH, &format!("/api/v2/chat/messages/{}", ids[0]), &f.bob, Some(json!({"content": "no"})))
        .await
        .err_reason(403, "forbidden", "chat_forbidden");
    let edited = srv
        .call(Method::PATCH, &format!("/api/v2/chat/messages/{}", ids[0]), &f.alice, Some(json!({"content": "first!"})))
        .await;
    assert!(edited.ok()["edited_at"].is_i64());
    let reply = srv
        .post(
            &format!("/api/v2/chat/channels/{general}/messages"),
            &f.bob,
            json!({"content": "agreed", "reply_to": ids[0], "mention_reply": true}),
        )
        .await;
    let reply = reply.ok().clone();
    assert_eq!(reply["reply"]["content"], "first!");
    assert_eq!(reply["mentions"]["users"][0], f.alice_id.as_str());
    srv.put(&format!("/api/v2/chat/messages/{}/pin", ids[0]), &f.bob, json!({}))
        .await
        .err_reason(403, "forbidden", "chat_forbidden");
    srv.put(&format!("/api/v2/chat/messages/{}/pin", ids[0]), &f.alice, json!({})).await.ok();
    let pins = srv.get(&format!("/api/v2/chat/channels/{general}/pins"), &f.bob).await;
    assert_eq!(pins.ok()["messages"][0]["content"], "first!");
    srv.delete(&format!("/api/v2/chat/messages/{}", ids[0]), &f.bob)
        .await
        .err_reason(403, "forbidden", "chat_forbidden");
    srv.delete(&format!("/api/v2/chat/messages/{}", ids[0]), &f.alice).await.ok();
    let gone = srv
        .get(&format!("/api/v2/chat/channels/{general}/messages?around={}&limit=2", ids[0]), &f.bob)
        .await;
    let first = &gone.ok()["messages"][0];
    assert_eq!(first["deleted"], true);
    assert_eq!(first["content"], "");
    assert_eq!(srv.get(&format!("/api/v2/chat/channels/{general}/pins"), &f.bob).await.ok()["messages"], json!([]));
    let page = srv
        .get(&format!("/api/v2/chat/channels/{general}/messages?after={}&limit=100", ids[119]), &f.bob)
        .await;
    assert_eq!(page.ok()["messages"][0]["reply"]["deleted"], true);
    // Empty and too long.
    srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.bob, json!({"content": "   "}))
        .await
        .err_reason(400, "bad_request", "chat_content_invalid");
    srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.bob, json!({"content": "字".repeat(4001)}))
        .await
        .err_reason(400, "bad_request", "chat_content_invalid");

    // Mentions count until read; reading never goes back.
    send(srv, &f.bob, &general, &format!("hey <@{}>", f.alice_id)).await;
    let st = srv.get("/api/v2/chat/state", &f.alice).await;
    let rs = st.ok()["read_states"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["channel_id"] == general.as_str())
        .unwrap()
        .clone();
    assert_eq!(rs["mentions"], 2);
    let last = s(&group_state(srv, &f.alice, &group).await["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "text")
        .unwrap()["last_message_id"]);
    let read = srv
        .post(&format!("/api/v2/chat/channels/{general}/read"), &f.alice, json!({"message_id": last}))
        .await;
    assert_eq!(read.ok()["mentions"], 0);
    let back = srv
        .post(&format!("/api/v2/chat/channels/{general}/read"), &f.alice, json!({"message_id": ids[1]}))
        .await;
    assert_eq!(s(&back.ok()["last_read_id"]), last);
}

#[tokio::test(flavor = "multi_thread")]
async fn direct_messages_follow_the_friendship() {
    let f = folk().await;
    let srv = &f.srv;
    srv.post("/api/v2/chat/dm", &f.alice, json!({"user": f.bob_id}))
        .await
        .err_reason(403, "forbidden", "chat_not_friends");
    befriend(srv, &f.alice, &f.bob, "bob").await;
    let dm = srv.post("/api/v2/chat/dm", &f.alice, json!({"user": f.bob_id})).await;
    let dm = dm.ok().clone();
    assert_eq!(dm["friends"], true);
    assert_eq!(dm["user"]["username"], "bob");
    let ch = s(&dm["channel"]["id"]);
    let again = srv.post("/api/v2/chat/dm", &f.bob, json!({"user": f.alice_id})).await;
    assert_eq!(s(&again.ok()["channel"]["id"]), ch);
    send(srv, &f.alice, &ch, "hello bob").await;
    let m = send(srv, &f.bob, &ch, "hi").await;
    // Both may pin; carol sees nothing.
    srv.put(&format!("/api/v2/chat/messages/{}/pin", s(&m["id"])), &f.alice, json!({})).await.ok();
    srv.get(&format!("/api/v2/chat/channels/{ch}/messages"), &f.carol).await.err(404, "not_found");
    // Read receipts: alice reads, bob's connection hears it.
    let (mut bw, snap) = chat_ws(srv, &f.bob).await;
    assert_eq!(snap["dms"][0]["channel"]["id"], ch.as_str());
    srv.post(&format!("/api/v2/chat/channels/{ch}/read"), &f.alice, json!({"message_id": s(&m["id"])}))
        .await
        .ok();
    let r = bw.recv_matching("read receipt", |v| v["type"] == "chat.read").await;
    assert_eq!(r["data"]["user_id"], f.alice_id.as_str());
    // No longer friends: read-only.
    srv.delete(&format!("/api/v2/friends/{}", f.bob_id), &f.alice).await.ok();
    let pushed = bw.recv_matching("dm change", |v| v["type"] == "chat.dm").await;
    assert_eq!(pushed["data"]["friends"], false);
    srv.post(&format!("/api/v2/chat/channels/{ch}/messages"), &f.bob, json!({"content": "?"}))
        .await
        .err_reason(403, "forbidden", "chat_not_friends");
    let history = srv.get(&format!("/api/v2/chat/channels/{ch}/messages"), &f.bob).await;
    assert_eq!(history.ok()["messages"].as_array().unwrap().len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn search_respects_words_authors_and_visibility() {
    let f = folk_with(|c| c.chat.rate_messages = 100).await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    let general = channel(&state, "text");
    invite_in(srv, &f.alice, &f.bob, &group).await;
    let secret = srv
        .post("/api/v2/chat/channels", &f.alice, json!({"group_id": group, "kind": "text", "name": "secret"}))
        .await;
    let secret = s(&secret.ok()["id"]);
    srv.put(&format!("/api/v2/chat/channels/{secret}/overwrites/{group}"), &f.alice, json!({"allow": 0, "deny": VIEW}))
        .await
        .ok();
    send(srv, &f.alice, &general, "The quick brown fox").await;
    send(srv, &f.bob, &general, "远程桌面的画质很好").await;
    send(srv, &f.bob, &general, "好").await;
    send(srv, &f.alice, &secret, "quick secret plans").await;

    let find = |q: &str, dev: &Dev| {
        let path = format!("/api/v2/chat/search?q={}", urlencode(q));
        let dev = dev.clone();
        async move { srv.get(&path, &dev).await.ok()["messages"].as_array().unwrap().clone() }
    };
    assert_eq!(find("quick", &f.alice).await.len(), 2);
    let bob_quick = find("quick", &f.bob).await;
    assert_eq!(bob_quick.len(), 1);
    assert_eq!(bob_quick[0]["content"], "The quick brown fox");
    assert_eq!(find("QUICK BROWN", &f.bob).await.len(), 1);
    assert_eq!(find("桌面的画质", &f.bob).await.len(), 1);
    // One and two characters: scanned.
    assert_eq!(find("好", &f.bob).await.len(), 2);
    assert_eq!(find("画质", &f.bob).await.len(), 1);
    assert!(find("nothing here", &f.bob).await.is_empty());
    let by_bob = srv.get(&format!("/api/v2/chat/search?author={}", f.bob_id), &f.alice).await;
    assert_eq!(by_bob.ok()["messages"].as_array().unwrap().len(), 2);
    srv.get(&format!("/api/v2/chat/search?q=quick&channel={secret}"), &f.bob).await.err(404, "not_found");
    // Edits and withdrawals follow into the index.
    let m = send(srv, &f.bob, &general, "searchable words").await;
    assert_eq!(find("searchable", &f.alice).await.len(), 1);
    srv.call(Method::PATCH, &format!("/api/v2/chat/messages/{}", s(&m["id"])), &f.bob, Some(json!({"content": "renamed"})))
        .await
        .ok();
    assert!(find("searchable", &f.alice).await.is_empty());
    assert_eq!(find("renamed", &f.alice).await.len(), 1);
    srv.delete(&format!("/api/v2/chat/messages/{}", s(&m["id"])), &f.bob).await.ok();
    assert!(find("renamed", &f.alice).await.is_empty());
    // carol, outside the group, finds nothing.
    assert!(find("quick", &f.carol).await.is_empty());
}

fn urlencode(text: &str) -> String {
    text.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                char::from(b).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

fn png(w: u32, h: u32) -> Vec<u8> {
    let img = image::RgbaImage::from_pixel(w, h, image::Rgba([200, 30, 30, 255]));
    let mut out = Vec::new();
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .unwrap();
    out
}

#[tokio::test(flavor = "multi_thread")]
async fn attachments_upload_download_quota_and_cleanup() {
    let f = folk_with(|c| {
        c.chat.max_attachment_mb = 1;
        c.chat.group_quota_gb = 1;
    })
    .await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    let general = channel(&state, "text");
    invite_in(srv, &f.alice, &f.bob, &group).await;

    let bytes = png(1200, 800);
    let up = srv
        .post(
            "/api/v2/chat/attachments",
            &f.alice,
            json!({"channel_id": general, "name": "../photo.png", "mime": "image/png", "size": bytes.len()}),
        )
        .await;
    let up = up.ok().clone();
    let aid = s(&up["id"]);
    // Wrong token, then too many bytes, then right.
    let r = srv.http.put(srv.url(&format!("/media/upload/{aid}?t=nope"))).body(bytes.clone()).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let mut more = bytes.clone();
    more.push(0);
    let r = srv.http.put(srv.url(&s(&up["upload"]))).body(more).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 413);
    let r = srv.http.put(srv.url(&s(&up["upload"]))).body(bytes.clone()).send().await.unwrap();
    assert!(r.status().is_success(), "{}", r.status());
    let done: Value = r.json().await.unwrap();
    assert_eq!((done["width"].as_u64(), done["height"].as_u64(), done["thumb"].as_bool()), (Some(1200), Some(800), Some(true)));
    assert_eq!(done["name"], "photo.png");

    let m = srv
        .post(
            &format!("/api/v2/chat/channels/{general}/messages"),
            &f.alice,
            json!({"content": "", "attachments": [aid]}),
        )
        .await;
    assert_eq!(m.ok()["attachments"][0]["id"], aid.as_str());
    // Used once only.
    srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.alice, json!({"content": "x", "attachments": [aid]}))
        .await
        .err(400, "bad_request");

    let token = |dev: &Dev| {
        let dev = dev.clone();
        async move { s(&srv.post_empty("/api/v2/chat/media-token", &dev).await.ok()["token"]) }
    };
    let bob_token = token(&f.bob).await;
    let r = srv
        .http
        .get(srv.url(&format!("/media/attachments/{aid}/photo.png?t={bob_token}")))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    assert_eq!(r.headers()["x-content-type-options"], "nosniff");
    assert_eq!(r.bytes().await.unwrap().as_ref(), bytes.as_slice());
    let r = srv
        .http
        .get(srv.url(&format!("/media/attachments/{aid}/photo.png?t={bob_token}")))
        .header("range", "bytes=0-9")
        .send()
        .await
        .unwrap();
    assert_eq!(r.status().as_u16(), 206);
    assert_eq!(r.bytes().await.unwrap().len(), 10);
    let thumb = srv.http.get(srv.url(&format!("/media/thumbs/{aid}?t={bob_token}"))).send().await.unwrap();
    let t = image::load_from_memory(&thumb.bytes().await.unwrap()).unwrap();
    assert_eq!((t.width(), t.height()), (400, 267));
    // Outsiders and missing tokens see nothing.
    let carol_token = token(&f.carol).await;
    let r = srv.http.get(srv.url(&format!("/media/attachments/{aid}/x?t={carol_token}"))).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 404);
    let r = srv.http.get(srv.url(&format!("/media/attachments/{aid}/x"))).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 401);

    // Over the size limit, then over the group's space.
    srv.post(
        "/api/v2/chat/attachments",
        &f.alice,
        json!({"channel_id": general, "name": "big.bin", "mime": "application/zip", "size": 2 * 1024 * 1024}),
    )
    .await
    .err_reason(413, "payload_too_large", "chat_too_large");
    let token_admin = srv.admin_token("admin");
    srv.admin_call(
        Method::PATCH,
        &format!("/api/v2/admin/chat/groups/{group}"),
        &token_admin,
        Some(json!({"quota_bytes": bytes.len() + 10})),
    )
    .await
    .ok();
    srv.post(
        "/api/v2/chat/attachments",
        &f.bob,
        json!({"channel_id": general, "name": "a.txt", "mime": "text/plain", "size": 100}),
    )
    .await
    .err_reason(403, "forbidden", "chat_quota");
    srv.admin_call(Method::PATCH, &format!("/api/v2/admin/chat/groups/{group}"), &token_admin, Some(json!({"quota_bytes": null})))
        .await
        .ok();

    // An upload never sent is gone after an hour.
    let up = srv
        .post(
            "/api/v2/chat/attachments",
            &f.bob,
            json!({"channel_id": general, "name": "a.txt", "mime": "text/plain", "size": 5}),
        )
        .await;
    let up = up.ok().clone();
    let r = srv.http.put(srv.url(&s(&up["upload"]))).body("hello").send().await.unwrap();
    assert!(r.status().is_success());
    let path = srv.cfg.data_dir.join("attachments").join(&group).join(s(&up["id"]));
    assert!(path.is_file());
    let c = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
    c.execute("UPDATE chat_attachments SET created_at = created_at - 7200 WHERE id = ?1", [s(&up["id"])]).unwrap();
    farwing_server::chat::upkeep(srv.app(), false).await;
    assert!(!path.exists());
    // Withdrawing a message removes its files.
    let sent = srv.cfg.data_dir.join("attachments").join(&group).join(&aid);
    assert!(sent.is_file());
    srv.delete(&format!("/api/v2/chat/messages/{}", s(&m.body["id"])), &f.alice).await.ok();
    assert!(!sent.exists());
    // Avatars.
    let avatar = srv.send(srv.signed(Method::PUT, "/api/v2/me/avatar", &f.bob, png(300, 200))).await;
    let v = s(&avatar.ok()["avatar"]);
    let r = srv.http.get(srv.url(&format!("/media/avatars/{}?v={v}&t={bob_token}", f.bob_id))).send().await.unwrap();
    let a = image::load_from_memory(&r.bytes().await.unwrap()).unwrap();
    assert_eq!((a.width(), a.height()), (256, 256));
    let me = srv.get("/api/v2/chat/state", &f.bob).await;
    assert_eq!(me.ok()["me"]["avatar"], v.as_str());
}

#[tokio::test(flavor = "multi_thread")]
async fn invitations_approvals_bans_and_discovery() {
    let f = folk().await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Open house").await;
    let group = s(&state["group"]["id"]);
    // One use only.
    let inv = srv.post(&format!("/api/v2/chat/groups/{group}/invites"), &f.alice, json!({"max_uses": 1})).await;
    let code = s(&inv.ok()["code"]);
    let preview = srv.get(&format!("/api/v2/chat/invites/{code}"), &f.bob).await;
    assert_eq!(preview.ok()["group"]["name"], "Open house");
    assert_eq!(preview.ok()["expired"], false);
    srv.post_empty(&format!("/api/v2/chat/invites/{code}/join"), &f.bob).await.ok();
    srv.post_empty(&format!("/api/v2/chat/invites/{code}/join"), &f.carol)
        .await
        .err_reason(400, "bad_request", "invite_expired");
    // Approval: carol asks, alice approves.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/groups/{group}"),
        &f.alice,
        Some(json!({"public": true, "join_approval": true})),
    )
    .await
    .ok();
    let found = srv.get("/api/v2/chat/discover?q=open", &f.carol).await;
    assert_eq!(found.ok()["groups"][0]["id"], group.as_str());
    let (mut aw, _) = chat_ws(srv, &f.alice).await;
    let (mut cw, _) = chat_ws(srv, &f.carol).await;
    let join = srv.post_empty(&format!("/api/v2/chat/groups/{group}/join"), &f.carol).await;
    assert_eq!(join.ok()["pending"], true);
    let req = aw.recv_matching("join request", |v| v["type"] == "chat.request").await;
    assert_eq!(req["data"]["user"]["username"], "carol");
    let list = srv.get(&format!("/api/v2/chat/groups/{group}/requests"), &f.alice).await;
    assert_eq!(list.ok()["requests"][0]["via"], "public");
    srv.post_empty(&format!("/api/v2/chat/groups/{group}/requests/{}/approve", f.carol_id), &f.alice)
        .await
        .ok();
    let answer = cw.recv_matching("approval", |v| v["type"] == "chat.request").await;
    assert_eq!(answer["data"]["approved"], true);
    let joined = cw.recv_matching("group", |v| v["type"] == "chat.group").await;
    assert_eq!(joined["data"]["group"]["id"], group.as_str());
    // Banned: out, and kept out.
    srv.put(&format!("/api/v2/chat/groups/{group}/bans/{}", f.carol_id), &f.alice, json!({"reason": "spam"}))
        .await
        .ok();
    let removed = cw.recv_matching("removed", |v| v["type"] == "chat.group_removed").await;
    assert_eq!(removed["data"]["reason"], "banned");
    srv.post_empty(&format!("/api/v2/chat/groups/{group}/join"), &f.carol)
        .await
        .err_reason(403, "forbidden", "chat_banned");
    let bans = srv.get(&format!("/api/v2/chat/groups/{group}/bans"), &f.alice).await;
    assert_eq!(bans.ok()["bans"][0]["reason"], "spam");
    srv.delete(&format!("/api/v2/chat/groups/{group}/bans/{}", f.carol_id), &f.alice).await.ok();
    // A friend asked in needs no approval.
    befriend(srv, &f.alice, &f.carol, "carol").await;
    srv.post(&format!("/api/v2/chat/groups/{group}/add"), &f.alice, json!({"user": f.carol_id})).await.ok();
    let invite = cw.recv_matching("invite", |v| v["type"] == "chat.invite").await;
    let invite_id = s(&invite["data"]["id"]);
    let accepted = srv.post_empty(&format!("/api/v2/chat/group-invites/{invite_id}/accept"), &f.carol).await;
    assert_eq!(accepted.ok()["joined"]["group"]["id"], group.as_str());
    // The audit log has it all.
    let log = srv.get(&format!("/api/v2/chat/groups/{group}/audit"), &f.alice).await;
    let actions: Vec<String> = log.ok()["entries"].as_array().unwrap().iter().map(|e| s(&e["action"])).collect();
    for a in ["member_ban", "member_unban", "request_approve", "invite_create", "member_invite"] {
        assert!(actions.iter().any(|x| x == a), "{a} in {actions:?}");
    }
    srv.get(&format!("/api/v2/chat/groups/{group}/audit"), &f.bob).await.err_reason(403, "forbidden", "chat_forbidden");
}

#[tokio::test(flavor = "multi_thread")]
async fn dissolving_and_restoring() {
    let f = folk().await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    invite_in(srv, &f.alice, &f.bob, &group).await;
    srv.delete(&format!("/api/v2/chat/groups/{group}"), &f.bob).await.err_reason(403, "forbidden", "chat_forbidden");
    srv.delete(&format!("/api/v2/chat/groups/{group}/members/@me"), &f.alice)
        .await
        .err_reason(409, "conflict", "chat_owner");
    srv.delete(&format!("/api/v2/chat/groups/{group}"), &f.alice).await.ok();
    srv.get(&format!("/api/v2/chat/groups/{group}"), &f.bob).await.err(404, "not_found");
    assert_eq!(srv.get("/api/v2/chat/state", &f.bob).await.ok()["groups"], json!([]));
    srv.post_empty(&format!("/api/v2/chat/groups/{group}/restore"), &f.alice).await.ok();
    assert_eq!(group_state(srv, &f.bob, &group).await["group"]["member_count"], 2);
    // Past the restore window it goes for good.
    srv.delete(&format!("/api/v2/chat/groups/{group}"), &f.alice).await.ok();
    let c = farwing_server::db::open_connection(&srv.cfg.db_path()).unwrap();
    c.execute("UPDATE chat_groups SET dissolved_at = dissolved_at - 31 * 86400", []).unwrap();
    farwing_server::chat::upkeep(srv.app(), true).await;
    srv.post_empty(&format!("/api/v2/chat/groups/{group}/restore"), &f.alice).await.err(404, "not_found");
    let left: i64 = c.query_row("SELECT COUNT(*) FROM chat_channels WHERE group_id = ?1", [&group], |r| r.get(0)).unwrap();
    assert_eq!(left, 0);
    // Leaving.
    let state = make_group(srv, &f.alice, "Again").await;
    let group = s(&state["group"]["id"]);
    invite_in(srv, &f.alice, &f.bob, &group).await;
    srv.delete(&format!("/api/v2/chat/groups/{group}/members/@me"), &f.bob).await.ok();
    assert_eq!(group_state(srv, &f.alice, &group).await["group"]["member_count"], 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn sending_rate_and_slow_mode() {
    let f = folk_with(|c| {
        c.chat.rate_messages = 3;
        c.chat.rate_window_s = 5;
    })
    .await;
    let srv = &f.srv;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    let general = channel(&state, "text");
    invite_in(srv, &f.alice, &f.bob, &group).await;
    for i in 0..3 {
        send(srv, &f.bob, &general, &format!("{i}")).await;
    }
    srv.post(&format!("/api/v2/chat/channels/{general}/messages"), &f.bob, json!({"content": "4"}))
        .await
        .err(429, "rate_limited");
    // Slow mode: once per minute for bob, not for the owner.
    srv.call(Method::PATCH, &format!("/api/v2/chat/channels/{general}"), &f.alice, Some(json!({"slow_mode_s": 60})))
        .await
        .ok();
    let other = srv
        .post("/api/v2/chat/channels", &f.alice, json!({"group_id": group, "kind": "text", "name": "slow"}))
        .await;
    let slow = s(&other.ok()["id"]);
    srv.call(Method::PATCH, &format!("/api/v2/chat/channels/{slow}"), &f.alice, Some(json!({"slow_mode_s": 60})))
        .await
        .ok();
    tokio::time::sleep(Duration::from_millis(5100)).await;
    send(srv, &f.bob, &slow, "one").await;
    let r = srv.post(&format!("/api/v2/chat/channels/{slow}/messages"), &f.bob, json!({"content": "two"})).await;
    r.err_reason(429, "rate_limited", "chat_slow_mode");
    assert!(r.body["error"]["details"]["retry_after_ms"].as_i64().unwrap() > 50_000);
    send(srv, &f.alice, &slow, "owner").await;
    send(srv, &f.alice, &slow, "owner again").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_chat_connection_pushes_messages_typing_and_presence() {
    let f = folk().await;
    let srv = &f.srv;
    befriend(srv, &f.alice, &f.bob, "bob").await;
    let state = make_group(srv, &f.alice, "Team").await;
    let group = s(&state["group"]["id"]);
    let general = channel(&state, "text");
    invite_in(srv, &f.alice, &f.bob, &group).await;
    let (mut aw, snap) = chat_ws(srv, &f.alice).await;
    assert_eq!(snap["groups"][0]["group"]["name"], "Team");
    assert_eq!(snap["me"]["username"], "alice");
    assert_eq!(snap["limits"]["max_content_chars"], 4000);
    let (mut bw, snap) = chat_ws(srv, &f.bob).await;
    // Bob sees alice online.
    assert!(snap["presences"].as_array().unwrap().iter().any(|p| p["user_id"] == f.alice_id.as_str() && p["status"] == "online"));
    let p = aw.recv_matching("bob online", |v| v["type"] == "chat.presence" && v["data"]["user_id"] == f.bob_id.as_str()).await;
    assert_eq!(p["data"]["status"], "online");

    send(srv, &f.alice, &general, "pushed").await;
    let m = bw.recv_matching("message", |v| v["type"] == "chat.message").await;
    assert_eq!(m["data"]["content"], "pushed");
    // Typing reaches the others, not the typist.
    bw.send(json!({"v": V, "type": "chat.typing", "data": {"channel_id": general}})).await;
    let t = aw.recv_matching("typing", |v| v["type"] == "chat.typing").await;
    assert_eq!(t["data"]["user_id"], f.bob_id.as_str());
    // Idle, do not disturb, invisible.
    bw.send(json!({"v": V, "type": "chat.idle", "data": {"idle": true}})).await;
    let p = aw.recv_matching("bob idle", |v| v["type"] == "chat.presence" && v["data"]["user_id"] == f.bob_id.as_str()).await;
    assert_eq!(p["data"]["status"], "idle");
    srv.put("/api/v2/me/status", &f.bob, json!({"status": "invisible"})).await.ok();
    let p = aw.recv_matching("bob gone", |v| v["type"] == "chat.presence" && v["data"]["user_id"] == f.bob_id.as_str()).await;
    assert_eq!(p["data"]["status"], "offline");
    bw.recv_matching("own status", |v| {
        v["type"] == "chat.presence" && v["data"]["user_id"] == f.bob_id.as_str() && v["data"]["status"] == "invisible"
    })
    .await;
    srv.put("/api/v2/me/status", &f.bob, json!({"status": "online"})).await.ok();
    aw.recv_matching("bob back", |v| v["type"] == "chat.presence" && v["data"]["status"] == "idle").await;
    // Group changes arrive as the whole group.
    srv.call(Method::PATCH, &format!("/api/v2/chat/groups/{group}"), &f.alice, Some(json!({"name": "Renamed"})))
        .await
        .ok();
    let g = bw.recv_matching("group", |v| v["type"] == "chat.group").await;
    assert_eq!(g["data"]["group"]["name"], "Renamed");
    // A new connection of the same device replaces the old.
    let (_bw2, _) = chat_ws(srv, &f.bob).await;
    assert_eq!(bw.expect_close(&["chat.presence", "chat.group", "chat.message", "chat.typing"]).await, Some(4001));
    // Signing out closes it.
    drop(aw);
    let (mut aw, _) = chat_ws(srv, &f.alice).await;
    srv.post_empty("/api/v2/auth/logout", &f.alice).await.ok();
    assert_eq!(aw.expect_close(&["chat.presence"]).await, Some(4003));
}

#[tokio::test(flavor = "multi_thread")]
async fn emoji_images_and_their_index_are_served() {
    let srv = start().await;
    let r = srv.http.get(srv.url("/emoji/1f600.png")).send().await.unwrap();
    assert_eq!(r.status().as_u16(), 200);
    assert_eq!(r.headers()["content-type"], "image/png");
    let img = image::load_from_memory(&r.bytes().await.unwrap()).unwrap();
    assert_eq!((img.width(), img.height()), (72, 72));
    let index: Value = srv.http.get(srv.url("/emoji/index.json")).send().await.unwrap().json().await.unwrap();
    let first = &index[0];
    assert_eq!(first["c"], "1f600");
    assert!(first["zk"].as_array().is_some_and(|k| !k.is_empty()));
    assert_eq!(srv.http.get(srv.url("/emoji/nope.png")).send().await.unwrap().status().as_u16(), 404);
}
