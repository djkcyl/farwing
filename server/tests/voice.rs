//! Voice channels (`docs/protocol/chat-design.md` §7): every participant's
//! mic reaches everyone else through the forwarder, downlinks follow as
//! people come and go, frame keys pass between devices, moderators mute.

mod common;

use bytes::Bytes;
use common::rtc;
use common::*;
use reqwest::Method;
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use webrtc::api::media_engine::MIME_TYPE_OPUS;
use webrtc::peer_connection::sdp::session_description::RTCSessionDescription;
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::track::track_local::TrackLocalWriter;
use webrtc::track::track_local::track_local_static_rtp::TrackLocalStaticRTP;

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn s(v: &Value) -> String {
    v.as_str().map(str::to_owned).unwrap_or_else(|| v.to_string())
}

/// One device in the channel: its uplink with a mic, its downlink as the
/// forwarder offers it, its chat connection.
struct Voicer {
    dev: Dev,
    ws: Ws,
    participant: u32,
    up: rtc::Peer,
    mic: Arc<TrackLocalStaticRTP>,
    down: Option<rtc::Peer>,
    /// (stream, payload) of what the downlink delivers.
    heard_tx: mpsc::UnboundedSender<(String, Bytes)>,
    heard: mpsc::UnboundedReceiver<(String, Bytes)>,
    pushes: Vec<Value>,
}

async fn chat_ws(srv: &TestServer, dev: &Dev) -> Ws {
    let r = srv.post_empty("/api/v2/chat/ticket", dev).await;
    let ticket = s(&r.ok()["ticket"]);
    let mut ws = srv.ws_connect(farwing_protocol::headers::TICKET, &ticket, None).await.expect("chat connects");
    ws.recv_type("hello").await;
    ws.send(json!({"v": V, "type": "chat.hello", "id": "h1", "data": {"idle": false}})).await;
    ws.recv_matching("snapshot", |v| v["type"] == "ack" && v["re"] == "h1").await;
    ws
}

