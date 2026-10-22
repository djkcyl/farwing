//! NAT behaviour discovery endpoints (`[[turn.stun]]`) of the builtin server,
//! probed with an independent STUN implementation (the `stun` crate).

mod common;

use common::*;
use farwing_server::config::{StunEndpoint, TurnMode};
use farwing_server::stun_server::RATE_BURST;
use serde_json::json;
use std::net::SocketAddr;
use std::time::Duration;
use stun::addr::MappedAddress;
use stun::agent::TransactionId;
use stun::attributes::{
    ATTR_CHANGE_REQUEST, ATTR_FINGERPRINT, ATTR_OTHER_ADDRESS, ATTR_PADDING, ATTR_RESPONSE_ORIGIN,
    ATTR_RESPONSE_PORT, ATTR_SOFTWARE, AttrType,
};
use stun::error_code::ErrorCodeAttribute;
use stun::fingerprint::FINGERPRINT;
use stun::message::{
    BINDING_ERROR, BINDING_REQUEST, BINDING_SUCCESS, CLASS_INDICATION, CLASS_REQUEST, Getter,
    METHOD_ALLOCATE, METHOD_BINDING, Message, MessageType, Setter,
};
use stun::uattrs::UnknownAttributes;
use stun::xoraddr::XorMappedAddress;
use tokio::net::UdpSocket;

const CHANGE_IP: u32 = 0x04;
const CHANGE_PORT: u32 = 0x02;

/// A raw request attribute.
struct Raw(AttrType, Vec<u8>);

impl Setter for Raw {
    fn add_to(&self, m: &mut Message) -> Result<(), stun::Error> {
        m.add(self.0, &self.1);
        Ok(())
    }
}

/// `typ` with a fresh transaction ID, `attrs` and FINGERPRINT.
fn request(typ: MessageType, attrs: Vec<Raw>) -> Message {
    let mut setters: Vec<Box<dyn Setter>> = vec![Box::new(TransactionId::new()), Box::new(typ)];
    for a in attrs {
        setters.push(Box::new(a));
    }
    setters.push(Box::new(FINGERPRINT));
    let mut m = Message::new();
    m.build(&setters).unwrap();
    m
}

fn binding(change: u32) -> Message {
    let attrs = match change {
        0 => Vec::new(),
        flags => vec![Raw(ATTR_CHANGE_REQUEST, flags.to_be_bytes().to_vec())],
    };
    request(BINDING_REQUEST, attrs)
}

/// A response and the address it came from.
struct Reply {
    from: SocketAddr,
    msg: Message,
}

impl Reply {
    fn mapped(&self) -> SocketAddr {
        let mut x = XorMappedAddress::default();
        x.get_from(&self.msg).expect("XOR-MAPPED-ADDRESS");
        SocketAddr::new(x.ip, x.port)
    }

    fn addr(&self, t: AttrType) -> Option<SocketAddr> {
        let mut a = MappedAddress::default();
        a.get_from_as(&self.msg, t).ok()?;
        Some(SocketAddr::new(a.ip, a.port))
    }

    fn origin(&self) -> Option<SocketAddr> {
        self.addr(ATTR_RESPONSE_ORIGIN)
    }

    fn other(&self) -> Option<SocketAddr> {
        self.addr(ATTR_OTHER_ADDRESS)
    }

    /// ERROR-CODE and UNKNOWN-ATTRIBUTES of an error response.
    fn error(&self) -> (u16, Vec<u16>) {
        assert_eq!(self.msg.typ, BINDING_ERROR);
        let mut code = ErrorCodeAttribute::default();
        code.get_from(&self.msg).expect("ERROR-CODE");
        let mut unknown = UnknownAttributes(Vec::new());
        unknown.get_from(&self.msg).ok();
        (code.code.0, unknown.0.iter().map(|t| t.0).collect())
    }
}

/// Test STUN client on 127.0.0.1.
struct Probe {
    sock: UdpSocket,
}

impl Probe {
    async fn new() -> Self {
        Self {
            sock: UdpSocket::bind("127.0.0.1:0").await.unwrap(),
        }
    }

    fn addr(&self) -> SocketAddr {
        self.sock.local_addr().unwrap()
    }

    /// The response to `req` (matching transaction ID) within `wait`.
    async fn recv(&self, req: &Message, wait: Duration) -> Option<Reply> {
        let deadline = tokio::time::Instant::now() + wait;
        let mut buf = [0u8; 1500];
        loop {
            let (n, from) = match tokio::time::timeout_at(deadline, self.sock.recv_from(&mut buf))
                .await
            {
                Err(_) => return None,
                Ok(Ok(r)) => r,
                // ICMP port unreachable (Windows reports it on receive).
                Ok(Err(_)) => continue,
            };
            let mut msg = Message::new();
            msg.raw = buf[..n].to_vec();
            msg.decode().expect("STUN message");
            if msg.transaction_id != req.transaction_id {
                continue;
            }
            if msg.contains(ATTR_FINGERPRINT) {
                FINGERPRINT.check(&msg).expect("valid FINGERPRINT");
            }
            return Some(Reply { from, msg });
        }
    }

