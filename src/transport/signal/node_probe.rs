//! Round trips to the server's relay nodes (`Hello::probe_nodes`): three
//! STUN Binding requests to each node at once, the lowest round trip within
//! two seconds, reported as `nodes.rtt` on the presence connection now and
//! every 30 minutes while it lasts. The server hands each session side the
//! nodes nearest to it (`docs/protocol/deployment-design.md` §4.5).

use farwing_protocol::realtime::{Envelope, NodesRtt, ProbeNode, types};
use std::collections::BTreeMap;
use std::time::Duration;
use tokio::net::UdpSocket;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message;

const REQUESTS: usize = 3;
const SPACING: Duration = Duration::from_millis(100);
const WAIT: Duration = Duration::from_secs(2);
const AGAIN_AFTER_MINUTES: u32 = 30;
const MAGIC_COOKIE: [u8; 4] = [0x21, 0x12, 0xa4, 0x42];

/// Measure `nodes` and report over `writer` until its connection closes.
pub(super) fn spawn(nodes: Vec<ProbeNode>, writer: mpsc::UnboundedSender<Message>) {
    tokio::spawn(async move {
        loop {
            let rtt = measure(&nodes).await;
            tracing::info!(?rtt, "relay node round trips");
            let report = Envelope::new(types::NODES_RTT, NodesRtt { rtt });
            if writer.send(Message::Text(report.encode().into())).is_err() {
                return;
            }
            for _ in 0..AGAIN_AFTER_MINUTES {
                tokio::time::sleep(Duration::from_secs(60)).await;
                if writer.is_closed() {
                    return;
                }
            }
        }
    });
}

async fn measure(nodes: &[ProbeNode]) -> BTreeMap<String, Option<u32>> {
    futures_util::future::join_all(
        nodes
            .iter()
            .map(|node| async { (node.id.clone(), round_trip(&node.stun).await) }),
    )
    .await
    .into_iter()
    .collect()
}

fn request(transaction: &[u8; 12]) -> [u8; 20] {
    let mut packet = [0u8; 20];
    packet[..2].copy_from_slice(&0x0001u16.to_be_bytes());
    packet[4..8].copy_from_slice(&MAGIC_COOKIE);
    packet[8..].copy_from_slice(transaction);
    packet
}

/// The transaction a Binding success response answers.
fn answered(packet: &[u8]) -> Option<[u8; 12]> {
    (packet.len() >= 20 && packet[..2] == 0x0101u16.to_be_bytes() && packet[4..8] == MAGIC_COOKIE)
        .then(|| packet[8..20].try_into().ok())
        .flatten()
}

/// The lowest round trip to `address` (`host:port`) in milliseconds.
async fn round_trip(address: &str) -> Option<u32> {
    let target = tokio::net::lookup_host(address).await.ok()?.next()?;
    let socket = UdpSocket::bind(if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })
        .await
        .ok()?;
    let mut sent: Vec<([u8; 12], Instant)> = Vec::new();
    let mut best: Option<Duration> = None;
    let deadline = Instant::now() + WAIT;
    let mut next_send = Instant::now();
    let mut buffer = [0u8; 512];
    loop {
        if sent.len() < REQUESTS && Instant::now() >= next_send {
            let transaction: [u8; 12] = rand::random();
            if socket.send_to(&request(&transaction), target).await.is_ok() {
                sent.push((transaction, Instant::now()));
            }
            next_send = Instant::now() + SPACING;
        }
        let wake = if sent.len() < REQUESTS {
            next_send.min(deadline)
        } else {
            deadline
        };
        tokio::select! {
            received = socket.recv_from(&mut buffer) => {
                let Ok((n, from)) = received else { continue };
                if from != target {
                    continue;
                }
                if let Some(answer) = answered(&buffer[..n])
                    && let Some((_, at)) = sent.iter().find(|(t, _)| *t == answer)
                {
                    let elapsed = at.elapsed();
                    best = Some(best.map_or(elapsed, |b| b.min(elapsed)));
                    if sent.len() == REQUESTS {
                        break;
                    }
                }
            }
            _ = tokio::time::sleep_until(wake) => {
                if Instant::now() >= deadline {
                    break;
                }
            }
        }
    }
    best.map(|d| d.as_millis().min(u128::from(u32::MAX - 2)) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_packets() {
        let transaction = [7u8; 12];
        let packet = request(&transaction);
        assert_eq!(&packet[..4], &[0, 1, 0, 0]);
        let mut response = packet.to_vec();
        response[..2].copy_from_slice(&0x0101u16.to_be_bytes());
        response.extend_from_slice(&[0, 0x20, 0, 8, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(answered(&response), Some(transaction));
        assert_eq!(answered(&packet), None, "a request is no answer");
    }

    #[tokio::test]
    async fn round_trip_to_a_local_responder() {
        let server = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = server.local_addr().unwrap().to_string();
        tokio::spawn(async move {
            let mut buffer = [0u8; 64];
            while let Ok((n, from)) = server.recv_from(&mut buffer).await {
                let mut response = buffer[..n].to_vec();
                response[..2].copy_from_slice(&0x0101u16.to_be_bytes());
                let _ = server.send_to(&response, from).await;
            }
        });
        assert!(round_trip(&address).await.is_some_and(|ms| ms < 1000));
        // Nobody answers here.
        let silent = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let address = silent.local_addr().unwrap().to_string();
        assert_eq!(round_trip(&address).await, None);
    }
}