impl Voicer {
    async fn join(srv: &TestServer, dev: Dev, channel: &str) -> Self {
        let ws = chat_ws(srv, &dev).await;
        let r = srv.post(&format!("/api/v2/chat/channels/{channel}/voice/join"), &dev, json!({})).await;
        let joined = r.ok().clone();
        let participant = joined["participant"].as_u64().unwrap() as u32;
        let up = rtc::peer(false).await;
        let mic = Arc::new(TrackLocalStaticRTP::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48_000,
                channels: 2,
                sdp_fmtp_line: farwing_rtc_wire::OPUS_FMTP.to_owned(),
                rtcp_feedback: Vec::new(),
            },
            "mic".to_owned(),
            "me".to_owned(),
        ));
        up.pc
            .add_transceiver_from_track(
                Arc::clone(&mic) as _,
                Some(RTCRtpTransceiverInit {
                    direction: RTCRtpTransceiverDirection::Sendonly,
                    send_encodings: Vec::new(),
                }),
            )
            .await
            .unwrap();
        let (heard_tx, heard) = mpsc::unbounded_channel();
        Self {
            dev,
            ws,
            participant,
            up,
            mic,
            down: None,
            heard_tx,
            heard,
            pushes: Vec::new(),
        }
    }

    async fn publish(&mut self, srv: &TestServer, channel: &str) {
        let offer = self.up.offer().await;
        let r = srv
            .put(&format!("/api/v2/chat/channels/{channel}/voice/publish"), &self.dev, json!({"sdp": offer}))
            .await;
        self.up.answer(s(&r.ok()["sdp"])).await;
        self.up.connected().await;
    }

    /// Answers the forwarder's offers and keeps other pushes, for `wait`.
    async fn pump(&mut self, wait: Duration) {
        let until = tokio::time::Instant::now() + wait;
        loop {
            let left = until.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            let Some(Ok(v)) = self.ws.next(left).await else { break };
            if v["type"] == "voice.offer" {
                self.answer(&v["data"]).await;
            } else if v["type"] != "ping" {
                self.pushes.push(v);
            }
        }
    }

    async fn answer(&mut self, offer: &Value) {
        if self.down.is_none() {
            let peer = rtc::peer(false).await;
            let tx = self.heard_tx.clone();
            peer.pc.on_track(Box::new(move |track, _, _| {
                let tx = tx.clone();
                Box::pin(async move {
                    let stream = track.stream_id().to_owned();
                    tokio::spawn(async move {
                        while let Ok((packet, _)) = track.read_rtp().await {
                            let _ = tx.send((stream.clone(), packet.payload));
                        }
                    });
                })
            }));
            self.down = Some(peer);
        }
        let down = self.down.as_ref().unwrap();
        down.pc
            .set_remote_description(RTCSessionDescription::offer(s(&offer["sdp"])).unwrap())
            .await
            .unwrap();
        let answer = down.pc.create_answer(None).await.unwrap();
        let mut gathered = down.pc.gathering_complete_promise().await;
        down.pc.set_local_description(answer).await.unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(5), gathered.recv()).await;
        let sdp = down.pc.local_description().await.unwrap().sdp;
        self.ws
            .send(json!({"v": V, "type": "voice.answer", "data": {
                "channel_id": offer["channel_id"], "generation": offer["generation"], "sdp": sdp,
            }}))
            .await;
    }

    async fn say(&self, words: &'static [u8], n: u16) {
        for i in 0..n {
            let packet = Packet {
                header: Header {
                    version: 2,
                    sequence_number: i,
                    timestamp: u32::from(i) * 960,
                    ..Default::default()
                },
                payload: Bytes::from_static(words),
            };
            self.mic.write_rtp(&packet).await.unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// The streams heard (`p<participant>` → payloads) for `wait`.
    async fn listen(&mut self, wait: Duration) -> HashMap<String, HashSet<Bytes>> {
        let mut out: HashMap<String, HashSet<Bytes>> = HashMap::new();
        let until = tokio::time::Instant::now() + wait;
        while let Ok(Some((stream, payload))) = tokio::time::timeout_at(until, self.heard.recv()).await {
            out.entry(stream).or_default().insert(payload);
        }
        out
    }
}

async fn setup(customize: impl FnOnce(&mut farwing_server::Config)) -> (TestServer, Vec<Dev>, String) {
    let srv = start_with(|c| {
        c.sfu.port = free_udp_port();
        c.turn.relay_bind = "127.0.0.1".parse().unwrap();
        customize(c);
    })
    .await;
    let mut devs = Vec::new();
    for name in ["alice", "bob", "carol"] {
        srv.create_user(name);
        devs.push(srv.account_device(name, &format!("{name}-PC"), false).await);
    }
    let state = srv
        .post(
            "/api/v2/chat/groups",
            &devs[0],
            json!({"name": "Team", "text_channel": "综合", "voice_channel": "语音", "category": "文字频道", "admin_role": "管理员"}),
        )
        .await
        .ok()
        .clone();
    let group = s(&state["group"]["id"]);
    for d in &devs[1..] {
        let inv = srv.post(&format!("/api/v2/chat/groups/{group}/invites"), &devs[0], json!({})).await;
        let code = s(&inv.ok()["code"]);
        srv.post_empty(&format!("/api/v2/chat/invites/{code}/join"), d).await.ok();
    }
    let voice = s(&state["channels"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["kind"] == "voice")
        .unwrap()["id"]);
    (srv, devs, voice)
}

#[tokio::test(flavor = "multi_thread")]
async fn every_mic_reaches_everyone_else_as_people_come_and_go() {
    let (srv, devs, voice) = setup(|c| c.e2ee = farwing_protocol::api::E2ee::Off).await;
    let mut people = Vec::new();
    for dev in devs {
        let mut v = Voicer::join(&srv, dev, &voice).await;
        v.publish(&srv, &voice).await;
        people.push(v);
    }
    // Downlinks follow every publication.
    for _ in 0..2 {
        for v in &mut people {
            v.pump(Duration::from_millis(700)).await;
        }
    }
    for v in &people {
        assert!(v.down.is_some(), "participant {} got a downlink", v.participant);
    }
    let words: [&'static [u8]; 3] = [b"alice", b"bob", b"carol"];
    let say = people.iter().zip(words).map(|(v, w)| v.say(w, 30));
    futures_util::future::join_all(say).await;
    let ids: Vec<u32> = people.iter().map(|v| v.participant).collect();
    for (i, v) in people.iter_mut().enumerate() {
        let heard = v.listen(Duration::from_millis(800)).await;
        for (j, other) in ids.iter().enumerate() {
            let stream = format!("p{other}");
            if i == j {
                assert!(!heard.contains_key(&stream), "{} hears itself", v.participant);
            } else {
                assert!(
                    heard.get(&stream).is_some_and(|p| p.contains(&Bytes::from_static(words[j]))),
                    "{} hears {stream}: {heard:?}",
                    v.participant
                );
            }
        }
    }

    // Who is in it, pushed to the group.
    let state = people[0]
        .pushes
        .iter()
        .rev()
        .find(|p| p["type"] == "voice.state")
        .expect("voice.state")
        .clone();
    assert_eq!(state["data"]["participants"].as_array().unwrap().len(), 3);

    // A frame key for one device reaches only it, from the sender's number.
    let bob_device = people[1].dev.id.clone();
    srv.post(
        &format!("/api/v2/chat/channels/{voice}/voice/keys"),
        &people[0].dev,
        json!({"key_id": 3, "boxes": [{"device_id": bob_device, "ephemeral": "e", "data": "d", "signature": "s"}]}),
    )
    .await
    .ok();
    people[1].pump(Duration::from_millis(500)).await;
    let keys = people[1].pushes.iter().find(|p| p["type"] == "voice.keys").expect("voice.keys").clone();
    assert_eq!(keys["data"]["participant"], ids[0]);
    assert_eq!(keys["data"]["key_id"], 3);
    assert_eq!(keys["data"]["device_id"], people[0].dev.id.as_str());

    // A moderator's mute: alice (the owner) mutes bob; nobody hears him.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/channels/{voice}/voice/participants/{}", ids[1]),
        &people[0].dev,
        Some(json!({"server_muted": true})),
    )
    .await
    .ok();
    people[1].say(b"bob-muted", 20).await;
    let heard = people[2].listen(Duration::from_millis(600)).await;
    assert!(
        !heard.get(&format!("p{}", ids[1])).is_some_and(|p| p.contains(&Bytes::from_static(b"bob-muted"))),
        "a muted participant is not forwarded"
    );
    // Bob may not mute alice.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/channels/{voice}/voice/participants/{}", ids[0]),
        &people[1].dev,
        Some(json!({"server_muted": true})),
    )
    .await
    .err_reason(403, "forbidden", "chat_forbidden");

    // Carol leaves: the others' downlinks drop her track; two remain.
    let carol = people.pop().unwrap();
    srv.post_empty(&format!("/api/v2/chat/channels/{voice}/voice/leave"), &carol.dev).await.ok();
    for v in &mut people {
        v.pump(Duration::from_millis(700)).await;
    }
    let last = people[0].pushes.iter().rev().find(|p| p["type"] == "voice.state").unwrap().clone();
    assert_eq!(last["data"]["participants"].as_array().unwrap().len(), 2);
    let transceivers = people[0].down.as_ref().unwrap().pc.get_transceivers().await;
    let mut receiving = 0;
    for t in transceivers {
        if t.current_direction() == RTCRtpTransceiverDirection::Recvonly {
            receiving += 1;
        }
    }
    assert_eq!(receiving, 1, "alice receives bob alone");
    // Leaving twice is harmless; publishing outside is refused.
    srv.post_empty(&format!("/api/v2/chat/channels/{voice}/voice/leave"), &carol.dev).await.ok();
    srv.put(&format!("/api/v2/chat/channels/{voice}/voice/publish"), &carol.dev, json!({"sdp": "v=0"}))
        .await
        .err_reason(404, "not_found", "voice_not_in");
}

