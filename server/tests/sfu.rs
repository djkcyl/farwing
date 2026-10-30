//! Media forwarding (SFU) core: a host publishes once, viewers receive its
//! packets unchanged (but for the transport-wide sequence number), lost
//! uplink packets are requested from the host, viewers' retransmission
//! requests are answered by the forwarder, keyframe requests are merged and
//! sender reports passed on.

mod common;

use bytes::Bytes;
use common::rtc::{self, Received};
use farwing_server::sfu::{Sfu, SfuConfig, sdp};
use std::sync::Arc;
use std::time::Duration;
use webrtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use webrtc::rtcp::sender_report::SenderReport;
use webrtc::rtcp::transport_feedbacks::transport_layer_nack::{
    TransportLayerNack, nack_pairs_from_sequence_numbers,
};
use webrtc::rtp::header::Header;
use webrtc::rtp::packet::Packet;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::util::marshal::Marshal;

const TCC: u8 = 1;
const TIMING: u8 = 3;

fn free_udp_port() -> u16 {
    std::net::UdpSocket::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn packet(ssrc: u32, pt: u8, seq: u16, payload: &[u8]) -> Packet {
    let mut header = Header {
        version: 2,
        extension: true,
        extension_profile: 0xBEDE,
        payload_type: pt,
        sequence_number: seq,
        timestamp: 9000 * u32::from(seq),
        ssrc,
        ..Default::default()
    };
    header
        .set_extension(TCC, Bytes::from_static(&[0xAA, 0xBB]))
        .unwrap();
    header
        .set_extension(
            TIMING,
            Bytes::from_static(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13]),
        )
        .unwrap();
    Packet {
        header,
        payload: Bytes::copy_from_slice(payload),
    }
}

/// The packet's bytes without the transport-wide sequence number.
fn wire(packet: &Packet) -> Bytes {
    let mut packet = packet.clone();
    packet.header.del_extension(TCC).ok();
    packet.marshal().unwrap()
}

async fn next(rx: &mut Received, kind: &str) -> Packet {
    loop {
        let (k, p) = tokio::time::timeout(Duration::from_secs(10), rx.recv())
            .await
            .unwrap_or_else(|_| panic!("no {kind} packet"))
            .unwrap();
        if k == kind {
            return p;
        }
    }
}

