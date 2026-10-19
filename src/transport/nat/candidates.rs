//! ICE candidate lines derived from real ones: predicted remote srflx ports
//! and the srflx candidate of a gateway port mapping. Both keep the source
//! candidate's foundation, generation, network-id/cost and ufrag fields.
use std::net::{Ipv4Addr, SocketAddrV4};
use webrtc::ice_transport::ice_candidate::RTCIceCandidateInit;

/// `foundation component transport priority address port typ type [k v]...`
struct Fields<'a> {
    foundation: &'a str,
    component: &'a str,
    transport: &'a str,
    priority: u32,
    address: &'a str,
    port: u16,
    kind: &'a str,
    extensions: Vec<(&'a str, &'a str)>,
}

impl<'a> Fields<'a> {
    fn parse(candidate: &'a str) -> Option<Self> {
        let value = candidate.trim();
        let value = value.strip_prefix("a=").unwrap_or(value);
        let value = value.strip_prefix("candidate:").unwrap_or(value);
        let words: Vec<&str> = value.split_whitespace().collect();
        if words.len() < 8 || words[6] != "typ" || (words.len() - 8) % 2 != 0 {
            return None;
        }
        Some(Self {
            foundation: words[0],
            component: words[1],
            transport: words[2],
            priority: words[3].parse().ok()?,
            address: words[4],
            port: words[5].parse().ok()?,
            kind: words[7],
            extensions: words[8..]
                .chunks(2)
                .map(|pair| (pair[0], pair[1]))
                .collect(),
        })
    }

    fn udp_v4(&self) -> Option<Ipv4Addr> {
        if !self.transport.eq_ignore_ascii_case("udp") {
            return None;
        }
        self.address.parse().ok()
    }

    /// The line with another type, address, port and priority. Related
    /// address fields are replaced by `related`.
    fn render(
        &self,
        foundation: &str,
        kind: &str,
        address: &str,
        port: u16,
        priority: u32,
        related: Option<SocketAddrV4>,
    ) -> String {
        let mut line = format!(
            "candidate:{foundation} {} {} {priority} {address} {port} typ {kind}",
            self.component, self.transport
        );
        let related = related.map(|address| address.to_string());
        if let Some(related) = related.as_ref().and_then(|r| r.rsplit_once(':')) {
            line.push_str(&format!(" raddr {} rport {}", related.0, related.1));
        }
        for (key, value) in &self.extensions {
            if related.is_some() && matches!(*key, "raddr" | "rport") {
                continue;
            }
            line.push_str(&format!(" {key} {value}"));
        }
        line
    }
}

/// Remote candidates at `port + delta * k` (k = 1..=count) for a remote
/// UDP/IPv4 srflx candidate, at slightly lower priorities (one local
/// preference step per k). Other candidates yield none.
pub(super) fn predicted(
    candidate: &RTCIceCandidateInit,
    delta: i32,
    count: u16,
) -> Vec<RTCIceCandidateInit> {
    let Some(fields) = Fields::parse(&candidate.candidate) else {
        return Vec::new();
    };
    if delta == 0 || fields.kind != "srflx" || fields.udp_v4().is_none() {
        return Vec::new();
    }
    (1..=i32::from(count))
        .filter_map(|k| {
            let port = u16::try_from(i32::from(fields.port) + delta * k)
                .ok()
                .filter(|port| *port >= 1024)?;
            let priority = fields.priority.saturating_sub((k as u32) << 8);
            let line = fields.render(
                fields.foundation,
                fields.kind,
                fields.address,
                port,
                priority,
                None,
            );
            Some(RTCIceCandidateInit {
                candidate: line,
                ..candidate.clone()
            })
        })
        .collect()
}

/// The address of a UDP/IPv4 candidate of type `kind` (`host`, `srflx`).
pub(super) fn udp_v4(candidate: &RTCIceCandidateInit, kind: &str) -> Option<SocketAddrV4> {
    let fields = Fields::parse(&candidate.candidate)?;
    (fields.kind == kind)
        .then(|| fields.udp_v4())
        .flatten()
        .map(|ip| SocketAddrV4::new(ip, fields.port))
}