    async fn exchange(&self, to: SocketAddr, req: &Message) -> Reply {
        self.sock.send_to(&req.raw, to).await.unwrap();
        let reply = self
            .recv(req, Duration::from_secs(5))
            .await
            .expect("STUN response");
        assert!(reply.msg.contains(ATTR_FINGERPRINT));
        reply
    }

    /// Binding request with CHANGE-REQUEST `change` (0: none).
    async fn binding(&self, to: SocketAddr, change: u32) -> Reply {
        self.exchange(to, &binding(change)).await
    }

    async fn expect_silence(&self, to: SocketAddr, datagram: &[u8]) {
        self.sock.send_to(datagram, to).await.unwrap();
        let mut buf = [0u8; 1500];
        let deadline = tokio::time::Instant::now() + Duration::from_millis(300);
        while let Ok(res) = tokio::time::timeout_at(deadline, self.sock.recv_from(&mut buf)).await {
            if let Ok((n, from)) = res {
                panic!("unexpected {n}-byte answer from {from}");
            }
        }
    }
}

fn sa(s: &str) -> SocketAddr {
    s.parse().unwrap()
}

fn ep(listen: SocketAddr, public: Option<SocketAddr>) -> StunEndpoint {
    StunEndpoint { listen, public }
}

/// Builtin server (public_ip 127.0.0.1) with these `[[turn.stun]]` endpoints.
async fn server(endpoints: Vec<StunEndpoint>) -> TestServer {
    let srv = start_with(|c| {
        c.turn.mode = TurnMode::Builtin;
        c.turn.urls.clear();
        c.turn.shared_secret = "builtin-secret".into();
        c.turn.listen_udp = Some(sa("127.0.0.1:0"));
        c.turn.listen_tcp = None;
        c.turn.listen_tls = None;
        c.turn.public_ip = Some("127.0.0.1".parse().unwrap());
        c.turn.relay_bind = "127.0.0.1".parse().unwrap();
        c.turn.stun = endpoints;
    })
    .await;
    srv.cfg.validate().expect("valid test configuration");
    assert!(srv.cfg.warnings().is_empty(), "{:?}", srv.cfg.warnings());
    srv
}

