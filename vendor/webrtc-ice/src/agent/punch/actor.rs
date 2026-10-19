//! One owned probe event loop. Timers and temporary sockets are separate from
//! the media receiver; no media buffering or retry policy is introduced here.
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::future::{poll_fn, Future};
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::Weak;
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::io::{AsyncRead, ReadBuf};
use tokio::net::{TcpSocket, TcpStream, UdpSocket};
use tokio::sync::mpsc;
use tokio::time::Instant;

use super::{Coordinator, NatKind, Plan, REQUEST, RESPONSE};

pub(super) enum Command {
    Start(Plan),
    Direct,
}

#[derive(Clone, Copy, Debug)]
enum Phase {
    Local {
        count: usize,
        interval_ms: u64,
    },
    Random {
        count: usize,
        all_ips: bool,
        interval_ms: u64,
        hard: bool,
    },
    Linear,
    LinearWait,
    BothLinear,
}
impl Phase {
    fn backoff(self) -> Duration {
        Duration::from_secs(if matches!(self, Self::Random { hard: true, .. }) {
            120
        } else {
            5
        })
    }
}

#[derive(Clone, Copy)]
enum Event {
    Phase(Phase),
    EndRound,
    Resume,
    FirewallStart,
    FirewallCheck,
    Tick,
}
struct Timer {
    epoch: u64,
    event: Event,
}

struct ProbeSocket {
    socket: Option<UdpSocket>,
    target: SocketAddr,
}
enum Firewall {
    Connecting(Pin<Box<dyn Future<Output = io::Result<TcpStream>> + Send>>),
    Done {
        reachable: bool,
        socket: Option<TcpStream>,
    },
}
enum FirewallEvent {
    Connected(usize, io::Result<TcpStream>),
    Closed(usize),
    Data,
}
enum Next {
    Command(Option<Command>),
    Timer,
    Probe(io::Result<(usize, SocketAddr, usize)>),
    Firewall(FirewallEvent),
}

pub(super) struct Actor {
    coordinator: Weak<Coordinator>,
    commands: mpsc::UnboundedReceiver<Command>,
    plan: Option<Plan>,
    epoch: u64,
    serial: u64,
    mode: u8,
    backoff: Duration,
    timers: BTreeMap<(Instant, u64), Timer>,
    targets: Vec<SocketAddr>,
    pending_targets: VecDeque<SocketAddr>,
    sockets: Vec<ProbeSocket>,
    pending_sockets: VecDeque<usize>,
    firewall: Vec<Firewall>,
}

impl Actor {
    pub fn new(coordinator: Weak<Coordinator>, commands: mpsc::UnboundedReceiver<Command>) -> Self {
        Self {
            coordinator,
            commands,
            plan: None,
            epoch: 0,
            serial: 0,
            mode: 10,
            backoff: Duration::from_secs(5),
            timers: BTreeMap::new(),
            targets: Vec::new(),
            pending_targets: VecDeque::new(),
            sockets: Vec::new(),
            pending_sockets: VecDeque::new(),
            firewall: Vec::new(),
        }
    }

    fn schedule(&mut self, delay: Duration, event: Event) {
        self.serial = self.serial.wrapping_add(1);
        self.timers.insert(
            (Instant::now() + delay, self.serial),
            Timer {
                epoch: self.epoch,
                event,
            },
        );
    }

    fn reset_probes(&mut self) {
        // Closes temporary sockets and clears both live/remaining sets.
        // It does not purge already-posted timers or close the base UDP Port.
        self.sockets.clear();
        self.pending_sockets.clear();
        self.targets.clear();
        self.pending_targets.clear();
    }

    pub async fn run(mut self) {
        let mut packet = vec![0; 65_556];
        loop {
            match self.next(&mut packet).await {
                Next::Command(None) => break,
                Next::Command(Some(Command::Start(plan))) => {
                    if self.plan.is_some() {
                        self.epoch = self.epoch.wrapping_add(1);
                        self.reset_probes();
                        self.firewall.clear();
                    }
                    self.mode = mode(plan.local_kind, plan.remote.kind);
                    self.plan = Some(plan);
                    self.begin_round().await;
                }
                Next::Command(Some(Command::Direct)) => {
                    // Invalidates generation-tagged send work, but
                    // intentionally leaves readers for late probe responses.
                    self.epoch = self.epoch.wrapping_add(1);
                    log::info!("punch sends stopped after direct connection selection");
                }
                Next::Timer => {
                    let Some((_, timer)) = self.timers.pop_first() else {
                        continue;
                    };
                    self.timer(timer).await;
                }
                Next::Probe(Ok((index, source, n))) => {
                    self.probe_received(index, source, &packet[..n])
                }
                Next::Probe(Err(error)) => log::debug!("temporary probe socket read: {error}"),
                Next::Firewall(event) => self.firewall_event(event),
            }
        }
    }