/// A srflx candidate for the gateway mapping `external` of a host
/// candidate, with srflx type preference (100) and the host's local
/// preference, like the agent's own srflx candidates.
pub(super) fn mapped(
    host: &RTCIceCandidateInit,
    external: SocketAddrV4,
) -> Option<RTCIceCandidateInit> {
    let fields = Fields::parse(&host.candidate)?;
    let internal = SocketAddrV4::new(fields.udp_v4()?, fields.port);
    let priority = (100 << 24) | (fields.priority & 0x00ff_ffff);
    let foundation = foundation(&format!("pmap/{}/{}", internal.ip(), external.ip()));
    Some(RTCIceCandidateInit {
        candidate: fields.render(
            &foundation,
            "srflx",
            &external.ip().to_string(),
            external.port(),
            priority,
            Some(internal),
        ),
        ..host.clone()
    })
}

/// A stable numeric foundation (FNV-1a), distinct from the agent's own.
fn foundation(key: &str) -> String {
    let hash = key.bytes().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193)
    });
    hash.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init(candidate: &str) -> RTCIceCandidateInit {
        RTCIceCandidateInit {
            candidate: candidate.into(),
            sdp_mid: Some("0".into()),
            sdp_mline_index: Some(0),
            username_fragment: Some("abcd".into()),
        }
    }

    #[test]
    fn predicts_srflx_ports_only() {
        let srflx = init(
            "candidate:842163049 1 udp 1686052607 203.0.113.7 40000 typ srflx raddr 192.168.1.5 rport 50000 generation 0 network-id 1 network-cost 10 ufrag abcd",
        );
        let list = predicted(&srflx, 2, 8);
        assert_eq!(list.len(), 8);
        assert_eq!(
            list[0].candidate,
            "candidate:842163049 1 udp 1686052351 203.0.113.7 40002 typ srflx raddr 192.168.1.5 rport 50000 generation 0 network-id 1 network-cost 10 ufrag abcd"
        );
        assert!(list[7].candidate.contains(" 203.0.113.7 40016 typ srflx"));
        assert_eq!(list[7].username_fragment.as_deref(), Some("abcd"));
        assert_eq!(list[7].sdp_mid.as_deref(), Some("0"));
        // Downward steps stop before the reserved range.
        assert_eq!(
            predicted_ports(
                &init("candidate:1 1 udp 100 203.0.113.7 1030 typ srflx raddr 0.0.0.0 rport 0"),
                -3
            ),
            vec![1027, 1024]
        );
        // Host, relay, TCP and IPv6 candidates are not predicted.
        for line in [
            "candidate:1 1 udp 2130706431 192.168.1.5 50000 typ host",
            "candidate:1 1 udp 16777215 198.51.100.1 3478 typ relay raddr 203.0.113.7 rport 40000",
            "candidate:1 1 tcp 1686052607 203.0.113.7 40000 typ srflx raddr 192.168.1.5 rport 9",
            "candidate:1 1 udp 1686052607 2001:db8::1 40000 typ srflx raddr 2001:db8::1 rport 1",
        ] {
            assert!(predicted(&init(line), 1, 8).is_empty(), "{line}");
        }
        assert!(predicted(&srflx, 0, 8).is_empty());
    }

    fn predicted_ports(candidate: &RTCIceCandidateInit, delta: i32) -> Vec<u16> {
        predicted(candidate, delta, 8)
            .iter()
            .map(|c| Fields::parse(&c.candidate).unwrap().port)
            .collect()
    }

    #[test]
    fn mapped_srflx_from_host() {
        let host = init(
            "candidate:1966762133 1 udp 2122260223 192.168.1.5 50000 typ host generation 0 network-id 1 network-cost 10 ufrag abcd",
        );
        assert_eq!(
            udp_v4(&host, "host"),
            Some("192.168.1.5:50000".parse().unwrap())
        );
        let srflx = mapped(&host, "203.0.113.7:50000".parse().unwrap()).unwrap();
        let fields = Fields::parse(&srflx.candidate).unwrap();
        assert_eq!(fields.kind, "srflx");
        assert_eq!(fields.priority, (100 << 24) | (2122260223 & 0x00ff_ffff));
        assert!(srflx.candidate.ends_with(
            " 203.0.113.7 50000 typ srflx raddr 192.168.1.5 rport 50000 generation 0 network-id 1 network-cost 10 ufrag abcd"
        ));
        assert_eq!(srflx.username_fragment.as_deref(), Some("abcd"));
        // Only UDP/IPv4 host candidates.
        assert_eq!(
            udp_v4(
                &init("candidate:1 1 tcp 1 192.168.1.5 9 typ host tcptype active"),
                "host"
            ),
            None
        );
        assert_eq!(
            udp_v4(&srflx, "srflx"),
            Some("203.0.113.7:50000".parse().unwrap())
        );
    }
}