#[tokio::test(flavor = "multi_thread")]
async fn end_to_end_voice_needs_a_device_key_and_respects_the_limit() {
    let (srv, devs, voice) = setup(|_| {}).await;
    srv.post(&format!("/api/v2/chat/channels/{voice}/voice/join"), &devs[0], json!({}))
        .await
        .err_reason(400, "bad_request", "voice_no_key");
    // The key, signed by the device.
    // The server keeps the key as it is (32 bytes); devices use it.
    let public = farwing_protocol::crypto::b64(&rand::random::<[u8; 32]>());
    let bad = devs[1].key.sign(farwing_protocol::chat::x25519_message(&public).as_bytes());
    srv.put("/api/v2/devices/self/x25519", &devs[0], json!({"public": public, "signature": bad}))
        .await
        .err(400, "bad_request");
    for d in &devs {
        let signature = d.key.sign(farwing_protocol::chat::x25519_message(&public).as_bytes());
        srv.put("/api/v2/devices/self/x25519", d, json!({"public": public, "signature": signature}))
            .await
            .ok();
    }
    let joined = srv.post(&format!("/api/v2/chat/channels/{voice}/voice/join"), &devs[0], json!({})).await;
    assert_eq!(joined.ok()["mode"], "e2ee");
    assert_eq!(joined.ok()["participants"][0]["x25519"], public.as_str());

    // A limit of one: bob is refused; alice (the owner holds every
    // permission, priority included) is not counted against herself.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/channels/{voice}"),
        &devs[0],
        Some(json!({"voice_limit": 1})),
    )
    .await
    .ok();
    srv.post(&format!("/api/v2/chat/channels/{voice}/voice/join"), &devs[1], json!({}))
        .await
        .err_reason(403, "forbidden", "voice_full");
    // Compatible mode by the owner's choice: the join says so.
    srv.call(
        Method::PATCH,
        &format!("/api/v2/chat/channels/{voice}"),
        &devs[0],
        Some(json!({"voice_limit": 0, "voice_compat": true})),
    )
    .await
    .ok();
    let joined = srv.post(&format!("/api/v2/chat/channels/{voice}/voice/join"), &devs[1], json!({})).await;
    // The channel's mode is fixed while people are in it.
    assert_eq!(joined.ok()["mode"], "e2ee");
    srv.post_empty(&format!("/api/v2/chat/channels/{voice}/voice/leave"), &devs[0]).await.ok();
    srv.post_empty(&format!("/api/v2/chat/channels/{voice}/voice/leave"), &devs[1]).await.ok();
    let joined = srv.post(&format!("/api/v2/chat/channels/{voice}/voice/join"), &devs[1], json!({})).await;
    assert_eq!(joined.ok()["mode"], "compatible");
}

