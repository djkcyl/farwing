//! UDP association per local source endpoint. Datagrams travel on the
//! unordered, unreliable dgram channel; overload drops single datagrams and
//! never truncates or splices them.
use super::*;
use crate::i18n::t;
use tokio::net::UdpSocket;

/// An association ends after this long without traffic.
pub(super) const IDLE: Duration = Duration::from_secs(60);

pub(super) async fn run_rule(
    rule: Rule,
    transport: Arc<Transport>,
    stop: CancellationToken,
    stats: Arc<Mutex<RuleStatus>>,
) {
    let result=async {
        rule.validate()?;
        let socket=Arc::new(UdpSocket::bind(SocketAddr::new(rule.local_addr,rule.local_port)).await.context(t().features.mapping_udp_listen_failed)?);
        lock(&stats).listening=true;
        let mut flows=HashMap::<SocketAddr,mpsc::Sender<Bytes>>::new();
        let mut jobs=JoinSet::new();
        let mut buffer=vec![0;65_536];
        let outcome=loop {
            tokio::select! {
                biased;
                _=stop.cancelled()=>break Ok::<_,anyhow::Error>(()),
                Some(result)=jobs.join_next(), if !jobs.is_empty()=>{if let Ok(addr)=result {flows.remove(&addr);}},
                packet=socket.recv_from(&mut buffer)=>{
                    let (size,source)=match packet {Ok(v)=>v,Err(e) if matches!(e.kind(),std::io::ErrorKind::ConnectionReset|std::io::ErrorKind::ConnectionRefused)=>continue,Err(e)=>break Err(e.into())};
                    if size>wire::DATAGRAM_LIMIT {continue;}
                    if !flows.contains_key(&source) {
                        if flows.len()>=64 {continue;}
                        let (tx,rx)=mpsc::channel(64);
                        flows.insert(source,tx);
                        let transport=transport.clone();let rule=rule.clone();let socket=socket.clone();
                        let stop=stop.clone();let stats=stats.clone();
                        jobs.spawn(async move {association(rule,transport,socket,source,rx,stop,stats).await;source});
                    }
                    let _=flows[&source].try_send(Bytes::copy_from_slice(&buffer[..size]));
                }
            }
        };
        stop.cancel();
        drop(socket);
        flows.clear();
        while jobs.join_next().await.is_some() {}
        outcome
    }.await;
    let mut state = lock(&stats);
    state.listening = false;
    if let Err(error) = result {
        state.error = Some(error.to_string());
    }
}

async fn association(
    rule: Rule,
    transport: Arc<Transport>,
    socket: Arc<UdpSocket>,
    source: SocketAddr,
    mut packets: mpsc::Receiver<Bytes>,
    parent: CancellationToken,
    stats: Arc<Mutex<RuleStatus>>,
) {
    let stop = parent.child_token();
    lock(&stats).connections += 1;
    let mut opened = None;
    let result = tokio::select! {
        biased;
        _=stop.cancelled()=>Ok(()),
        result=async {
            let (stream, mut inbound) = transport
                .open(wire::proto::UDP, &rule.target, rule.remote_port, stop.clone())
                .await?;
            opened = Some(stream.id());
            lock(&stats).error=None;
            loop {
                tokio::select! {
                    _=tokio::time::sleep(IDLE)=>return Ok(()),
                    packet=packets.recv()=>{
                        let packet=packet.context(t().features.mapping_udp_listener_closed)?;
                        let size=packet.len();
                        if transport.send_datagram(stream.id(),packet).await? {
                            lock(&stats).sent+=size as u64;
                        }
                    },
                    message=inbound.recv()=>{
                        match message.context(t().features.mapping_udp_channel_closed)? {
                            Inbound::Datagram(data) => {
                                Transport::datagram_taken(&stream);
                                socket.send_to(&data,source).await?;
                                lock(&stats).received+=data.len() as u64;
                            }
                            Inbound::Reset(reset) if reset.code == wire::code::CLOSED => return Ok(()),
                            Inbound::Reset(reset) => anyhow::bail!(wire::describe(reset.code)),
                            _ => anyhow::bail!("{}", t().features.mapping_udp_invalid_message),
                        }
                    }
                }
            }
        }=>result
    };
    if let Some(id) = opened {
        transport.reset(id, wire::code::CLOSED).await;
    }
    let mut state = lock(&stats);
    state.connections = state.connections.saturating_sub(1);
    if !parent.is_cancelled() {
        if let Err(error) = result {
            state.error = Some(format!("{error:#}"));
        }
    }
}