    async fn next(&mut self, packet: &mut [u8]) -> Next {
        let due = self.timers.first_key_value().map(|(key, _)| key.0);
        let timer = async move {
            if let Some(due) = due {
                tokio::time::sleep_until(due).await;
            } else {
                std::future::pending::<()>().await;
            }
        };
        let probes = &self.sockets;
        let firewall = &mut self.firewall;
        tokio::select! {
            biased;
            command=self.commands.recv()=>Next::Command(command),
            _=timer=>Next::Timer,
            packet=poll_fn(|cx|poll_probe(probes,cx,packet))=>Next::Probe(packet),
            event=poll_fn(|cx|poll_firewall(firewall,cx))=>Next::Firewall(event),
        }
    }

    async fn begin_round(&mut self) {
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        let controlling = plan.controlling;
        log::info!("punch round: epoch={} mode={}", self.epoch, self.mode);
        // Blocks only the punch thread for these two modes. The
        // enclosing owner can still cancel this async wait during shutdown.
        if self.mode == 3 || self.mode == 5 {
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
        let phase = match self.mode {
            0 | 1 => {
                let odd = (self.epoch / 5) & 1 != 0;
                if controlling ^ odd {
                    Some(Phase::Local {
                        count: 300,
                        interval_ms: 20_000,
                    })
                } else {
                    Some(Phase::Random {
                        count: 250,
                        all_ips: true,
                        interval_ms: 10_000,
                        hard: false,
                    })
                }
            }
            2 => Some(Phase::Linear),
            3 => Some(Phase::LinearWait),
            4 => Some(Phase::Random {
                count: 500,
                all_ips: true,
                interval_ms: 60_000,
                hard: false,
            }),
            5 => Some(Phase::Local {
                count: 150,
                interval_ms: 6_000,
            }),
            6 => Some(Phase::BothLinear),
            7 | 9 => Some(Phase::Random {
                count: 100,
                all_ips: true,
                interval_ms: 60_000,
                hard: true,
            }),
            8 => Some(Phase::Random {
                count: 100,
                all_ips: false,
                interval_ms: 60_000,
                hard: true,
            }),
            _ => None,
        };
        if let Some(phase) = phase {
            self.phase(self.epoch, phase).await;
        }
        self.schedule(Duration::from_secs(20), Event::EndRound);
        self.schedule(Duration::from_secs(10), Event::FirewallStart);
        self.tick();
    }

    async fn timer(&mut self, timer: Timer) {
        if let Event::Phase(phase) = timer.event {
            // Native phase helpers assign the next-round interval BEFORE
            // checking epoch; retain this order, even for old posted work.
            self.phase(timer.epoch, phase).await;
            return;
        }
        if timer.epoch != self.epoch {
            return;
        }
        match timer.event {
            Event::EndRound => {
                self.epoch = self.epoch.wrapping_add(1);
                log::info!(
                    "punch round ended; next round in {}ms",
                    self.backoff.as_millis()
                );
                self.schedule(self.backoff, Event::Resume);
            }
            Event::Resume => self.begin_round().await,
            Event::Tick => self.tick(),
            Event::FirewallStart => self.start_firewall().await,
            Event::FirewallCheck => self.check_firewall(),
            Event::Phase(_) => unreachable!(),
        }
    }

    async fn phase(&mut self, epoch: u64, phase: Phase) {
        self.backoff = phase.backoff();
        if epoch != self.epoch {
            return;
        }
        let Some(plan) = self.plan.as_ref() else {
            return;
        };
        let remote = plan.remote.clone();
        let local = plan.local_bind;
        if !matches!(phase, Phase::LinearWait) {
            self.reset_probes();
        }
        let next = match phase {
            Phase::Local { count, interval_ms } => {
                self.make_sockets(local, remote.last().addr(), count).await;
                (interval_ms, phase)
            }
            Phase::Random {
                count,
                all_ips,
                interval_ms,
                ..
            } => {
                let ips = if all_ips {
                    remote.ips()
                } else {
                    vec![remote.last().addr().ip()]
                };
                let mut ports = BTreeSet::new();
                while ports.len() < count {
                    ports.insert((rand::random::<u32>() >> 16) as u16);
                }
                let mut targets = BTreeSet::new();
                for port in ports {
                    for ip in &ips {
                        targets.insert(SocketAddr::new(*ip, port));
                    }
                }
                self.targets = targets.into_iter().collect();
                (interval_ms, phase)
            }
            Phase::Linear => {
                let mut targets = BTreeSet::new();
                let last = remote.last().addr();
                let mut port = last.port();
                for _ in 0..50 {
                    port = port.wrapping_add(remote.step);
                    if port != 0 {
                        targets.insert(SocketAddr::new(last.ip(), port));
                    }
                }
                self.targets = targets.into_iter().collect();
                (
                    5_000,
                    Phase::Random {
                        count: 500,
                        all_ips: true,
                        interval_ms: 180_000,
                        hard: false,
                    },
                )
            }
            Phase::LinearWait => (
                5_000,
                Phase::Local {
                    count: 150,
                    interval_ms: 15_000,
                },
            ),
            Phase::BothLinear => {
                let last = remote.last().addr();
                let port = last.port().wrapping_add(remote.step.wrapping_mul(30));
                let port = if port == 0 { remote.step } else { port };
                self.make_sockets(local, SocketAddr::new(last.ip(), port), 30)
                    .await;
                (
                    5_000,
                    Phase::Random {
                        count: 100,
                        all_ips: true,
                        interval_ms: 60_000,
                        hard: true,
                    },
                )
            }
        };
        log::debug!(
            "punch phase {phase:?}: targets={} sockets={}",
            self.targets.len(),
            self.sockets.len()
        );
        self.schedule(Duration::from_millis(next.0), Event::Phase(next.1));
    }

    async fn make_sockets(&mut self, local: Option<IpAddr>, target: SocketAddr, count: usize) {
        let Some(local) = local else {
            log::warn!("punch has no usable base address for temporary sockets");
            return;
        };
        for _ in 0..count {
            match UdpSocket::bind(SocketAddr::new(local, 0)).await {
                Ok(socket) => self.sockets.push(ProbeSocket {
                    socket: Some(socket),
                    target,
                }),
                Err(error) => {
                    log::warn!("temporary UDP socket creation failed on {local}: {error}")
                }
            }
        }
    }

    fn tick(&mut self) {
        if self.pending_targets.is_empty() {
            self.pending_targets.extend(self.targets.iter().copied());
        }
        if let Some(target) = self.pending_targets.pop_front() {
            if let Some(base) = self
                .plan
                .as_ref()
                .and_then(|plan| plan.base.canonical.get_conn())
            {
                if let Err(error) = base.try_send_to(&REQUEST, target) {
                    log::trace!("base probe to {target}: {error}");
                }
            }
        }
        if self.pending_sockets.is_empty() {
            self.pending_sockets.extend(0..self.sockets.len());
        }
        if let Some(index) = self.pending_sockets.pop_front() {
            if let Some(probe) = self.sockets.get(index) {
                if let Some(socket) = &probe.socket {
                    if let Err(error) = socket.try_send_to(&REQUEST, probe.target) {
                        log::trace!("temporary probe to {}: {error}", probe.target);
                    }
                }
                // Closed native socket objects still occupy their place in
                // the round-robin sets until reset_probes clears the entire phase.
            }
        }
        self.schedule(Duration::from_millis(10), Event::Tick);
    }

    fn probe_received(&mut self, index: usize, source: SocketAddr, packet: &[u8]) {
        if packet != REQUEST && packet != RESPONSE {
            log::trace!(
                "probe socket ignored {} bytes from {source}",
                packet.len()
            );
            return;
        }
        let Some(probe) = self.sockets.get_mut(index) else {
            return;
        };
        let Some(socket) = probe.socket.take() else {
            return;
        };
        if packet == REQUEST {
            let _ = socket.try_send_to(&RESPONSE, source);
        }
        let local = socket.local_addr();
        drop(socket); // Release the OS binding BEFORE posting the promotion.
        if let (Ok(local), Some(coordinator)) = (local, self.coordinator.upgrade()) {
            coordinator.promote(local.port(), source);
        }
    }

    async fn start_firewall(&mut self) {
        self.firewall.clear();
        let Some(local) = self.plan.as_ref().and_then(|plan| plan.local_bind) else {
            self.schedule(Duration::from_secs(5), Event::FirewallCheck);
            return;
        };
        // Fixed endpoints, not DNS queries and not proxy setup.
        for remote in [
            SocketAddr::from(([119, 29, 29, 29], 53)),
            SocketAddr::from(([223, 5, 5, 5], 53)),
        ] {
            let socket = if local.is_ipv4() {
                TcpSocket::new_v4()
            } else {
                TcpSocket::new_v6()
            };
            let Ok(socket) = socket else {
                continue;
            };
            if let Err(error) = socket.bind(SocketAddr::new(local, 0)) {
                log::debug!("firewall probe bind: {error}");
                continue;
            }
            let mut connecting = Box::pin(socket.connect(remote));
            let initial = poll_fn(|cx| {
                Poll::Ready(match connecting.as_mut().poll(cx) {
                    Poll::Ready(result) => Some(result),
                    Poll::Pending => None,
                })
            })
            .await;
            match initial {
                Some(Ok(socket)) => {
                    let _ = socket.set_nodelay(true);
                    self.firewall.push(Firewall::Done {
                        reachable: true,
                        socket: Some(socket),
                    });
                }
                Some(Err(error)) => log::debug!("firewall probe could not be created: {error}"),
                None => self.firewall.push(Firewall::Connecting(connecting)),
            }
        }
        self.schedule(Duration::from_secs(5), Event::FirewallCheck);
    }

    fn firewall_event(&mut self, event: FirewallEvent) {
        match event {
            FirewallEvent::Connected(index, result) => {
                log::debug!(
                    "firewall probe completion: endpoint_index={index} connected={}",
                    result.is_ok()
                );
                let state = match result {
                    Ok(socket) => {
                        let _ = socket.set_nodelay(true);
                        Firewall::Done {
                            reachable: true,
                            socket: Some(socket),
                        }
                    }
                    Err(error) => {
                        let blocked = matches!(
                            error.kind(),
                            io::ErrorKind::ConnectionReset
                                | io::ErrorKind::ConnectionRefused
                                | io::ErrorKind::TimedOut
                        );
                        log::debug!("firewall probe ended: {error}; blocked={blocked}");
                        Firewall::Done {
                            reachable: !blocked,
                            socket: None,
                        }
                    }
                };
                self.firewall[index] = state;
            }
            FirewallEvent::Closed(index) => {
                if let Firewall::Done { socket, .. } = &mut self.firewall[index] {
                    socket.take();
                }
            }
            FirewallEvent::Data => {}
        }
    }

    fn check_firewall(&mut self) {
        let tested = !self.firewall.is_empty();
        let reachable = self.firewall.iter().any(|probe| {
            matches!(
                probe,
                Firewall::Done {
                    reachable: true,
                    ..
                }
            )
        });
        log::debug!(
            "firewall reachability result: attempts={} reachable={reachable}",
            self.firewall.len()
        );
        self.firewall.clear();
        if tested && !reachable {
            self.epoch = self.epoch.wrapping_add(1);
            // This is stats only. Do not disconnect the room.
            log::info!("punch stopped by firewall reachability result; current media transport retained");
        }
    }
}

fn poll_probe(
    probes: &[ProbeSocket],
    cx: &mut Context<'_>,
    packet: &mut [u8],
) -> Poll<io::Result<(usize, SocketAddr, usize)>> {
    for (index, probe) in probes.iter().enumerate() {
        if let Some(socket) = &probe.socket {
            let mut buffer = ReadBuf::new(packet);
            match socket.poll_recv_from(cx, &mut buffer) {
                Poll::Ready(Ok(source)) => {
                    return Poll::Ready(Ok((index, source, buffer.filled().len())))
                }
                Poll::Ready(Err(error)) => return Poll::Ready(Err(error)),
                Poll::Pending => {}
            }
        }
    }
    Poll::Pending
}

fn poll_firewall(probes: &mut [Firewall], cx: &mut Context<'_>) -> Poll<FirewallEvent> {
    for (index, probe) in probes.iter_mut().enumerate() {
        match probe {
            Firewall::Connecting(connecting) => {
                if let Poll::Ready(result) = connecting.as_mut().poll(cx) {
                    return Poll::Ready(FirewallEvent::Connected(index, result));
                }
            }
            Firewall::Done {
                socket: Some(socket),
                ..
            } => {
                let mut bytes = [0; 128];
                let mut buffer = ReadBuf::new(&mut bytes);
                match Pin::new(socket).poll_read(cx, &mut buffer) {
                    Poll::Ready(Ok(())) if buffer.filled().is_empty() => {
                        return Poll::Ready(FirewallEvent::Closed(index))
                    }
                    Poll::Ready(Ok(())) => return Poll::Ready(FirewallEvent::Data),
                    Poll::Ready(Err(_)) => return Poll::Ready(FirewallEvent::Closed(index)),
                    Poll::Pending => {}
                }
            }
            _ => {}
        }
    }
    Poll::Pending
}

fn mode(local: NatKind, remote: NatKind) -> u8 {
    use NatKind::*;
    match (local, remote) {
        (None, _) | (_, None) => 0,
        (Cone, Cone) => 1,
        (Cone, Linear) => 2,
        (Linear, Cone) => 3,
        (Cone, Random) => 4,
        (Random, Cone) => 5,
        (Linear, Linear) => 6,
        (Linear, Random) => 7,
        (Random, Linear) => 8,
        (Random, Random) => 9,
        _ => 10,
    }
}