#[tokio::test(flavor = "multi_thread")]
async fn accounts_without_forwarding_stay_out() {
    let (srv, devs, voice) = setup(|c| {
        c.e2ee = farwing_protocol::api::E2ee::Off;
        c.egress.users.forward = false;
    })
    .await;
    srv.post(&format!("/api/v2/chat/channels/{voice}/voice/join"), &devs[0], json!({}))
        .await
        .err_reason(403, "forbidden", "voice_no_forwarding");
}

#[tokio::test(flavor = "multi_thread")]
async fn calls_ring_connect_signal_and_leave_a_record() {
    let srv = start().await;
    let alice_id = srv.create_user("alice");
    let bob_id = srv.create_user("bob");
    let alice = srv.account_device("alice", "ALICE-PC", false).await;
    let bob = srv.account_device("bob", "BOB-PC", false).await;
    let bob_phone = srv.account_device("bob", "BOB-PHONE", false).await;
    let r = srv.post("/api/v2/friends/requests", &alice, json!({"username": "bob"})).await;
    let request = s(&r.ok()["sent"]["request"]["id"]);
    srv.post(&format!("/api/v2/friends/requests/{request}/accept"), &bob, json!({"caps": ["view"]}))
        .await
        .ok();
    let dm = srv.post("/api/v2/chat/dm", &alice, json!({"user": bob_id})).await;
    let channel = s(&dm.ok()["channel"]["id"]);
    let mut aw = chat_ws(&srv, &alice).await;
    let mut bw = chat_ws(&srv, &bob).await;
    let mut pw = chat_ws(&srv, &bob_phone).await;

    let call = srv.post(&format!("/api/v2/chat/dm/{channel}/call"), &alice, json!({})).await;
    let call = call.ok().clone();
    let id = s(&call["id"]);
    assert_eq!(call["state"], "ringing");
    assert_eq!(call["caller"]["user_id"], alice_id.as_str());
    // Every device of bob rings.
    for ws in [&mut bw, &mut pw] {
        let ring = ws.recv_matching("ring", |v| v["type"] == "call.ring").await;
        assert_eq!(ring["data"]["id"], id.as_str());
        assert_eq!(ring["data"]["caller_key"], alice.key.public_key().as_str());
    }
    // The PC answers; the phone stops ringing; alice learns who.
    let answered = srv.post_empty(&format!("/api/v2/chat/calls/{id}/answer"), &bob).await;
    assert_eq!(answered.ok()["callee_device"], bob.id.as_str());
    let ended = pw.recv_matching("answered elsewhere", |v| v["type"] == "call.ended").await;
    assert_eq!(ended["data"]["reason"], "answered_elsewhere");
    let a = aw.recv_matching("answered", |v| v["type"] == "call.answered").await;
    assert_eq!(a["data"]["callee_device"], bob.id.as_str());
    assert_eq!(a["data"]["callee_key"], bob.key.public_key().as_str());
    // The phone can no longer answer.
    srv.post_empty(&format!("/api/v2/chat/calls/{id}/answer"), &bob_phone)
        .await
        .err_reason(409, "conflict", "call_gone");
    // Signaling goes to the other device only, marked with its sender.
    aw.send(json!({"v": V, "type": "call.signal", "data": {"call_id": id, "kind": "offer", "data": "sdp", "signature": "sig"}}))
        .await;
    let sig = bw.recv_matching("signal", |v| v["type"] == "call.signal").await;
    assert_eq!(sig["data"]["from_device"], alice.id.as_str());
    assert_eq!(sig["data"]["data"], "sdp");
    // Bob hangs up: both sides end; the record says how long.
    tokio::time::sleep(Duration::from_millis(1100)).await;
    srv.post_empty(&format!("/api/v2/chat/calls/{id}/hangup"), &bob).await.ok();
    let e = aw.recv_matching("ended", |v| v["type"] == "call.ended").await;
    assert_eq!(e["data"]["reason"], "hangup");
    let record = aw.recv_matching("record", |v| v["type"] == "chat.message").await;
    assert_eq!(record["data"]["author"]["user_id"], alice_id.as_str());
    assert_eq!(record["data"]["call"]["missed"], false);
    assert!(record["data"]["call"]["duration_s"].as_u64().unwrap() >= 1);

    // Declined: a missed call in the record.
    let call = srv.post(&format!("/api/v2/chat/dm/{channel}/call"), &alice, json!({})).await;
    let id = s(&call.ok()["id"]);
    srv.post_empty(&format!("/api/v2/chat/calls/{id}/decline"), &bob_phone).await.ok();
    let record = aw.recv_matching("missed record", |v| v["type"] == "chat.message").await;
    assert_eq!(record["data"]["call"]["missed"], true);
    let history = srv.get(&format!("/api/v2/chat/channels/{channel}/messages"), &bob).await;
    assert_eq!(history.ok()["messages"].as_array().unwrap().len(), 2);
}