async fn media_seq(rx: &mut Received, seq: u16) -> Packet {
    loop {
        let p = next(rx, "media").await;
        if p.header.sequence_number == seq && p.header.payload_type != 111 {
            return p;
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_published_stream_reaches_every_viewer() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(std::env::var("SFU_LOG").unwrap_or_else(|_| "off".into()))
        .try_init();
    let sfu = Sfu::new(SfuConfig {
        port: free_udp_port(),
        bind: "127.0.0.1".parse().unwrap(),
        public_ip: None,
        loopback: true,
    }, farwing_server::egress::Meters::new());
    let mut host = rtc::host().await;
    let offer = host.offer().await;
    let tracks = sdp::tracks(&offer);
    let video = tracks.iter().find(|t| t.id == "video_0").unwrap().clone();
    let audio = tracks.iter().find(|t| t.id == "audio_0").unwrap().clone();
    assert!(video.rtx != 0 && video.fec != 0, "{offer}");
    let answer = sfu.publish("room", &offer).await.unwrap();
    assert!(answer.contains("typ host"), "{answer}");
    host.answer(answer).await;
    host.connected().await;
    let mut host_rtcp = host.sender_rtcp(RTPCodecType::Video).await;

    let mut viewers = Vec::new();
    for id in ["alice", "bob"] {
        let (mut viewer, rx, rtcp) = rtc::viewer(1, 1).await;
        let offer = viewer.offer().await;
        let answer = sfu.subscribe("room", id, &offer, None).await.unwrap();
        // The host's SSRCs, with their repair streams.
        assert!(
            answer.contains(&format!("a=ssrc-group:FID {} {}", video.ssrc, video.rtx)),
            "{answer}"
        );
        assert!(answer.contains(&format!("a=ssrc-group:FEC-FR-RS {} {}", video.ssrc, video.fec)));
        viewer.answer(answer).await;
        viewer.connected().await;
        viewers.push((viewer, rx, rtcp));
    }

    // Packets sent before both ends finish their handshake are lost: warm up
    // with a contiguous run until every viewer receives.
    for seq in 980..1000u16 {
        host.write(&packet(video.ssrc, 98, seq, b"warm-up")).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    for (_, rx, _) in &mut viewers {
        next(rx, "media").await;
        while rx.try_recv().is_ok() {}
    }

    // Media: every viewer gets the host's bytes.
    let first = packet(video.ssrc, 98, 1000, b"keyframe");
    host.write(&first).await;
    host.write(&packet(audio.ssrc, 111, 7, b"opus")).await;
    for (_, rx, _) in &mut viewers {
        let got = media_seq(rx, 1000).await;
        assert_eq!(wire(&got), wire(&first));
        assert_ne!(got.header.get_extension(TCC), first.header.get_extension(TCC));
    }

    // RS-FEC repair packets pass unchanged.
    let fec = packet(video.fec, 36, 50, b"parity");
    host.write(&fec).await;
    for (_, rx, _) in &mut viewers {
        let got = next(rx, "fec").await;
        assert_eq!(wire(&got), wire(&fec));
    }

    // A packet lost on the uplink is requested from the host once for all
    // viewers; its RTX repair reaches them as the original.
    host.write(&packet(video.ssrc, 98, 1002, b"after gap")).await;
    let requested = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            for p in host_rtcp.recv().await.unwrap() {
                if let Some(nack) = p.as_any().downcast_ref::<TransportLayerNack>()
                    && nack.media_ssrc == video.ssrc
                {
                    return nack.nacks.iter().flat_map(|n| n.packet_list()).collect::<Vec<_>>();
                }
            }
        }
    })
    .await
    .expect("uplink NACK");
    assert_eq!(requested, vec![1001]);
    let lost = packet(video.ssrc, 98, 1001, b"repaired");
    let mut rtx = lost.clone();
    rtx.header.ssrc = video.rtx;
    rtx.header.payload_type = 99;
    rtx.header.sequence_number = 5;
    rtx.payload = Bytes::from([&1001u16.to_be_bytes()[..], b"repaired"].concat());
    host.write(&rtx).await;
    for (_, rx, _) in &mut viewers {
        let got = media_seq(rx, 1001).await;
        assert_eq!(wire(&got), wire(&lost));
    }

    // A viewer's NACK is answered from the forwarder's copy, to it alone.
    let (alice, alice_rx, _) = &mut viewers[0];
    alice
        .pc
        .write_rtcp(&[Box::new(TransportLayerNack {
            sender_ssrc: 0,
            media_ssrc: video.ssrc,
            nacks: nack_pairs_from_sequence_numbers(&[1000]),
        })])
        .await
        .unwrap();
    let repair = next(alice_rx, "rtx").await;
    assert_eq!(repair.header.ssrc, video.rtx);
    assert_eq!(repair.header.payload_type, 99);
    assert_eq!(&repair.payload[..2], &1000u16.to_be_bytes());
    assert_eq!(&repair.payload[2..], b"keyframe");

    // Keyframe requests from both viewers reach the host once.
    tokio::time::sleep(Duration::from_millis(400)).await;
    while host_rtcp.try_recv().is_ok() {}
    for (viewer, _, _) in &viewers {
        viewer
            .pc
            .write_rtcp(&[Box::new(PictureLossIndication {
                sender_ssrc: 0,
                media_ssrc: video.ssrc,
            })])
            .await
            .unwrap();
    }
    tokio::time::sleep(Duration::from_millis(250)).await;
    let mut plis = 0;
    while let Ok(packets) = host_rtcp.try_recv() {
        plis += packets
            .iter()
            .filter(|p| p.as_any().downcast_ref::<PictureLossIndication>().is_some())
            .count();
    }
    assert_eq!(plis, 1);

    // Sender reports reach the viewers.
    host.pc
        .write_rtcp(&[Box::new(SenderReport {
            ssrc: video.ssrc,
            ntp_time: 0x1234_5678_9ABC_DEF0,
            rtp_time: 42,
            packet_count: 3,
            octet_count: 30,
            ..Default::default()
        })])
        .await
        .unwrap();
    for (_, _, rtcp) in &mut viewers {
        let report = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                for p in rtcp.recv().await.unwrap() {
                    if let Some(sr) = p.as_any().downcast_ref::<SenderReport>()
                        && sr.ssrc == video.ssrc
                    {
                        return sr.clone();
                    }
                }
            }
        })
        .await
        .expect("sender report");
        assert_eq!(report.ntp_time, 0x1234_5678_9ABC_DEF0);
    }

    let stats = sfu.rooms();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].legs.len(), 2);
    assert!(stats[0].legs.iter().all(|l| l.packets >= 4), "{stats:?}");
    assert_eq!(stats[0].legs[0].repairs, 1);

    // A viewer leaves; the other keeps receiving.
    sfu.unsubscribe("room", "alice").await;
    assert_eq!(sfu.rooms()[0].legs.len(), 1);
    let later = packet(video.ssrc, 98, 1003, b"later");
    host.write(&later).await;
    let (_, bob_rx, _) = &mut viewers[1];
    assert_eq!(wire(&media_seq(bob_rx, 1003).await), wire(&later));

    sfu.close_room("room").await;
    assert!(sfu.rooms().is_empty());
    let host = Arc::new(host);
    let _ = host.pc.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn subscribing_needs_a_published_room() {
    let sfu = Sfu::new(SfuConfig {
        port: free_udp_port(),
        bind: "127.0.0.1".parse().unwrap(),
        public_ip: None,
        loopback: true,
    }, farwing_server::egress::Meters::new());
    let (viewer, _, _) = rtc::viewer(0, 1).await;
    let offer = viewer.offer().await;
    assert!(matches!(
        sfu.subscribe("nothing", "v", &offer, None).await,
        Err(farwing_server::sfu::SfuError::NoRoom)
    ));
}