fn stun_addrs(srv: &TestServer) -> Vec<SocketAddr> {
    srv.server.as_ref().unwrap().stun_udp.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn single_ip_endpoints() {
    let srv = server(vec![ep(sa("127.0.0.1:0"), None), ep(sa("127.0.0.1:0"), None)]).await;
    let e = stun_addrs(&srv);
    assert_eq!(e.len(), 2);

    // Sessions get the endpoints as extra stun: URLs; TURN is unchanged.
    let (_sid, incoming, ack) = srv.ice_for_session().await;
    for body in [&incoming, &ack] {
        let ice = body["ice_servers"].as_array().unwrap();
        assert_eq!(ice.len(), 2, "{ice:?}");
        let stun = ice[0]["urls"].as_array().unwrap();
        assert_eq!(stun.len(), 3, "{stun:?}");
        for a in &e {
            assert!(stun.contains(&json!(format!("stun:{a}"))), "{stun:?}");
        }
        assert_eq!(ice[0]["credential"], "");
        let turn = ice[1]["urls"].as_array().unwrap();
        assert_eq!(turn.len(), 1);
        assert!(turn[0].as_str().unwrap().starts_with("turn:127.0.0.1:"));
    }

    let probe = Probe::new().await;
    // Plain Binding: mapped address and origin; one IP, so no OTHER-ADDRESS.
    for at in [0, 1] {
        let r = probe.binding(e[at], 0).await;
        assert_eq!(r.msg.typ, BINDING_SUCCESS);
        assert_eq!(r.msg.typ.method, METHOD_BINDING);
        assert_eq!((r.from, r.origin()), (e[at], Some(e[at])));
        assert_eq!(r.mapped(), probe.addr());
        assert_eq!(r.other(), None);
    }
    // Change port: answered by the other endpoint.
    for (at, via) in [(0, 1), (1, 0)] {
        let r = probe.binding(e[at], CHANGE_PORT).await;
        assert_eq!(r.msg.typ, BINDING_SUCCESS);
        assert_eq!((r.from, r.origin()), (e[via], Some(e[via])));
        assert_eq!(r.mapped(), probe.addr());
    }
    // No second IP: change IP gets 420 listing CHANGE-REQUEST.
    for flags in [CHANGE_IP, CHANGE_IP | CHANGE_PORT] {
        let r = probe.binding(e[0], flags).await;
        assert_eq!(r.from, e[0]);
        assert_eq!(r.error(), (420, vec![0x0003]));
    }
    // RESPONSE-PORT and PADDING are not supported; SOFTWARE is ignored.
    let req = request(
        BINDING_REQUEST,
        vec![
            Raw(ATTR_RESPONSE_PORT, vec![0x30, 0x39, 0, 0]),
            Raw(ATTR_PADDING, vec![0; 8]),
        ],
    );
    let r = probe.exchange(e[1], &req).await;
    assert_eq!(r.error(), (420, vec![0x0027, 0x0026]));
    let req = request(BINDING_REQUEST, vec![Raw(ATTR_SOFTWARE, b"probe".to_vec())]);
    assert_eq!(probe.exchange(e[1], &req).await.origin(), Some(e[1]));

    // Dropped silently: other methods and classes, a broken FINGERPRINT, junk.
    let allocate = request(MessageType::new(METHOD_ALLOCATE, CLASS_REQUEST), Vec::new());
    probe.expect_silence(e[0], &allocate.raw).await;
    let indication = request(MessageType::new(METHOD_BINDING, CLASS_INDICATION), Vec::new());
    probe.expect_silence(e[0], &indication.raw).await;
    let mut broken = binding(0).raw;
    *broken.last_mut().unwrap() ^= 1;
    probe.expect_silence(e[0], &broken).await;
    probe.expect_silence(e[0], b"GET / HTTP/1.1\r\n\r\n").await;
    assert_eq!(probe.binding(e[0], 0).await.origin(), Some(e[0]));

    // The primary STUN/TURN socket (turn crate) keeps answering on its own,
    // without RFC 5780 attributes.
    let primary = srv.server.as_ref().unwrap().turn_udp.unwrap();
    let r = probe.binding(primary, 0).await;
    assert_eq!(r.msg.typ, BINDING_SUCCESS);
    assert_eq!(r.mapped(), probe.addr());
    assert_eq!(r.origin(), None);

    srv.shutdown().await;
    probe.expect_silence(e[0], &binding(0).raw).await;
}

/// Two ports that are free on both 127.0.0.1 and 127.0.0.2, or `None` when
/// 127.0.0.2 cannot be bound on this machine.
fn square_ports() -> Option<(u16, u16)> {
    use std::net::UdpSocket;
    UdpSocket::bind("127.0.0.2:0").ok()?;
    for _ in 0..50 {
        let a = UdpSocket::bind("127.0.0.1:0").ok()?;
        let b = UdpSocket::bind("127.0.0.1:0").ok()?;
        let (p, q) = (a.local_addr().ok()?.port(), b.local_addr().ok()?.port());
        if UdpSocket::bind(("127.0.0.2", p)).is_ok() && UdpSocket::bind(("127.0.0.2", q)).is_ok() {
            return Some((p, q));
        }
    }
    None
}

#[tokio::test(flavor = "multi_thread")]
async fn two_ip_square() {
    let Some((p, q)) = square_ports() else {
        eprintln!("skipping two_ip_square: 127.0.0.2 cannot be bound here");
        return;
    };
    let a: Vec<SocketAddr> = [
        ("127.0.0.1", p),
        ("127.0.0.1", q),
        ("127.0.0.2", p),
        ("127.0.0.2", q),
    ]
    .iter()
    .map(|(ip, port)| sa(&format!("{ip}:{port}")))
    .collect();
    // The 127.0.0.2 endpoints need public (public_ip is 127.0.0.1).
    let srv = server(vec![
        ep(a[0], None),
        ep(a[1], None),
        ep(a[2], Some(a[2])),
        ep(a[3], Some(a[3])),
    ])
    .await;
    assert_eq!(stun_addrs(&srv), a);

    let probe = Probe::new().await;
    // (endpoint, OTHER-ADDRESS = change both, change IP, change port)
    for (at, other, ip, port) in [(0, 3, 2, 1), (1, 2, 3, 0), (2, 1, 0, 3), (3, 0, 1, 2)] {
        for (flags, via) in [
            (0, at),
            (CHANGE_IP, ip),
            (CHANGE_PORT, port),
            (CHANGE_IP | CHANGE_PORT, other),
        ] {
            let r = probe.binding(a[at], flags).await;
            assert_eq!(r.msg.typ, BINDING_SUCCESS, "{at} {flags:#x}");
            assert_eq!(r.from, a[via], "{at} {flags:#x}");
            assert_eq!(r.origin(), Some(a[via]), "{at} {flags:#x}");
            assert_eq!(r.other(), Some(a[other]), "{at} {flags:#x}");
            assert_eq!(r.mapped(), probe.addr());
        }
    }
    srv.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn per_source_rate_limit() {
    let srv = server(vec![ep(sa("127.0.0.1:0"), None)]).await;
    let e = stun_addrs(&srv)[0];
    let probe = Probe::new().await;
    for _ in 0..3 * RATE_BURST {
        probe.sock.send_to(&binding(0).raw, e).await.unwrap();
    }
    let mut answered = 0;
    let mut buf = [0u8; 1500];
    while let Ok(Ok(_)) =
        tokio::time::timeout(Duration::from_millis(500), probe.sock.recv_from(&mut buf)).await
    {
        answered += 1;
    }
    // The burst plus what refills while sending.
    assert!(
        (RATE_BURST..RATE_BURST + 10).contains(&answered),
        "{answered} answers"
    );
    // Refilled after a while.
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(probe.binding(e, 0).await.origin(), Some(e));
    srv.shutdown().await;
}
